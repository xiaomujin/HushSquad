//! TLS 1.3 控制面：一条 TCP 连接一个会话。
//!
//! 每条连接两个方向：读（客户端消息 → 状态机）与写（状态机 → 客户端）。
//! 写有两个来源：本连接的应答，以及**别的连接**推过来的广播（有人进频道了之类），
//! 后者走 {@Shared::outbox@} 里的无界通道 —— 所以这里用 `tokio::select!` 同时等两边。

use std::path::Path;
use std::sync::Arc;

use protocol::{Framer, encode_frame};
use server_core::{Action, SessionId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::{Shared, now_ms};

/// ALPN 协议名（协议草案 §0）。
pub const ALPN: &[u8] = b"hushsquad/1";

/// 读 PEM 并建 TLS 配置。
pub fn load_tls(cert: &Path, key: &Path) -> Result<TlsAcceptor, String> {
    // 直接用 rustls-pki-types 自带的 PEM 解析（`PemObject`）。
    // 不再引 `rustls-pemfile`：它 2025-08 起归档，RUSTSEC-2025-0134 标记为不再维护，
    // 而且它现在本来就是这层代码的薄包装。
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("读证书 {} 失败：{e}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("解析证书 {} 失败：{e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{} 里没有证书", cert.display()));
    }
    let private_key = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| format!("读私钥 {} 失败：{e}", key.display()))?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, private_key)
        .map_err(|e| format!("TLS 配置失败：{e}"))?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// 绑定控制面监听地址，返回 (listener, 实际地址)。
pub async fn bind(addr: &str) -> Result<(TcpListener, std::net::SocketAddr), String> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("控制面监听 {addr} 失败：{e}"))?;
    let local = listener
        .local_addr()
        .map_err(|e| format!("取控制面本地地址失败：{e}"))?;
    Ok((listener, local))
}

/// 起控制面监听（main 用的便捷版本）。
pub async fn spawn(
    shared: Arc<Shared>,
    acceptor: TlsAcceptor,
    bind: String,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let (listener, _) = self::bind(&bind).await?;
    Ok(serve(listener, shared, acceptor))
}

/// 在已绑定的 listener 上跑 accept 循环。
pub fn serve(
    listener: TcpListener,
    shared: Arc<Shared>,
    acceptor: TlsAcceptor,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                continue;
            };
            let shared = Arc::clone(&shared);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Err(message) = handle(shared, acceptor, stream).await {
                    eprintln!("控制面连接 {peer} 结束：{message}");
                }
            });
        }
    })
}

async fn handle(
    shared: Arc<Shared>,
    acceptor: TlsAcceptor,
    stream: TcpStream,
) -> Result<(), String> {
    stream.set_nodelay(true).ok();
    let tls = acceptor
        .accept(stream)
        .await
        .map_err(|e| format!("TLS 握手失败：{e}"))?;

    // ---- 分配身份：voice_token 必须是 16 字节随机量（它是未认证源地址唯一的凭证）----
    let mut voice_token = [0u8; protocol::VOICE_TOKEN_LEN];
    getrandom::fill(&mut voice_token).map_err(|e| format!("取随机数失败：{e}"))?;
    let session_id = {
        let mut server = shared.server.lock().expect("锁没中毒");
        server
            .open_session("", voice_token)
            .ok_or("服务器已满（max_sessions）")?
    };
    shared
        .connections
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // ---- 导出语音密钥：上下文是 session_id || voice_token，两边都能独立算出 ----
    let context = transport::exporter_context(session_id, &voice_token);
    let mut exporter = [0u8; transport::EXPORTER_LEN];
    exporter = tls
        .get_ref()
        .1
        .export_keying_material(exporter, transport::EXPORTER_LABEL, Some(&context))
        .map_err(|e| format!("TLS 导出器失败：{e}"))?;
    {
        let mut server = shared.server.lock().expect("锁没中毒");
        server.install_keys(session_id, &exporter);
    }

    let (mut reader, mut writer) = tokio::io::split(tls);
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    shared
        .outbox
        .lock()
        .expect("锁没中毒")
        .insert(session_id, tx);

    let mut framer = Framer::new();
    let mut buf = vec![0u8; 8 * 1024];
    let result = loop {
        tokio::select! {
            read = reader.read(&mut buf) => {
                let n = match read {
                    Ok(0) => break Ok(()),
                    Ok(n) => n,
                    Err(e) => break Err(format!("读失败：{e}")),
                };
                if let Err(e) = framer.push(&buf[..n]) {
                    break Err(format!("分帧失败：{e}"));
                }
                loop {
                    let frame = match framer.poll_frame() {
                        Ok(Some(msg)) => msg,
                        Ok(None) => break,
                        Err(_) => break,
                    };
                    let actions = {
                        let mut server = shared.server.lock().expect("锁没中毒");
                        server.handle_control(session_id, frame, now_ms())
                    };
                    match dispatch(&shared, session_id, actions, &mut writer).await {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(e) => return Err(e),
                    }
                }
            }
            Some(bytes) = rx.recv() => {
                if let Err(e) = writer.write_all(&bytes).await {
                    break Err(format!("写失败：{e}"));
                }
            }
        }
    };

    shared.outbox.lock().expect("锁没中毒").remove(&session_id);
    {
        let mut server = shared.server.lock().expect("锁没中毒");
        server.close_session(session_id);
    }
    result
}

/// 执行一批动作。返回 `false` 表示这个会话要被关掉。
async fn dispatch<W>(
    shared: &Arc<Shared>,
    me: SessionId,
    actions: Vec<Action>,
    writer: &mut W,
) -> Result<bool, String>
where
    W: AsyncWriteExt + Unpin,
{
    let mut keep = true;
    for action in actions {
        match action {
            Action::Send { to, msg } => {
                let mut frame = Vec::new();
                encode_frame(&msg, &mut frame).map_err(|e| format!("编码失败：{e}"))?;
                if to == me {
                    writer
                        .write_all(&frame)
                        .await
                        .map_err(|e| format!("写失败：{e}"))?;
                } else {
                    // 先把发送端克隆出来，别在持有锁的时候 await。
                    let sender = shared.outbox.lock().expect("锁没中毒").get(&to).cloned();
                    if let Some(sender) = sender {
                        let _ = sender.send(frame);
                    }
                }
            }
            Action::Close {
                session, message, ..
            } => {
                if session == me {
                    keep = false;
                }
                // 只记事件，不记内容。
                eprintln!("会话 {session} 关闭：{message}");
            }
        }
    }
    Ok(keep)
}
