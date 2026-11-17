//! 控制面：TLS 1.3 连接、证书信任、语音密钥导出。
//!
//! 只做三件事：把 `TcpStream` 变成 `TlsStream`、按信任策略校验证书、从导出器里取那 64 字节。
//! 握手状态机在 `client-core`，不在这里。
//!
//! # 三种信任策略
//!
//! | 策略 | 什么时候用 | 安全性 |
//! |---|---|---|
//! | TOFU（默认） | 一般情况 | 第一次连信任并记住指纹，之后变了就拒绝 |
//! | `--ca <pem>` | 有明确要信任的证书 | 只认这一张，指纹无关 |
//! | `--insecure` | 本机调试 | 不校验，中间人可冒充 |
//!
//! TOFU 与 SSH 的 `known_hosts` 是同一个模型：**首次使用即信任，之后固定**。
//! 它换掉了「先把 cert.pem 拷到每台机器」这一步 —— 那正是用户抱怨的「操作过于复杂」。
//! 代价是第一次连接没有保护（这正是 TOFU 的定义），所以服务器启动时会把指纹打印出来，
//! 需要认真核对时可以对一眼。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use store::client::{ClientStore, fingerprint_of, format_fingerprint};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme};

/// ALPN 协议名（协议草案 §0），必须与服务器一致。
pub const ALPN: &[u8] = b"hushsquad/1";

/// 信任策略。
#[derive(Clone)]
pub enum Trust {
    /// 首次使用即信任，之后固定指纹。
    Tofu {
        /// 本地库（存指纹）。
        store: Arc<Mutex<ClientStore>>,
        /// 按哪个主机名记（含端口，一台机器多个端口就是多条记录）。
        host: String,
    },
    /// 只信这张证书。
    Ca(PathBuf),
    /// 不校验。
    Insecure,
}

impl core::fmt::Debug for Trust {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Tofu { host, .. } => write!(f, "Tofu({host})"),
            Self::Ca(path) => write!(f, "Ca({})", path.display()),
            Self::Insecure => f.write_str("Insecure"),
        }
    }
}

fn provider() -> Result<Arc<CryptoProvider>, String> {
    CryptoProvider::get_default()
        .cloned()
        .ok_or_else(|| "rustls 没有安装默认加密后端".to_owned())
}

/// 建客户端 TLS 配置。
pub fn client_config(trust: &Trust) -> Result<Arc<rustls::ClientConfig>, String> {
    let builder = rustls::ClientConfig::builder();
    let mut config = match trust {
        Trust::Insecure => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny {
                provider: provider()?,
            }))
            .with_no_client_auth(),
        Trust::Tofu { store, host } => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Tofu {
                provider: provider()?,
                store: Arc::clone(store),
                host: host.clone(),
            }))
            .with_no_client_auth(),
        Trust::Ca(path) => {
            let mut roots = rustls::RootCertStore::empty();
            let mut added = 0_usize;
            for cert in CertificateDer::pem_file_iter(path)
                .map_err(|e| format!("读证书 {} 失败：{e}", path.display()))?
            {
                let cert = cert.map_err(|e| format!("解析证书 {} 失败：{e}", path.display()))?;
                roots
                    .add(cert)
                    .map_err(|e| format!("证书 {} 不可用：{e}", path.display()))?;
                added += 1;
            }
            if added == 0 {
                return Err(format!("{} 里没有证书", path.display()));
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

/// 连上控制面并完成 TLS 握手。
pub async fn connect(
    addr: &str,
    trust: &Trust,
    server_name: &str,
) -> Result<TlsStream<TcpStream>, String> {
    let config = client_config(trust)?;
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("连不上 {addr}：{e}"))?;
    // 控制面全是小消息（心跳、成员变化），Nagle 只会白加 40 ms。
    tcp.set_nodelay(true).ok();
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|e| format!("证书名 {server_name:?} 不合法：{e}"))?;
    TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .map_err(|e| explain_handshake(e, trust))
}

/// 把 rustls 的握手错误翻成「该怎么办」。
fn explain_handshake(error: std::io::Error, trust: &Trust) -> String {
    let text = error.to_string();
    if text.contains("证书指纹变了") {
        return format!(
            "{text}\n\
             服务器换了证书？确认是你要的那台之后，用 --reset-trust 重连一次。"
        );
    }
    if matches!(trust, Trust::Tofu { .. }) {
        format!(
            "TLS 握手失败：{text}\n\
             如果服务器用的是自签证书，第一次连失败通常是因为地址与证书里的名字不一致：\
             用 --server-name 指定证书上的名字（服务器启动时会打印可用名字）。"
        )
    } else {
        format!("TLS 握手失败：{text}")
    }
}

/// 从 TLS 导出器取 64 字节，供 HKDF 派生语音密钥。
///
/// 上下文是 `session_id || voice_token`（协议草案 §3）：两端都能独立算出，
/// 所以不需要在控制面里传密钥，密钥也就不会出现在日志里。
pub fn exporter(
    tls: &TlsStream<TcpStream>,
    session_id: u64,
    voice_token: &[u8; 16],
) -> Result<[u8; transport::EXPORTER_LEN], String> {
    let context = transport::exporter_context(session_id, voice_token);
    // 注意：`export_keying_material` 是**按值**吃缓冲区、再把它填好的副本返回的。
    // 写成 `...?; Ok(buf)` 会拿回 64 个零 —— 编译能过、类型也对，直到 BindAck 解不开才发现。
    let out = tls
        .get_ref()
        .1
        .export_keying_material(
            [0u8; transport::EXPORTER_LEN],
            transport::EXPORTER_LABEL,
            Some(&context),
        )
        .map_err(|e| format!("TLS 导出器失败：{e}"))?;
    Ok(out)
}

/// TOFU 校验器：第一次记住指纹，之后必须一致。
///
/// 它**不**验证书链（自签证书本来就没有链），但**会**验证握手的签名确实由这张证书的
/// 私钥签出 —— 那一步由 `verify_tls13_signature` 委托给 rustls 完成，所以「对面持有私钥」
/// 这件事仍然成立。TOFU 补的是另一半：**这张证书是不是我上次见的那个**。
#[derive(Debug)]
struct Tofu {
    provider: Arc<CryptoProvider>,
    store: Arc<Mutex<ClientStore>>,
    host: String,
}

impl ServerCertVerifier for Tofu {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let fingerprint = fingerprint_of(end_entity.as_ref());
        let store = self
            .store
            .lock()
            .map_err(|_| Error::General("本地库的锁中毒了".to_owned()))?;
        match store
            .fingerprint(&self.host)
            .map_err(|e| Error::General(e.to_string()))?
        {
            Some(known) if known == fingerprint => Ok(ServerCertVerified::assertion()),
            Some(known) => {
                // 用 `General` 而不是 `InvalidCertificate`：前者的文本会原样出现在
                // 我们的错误信息里，用户才看得到「上次是 X，这次是 Y」这两串指纹。
                Err(Error::General(format!(
                    "证书指纹变了：上次是 {}，这次是 {}。",
                    format_fingerprint(&known),
                    format_fingerprint(&fingerprint)
                )))
            }
            None => {
                // 第一次见：记住。这是 TOFU 唯一「不做判断」的时刻。
                store
                    .trust(&self.host, &fingerprint)
                    .map_err(|e| Error::General(e.to_string()))?;
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `--insecure` 用的校验器：只做签名自洽性检查，不看证书链与名字。
#[derive(Debug)]
struct AcceptAny {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_is_the_one_in_the_spec() {
        assert_eq!(ALPN, b"hushsquad/1");
    }

    #[test]
    fn every_trust_mode_builds_a_config() {
        let store = Arc::new(Mutex::new(ClientStore::open_memory().expect("内存库")));
        assert!(
            client_config(&Trust::Tofu {
                store: Arc::clone(&store),
                host: "a:1".to_owned()
            })
            .is_ok()
        );
        assert!(client_config(&Trust::Insecure).is_ok());
    }

    #[test]
    fn a_missing_ca_file_is_reported_with_the_path() {
        let err = client_config(&Trust::Ca(PathBuf::from("definitely-not-here.pem"))).unwrap_err();
        assert!(
            err.contains("definitely-not-here.pem"),
            "报错要带路径：{err}"
        );
    }
}
