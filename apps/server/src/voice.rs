//! UDP 语音面：一个 socket、一个循环、一个 {@DatagramSink@}。
//!
//! 这里**没有任何判定逻辑** —— 校验流水线全在 `server-core` 里，那是可以脱机测的纯函数。
//! 这一层只负责把报文搬进搬出，以及把「写不出去」这件事安静地丢掉（语音面不重传）。

use std::net::SocketAddr;
use std::sync::Arc;

use server_core::DatagramSink;
use tokio::net::UdpSocket;

use crate::{Shared, now_ms};

/// 绑定语音面 socket，返回 (socket, 实际地址)。
///
/// 分成 bind / serve 两步是因为 `ServerHello` 里的 `voice_port` 必须先知道实际端口
/// （测试与「端口填 0 让系统分配」都需要这个顺序）。
pub async fn bind(addr: &str) -> Result<(Arc<UdpSocket>, std::net::SocketAddr), String> {
    let socket = UdpSocket::bind(addr)
        .await
        .map_err(|e| format!("语音面监听 {addr} 失败：{e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("取语音面本地地址失败：{e}"))?;
    Ok((Arc::new(socket), local))
}

/// 起语音面监听（main 用的便捷版本）。
pub async fn spawn(
    shared: Arc<Shared>,
    bind: String,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let (socket, _) = self::bind(&bind).await?;
    Ok(serve(socket, shared))
}

/// 在已绑定的 socket 上跑收包循环。
pub fn serve(socket: Arc<UdpSocket>, shared: Arc<Shared>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut packet = vec![0u8; 2048];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                continue;
            };
            // 原地解密需要一个可写缓冲；拷一次比锁着 socket 做解密更省事。
            packet[..n].copy_from_slice(&buf[..n]);
            let mut sink = UdpSink {
                socket: Arc::clone(&socket),
            };
            let mut server = shared.server.lock().expect("锁没中毒");
            server.handle_datagram(from, &mut packet[..n], now_ms(), &mut sink);
        }
    })
}

/// 把 {@DatagramSink@} 接到 UDP socket 上。
///
/// 用 `try_send_to`（非阻塞）：语音面没有重传，写不出去就丢 —— 排队只会把延迟推高。
/// 真实的丢包率会体现在对端的抖动缓冲里，不需要在这里补。
struct UdpSink {
    socket: Arc<UdpSocket>,
}

impl DatagramSink for UdpSink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let _ = self.socket.try_send_to(bytes, to);
    }
}
