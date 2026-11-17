//! 控制面：TLS 1.3 连接、证书校验、语音密钥导出。
//!
//! 只做三件事：把 `TcpStream` 变成 `TlsStream`、按 `--ca` / `--insecure` 建信任链、
//! 从 TLS 导出器里取那 64 字节。握手状态机在 `client-core`，不在这里。
//!
//! # 证书为什么必须显式给
//!
//! 服务器用的是自签证书（部署文档里那条 `openssl req -x509`），v1 没有公共 CA 的路子。
//! 所以默认**不**接受任何证书：要么 `--ca` 给出要信任的那张，要么 `--insecure` 明确关掉校验。
//! 「默认信任一切」是这类客户端最常见的错，后果是任何人都能中间人。

use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};

/// ALPN 协议名（协议草案 §0），必须与服务器一致。
pub const ALPN: &[u8] = b"hushsquad/1";

/// 信任链配置。
#[derive(Debug, Clone)]
pub struct Trust {
    /// 要信任的证书（PEM）。
    pub ca: Option<std::path::PathBuf>,
    /// 跳过证书校验。
    pub insecure: bool,
}

/// 建客户端 TLS 配置。
pub fn client_config(trust: &Trust) -> Result<Arc<rustls::ClientConfig>, String> {
    let builder = rustls::ClientConfig::builder();
    let mut config = if trust.insecure {
        let provider = rustls::crypto::CryptoProvider::get_default()
            .ok_or("rustls 没有安装默认加密后端")?
            .clone();
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
            .with_no_client_auth()
    } else {
        let path = trust
            .ca
            .as_ref()
            .ok_or("既没有给 --ca 也没有 --insecure：默认不信任任何服务器证书")?;
        let mut roots = rustls::RootCertStore::empty();
        let certs = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("读证书 {} 失败：{e}", path.display()))?;
        let mut added = 0_usize;
        for cert in certs {
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
        .map_err(|e| format!("连控制面 {addr} 失败：{e}"))?;
    // 控制面全是小消息（心跳、成员变化），Nagle 只会白加 40 ms。
    tcp.set_nodelay(true).ok();
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|e| format!("证书名 {server_name:?} 不合法：{e}"))?;
    TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS 握手失败：{e}"))
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
    // 写成 `export_keying_material(out, ...)?; Ok(out)` 会拿回 64 个零 ——
    // 编译能过、类型也对，直到 BindAck 解不开才发现（这里踩过一次）。
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

/// `--insecure` 用的校验器：只做签名自洽性检查，不看证书链与名字。
///
/// 仍然校验握手的签名（否则连「对面确实是那个证书的持有者」都不成立），
/// 只是不追问「这张证书该不该被信」。所以它挡不住中间人——文档与告警都按这个说。
#[derive(Debug)]
struct AcceptAnyServerCert {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusing_to_trust_anything_by_default() {
        let err = client_config(&Trust {
            ca: None,
            insecure: false,
        })
        .unwrap_err();
        assert!(err.contains("--ca"), "报错必须告诉用户怎么修：{err}");
    }

    #[test]
    fn alpn_is_the_one_in_the_spec() {
        assert_eq!(ALPN, b"hushsquad/1");
    }
}
