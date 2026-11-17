//! 自签证书：第一次启动时自己生成，用户不必再去找 openssl。
//!
//! # 为什么自签是可以的
//!
//! 控制面只跑在局域网/公网的一个固定地址上，没有「浏览器里点开一个网站」的场景。
//! 客户端第一次连的时候记住证书指纹（TOFU，见 `store::client::trust`），
//! 之后任何一次指纹变化都会被拒绝 —— 这与 SSH 的 `known_hosts` 是同一个模型。
//! 有正规证书（Let's Encrypt）时用 `--cert` / `--key` 指过去即可。
//!
//! # SAN 必须写全
//!
//! rustls **只看 SAN、不看 CN**。所以生成的证书要把用户可能连的每个名字都写进 SAN：
//! `localhost`、`127.0.0.1`、`::1`，以及本机的局域网 IP。少一个，客户端就会报
//! `NotValidForName` —— 而那句报错看起来像是「证书不对」，会让人查错方向。

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::Path;

/// 证书信息。
#[derive(Debug, Clone)]
pub struct CertInfo {
    /// 证书 DER 的 SHA-256（客户端 TOFU 比对的就是它）。
    pub fingerprint: [u8; 32],
    /// 是否是这次刚生成的。
    pub generated: bool,
    /// 写进 SAN 的名字，打印给用户看。
    pub names: Vec<String>,
}

/// SHA-256。
#[must_use]
pub fn fingerprint(der: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(der).into()
}

/// 指纹写成人能核对的样子。
#[must_use]
pub fn format_fingerprint(fingerprint: &[u8; 32]) -> String {
    fingerprint
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// 猜一个「别人能连到我」的 IPv4 地址。
///
/// 办法是往一个公网地址 `connect` 一个 UDP socket 再看 `local_addr` —— UDP 的 connect
/// 只在内核里选路，**不发任何包**，所以拿不到网络也不会有副作用。拿不到就返回 `None`。
#[must_use]
pub fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(SocketAddr::from(([8, 8, 8, 8], 80))).ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}

/// 确保证书存在：没有就生成一张。返回指纹与是否是新建的。
pub fn ensure(cert_path: &Path, key_path: &Path) -> Result<CertInfo, String> {
    if cert_path.exists() && key_path.exists() {
        return read(cert_path);
    }
    if cert_path.exists() != key_path.exists() {
        return Err(format!(
            "{} 与 {} 只有一个存在 —— 请删掉留下的那个让它重新生成，或者两个都指对",
            cert_path.display(),
            key_path.display()
        ));
    }
    generate(cert_path, key_path)
}

/// 读一张已有证书的指纹。
pub fn read(cert_path: &Path) -> Result<CertInfo, String> {
    let der = std::fs::read(cert_path)
        .map_err(|e| format!("读证书 {} 失败：{e}", cert_path.display()))?;
    // PEM 文件也能直接算指纹（哈希的是文件内容而不是 DER），但客户端算的是 DER，
    // 两边必须同一个口径 —— 所以这里解出 DER 再算。
    let der = if der.starts_with(b"-----BEGIN") {
        decode_pem(&der)?
    } else {
        der
    };
    Ok(CertInfo {
        fingerprint: fingerprint(&der),
        generated: false,
        names: Vec::new(),
    })
}

fn decode_pem(pem: &[u8]) -> Result<Vec<u8>, String> {
    use tokio_rustls::rustls::pki_types::CertificateDer;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    let mut items = CertificateDer::pem_slice_iter(pem);
    let Some(item) = items.next() else {
        return Err("证书文件里没有 CERTIFICATE 块".to_owned());
    };
    let cert = item.map_err(|e| format!("解析证书失败：{e}"))?;
    Ok(cert.as_ref().to_vec())
}

/// 生成一张新的自签证书，写两个 PEM 文件。
pub fn generate(cert_path: &Path, key_path: &Path) -> Result<CertInfo, String> {
    let mut names = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    if let Some(ip) = local_ip() {
        let text = ip.to_string();
        if !names.contains(&text) {
            names.push(text);
        }
    }

    let mut params = rcgen::CertificateParams::new(names.clone())
        .map_err(|e| format!("构造证书参数失败：{e}"))?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        names
            .first()
            .cloned()
            .unwrap_or_else(|| "localhost".to_owned()),
    );
    // 10 年：这是一张给内网用的自签证书，短有效期只会让用户半年后突然连不上。
    params.not_after = rcgen::date_time_ymd(2035, 1, 1);

    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("生成私钥失败：{e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("签名失败：{e}"))?;

    if let Some(parent) = cert_path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("建目录 {} 失败：{e}", parent.display()))?;
    }
    std::fs::write(cert_path, cert.pem())
        .map_err(|e| format!("写证书 {} 失败：{e}", cert_path.display()))?;
    std::fs::write(key_path, key_pair.serialize_pem())
        .map_err(|e| format!("写私钥 {} 失败：{e}", key_path.display()))?;
    // 私钥文件不该被同机的其他用户读到（Windows 上这一步是 no-op，不算失败）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(CertInfo {
        fingerprint: fingerprint(cert.der()),
        generated: true,
        names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hushsquad-cert-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn generating_then_reusing_a_certificate() {
        let dir = temp_dir("gen");
        let cert = dir.join("c.pem");
        let key = dir.join("k.pem");

        let first = ensure(&cert, &key).expect("生成");
        assert!(first.generated);
        assert!(cert.exists() && key.exists());
        assert!(
            first.names.iter().any(|name| name == "localhost"),
            "SAN 里必须有 localhost"
        );

        // 第二次必须复用，且指纹一致（指纹变了等于所有客户端都要重新信任）。
        let second = ensure(&cert, &key).expect("复用");
        assert!(!second.generated);
        assert_eq!(first.fingerprint, second.fingerprint);

        // 生成出来的东西必须真的能被 TLS 层加载（否则要到连接时才发现）。
        let acceptor = crate::control::load_tls(&cert, &key);
        assert!(acceptor.is_ok(), "生成的证书加载失败：{:?}", acceptor.err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_half_deleted_pair_is_refused_with_a_useful_message() {
        let dir = temp_dir("half");
        let cert = dir.join("c.pem");
        let key = dir.join("k.pem");
        std::fs::write(&cert, b"x").unwrap();
        let err = ensure(&cert, &key).unwrap_err();
        assert!(err.contains("只有一个存在"), "报错要能指导用户：{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fingerprints_print_as_colons() {
        let text = format_fingerprint(&[0xAB; 32]);
        assert_eq!(text.split(':').count(), 32);
        assert!(text.starts_with("AB:AB:"));
    }
}
