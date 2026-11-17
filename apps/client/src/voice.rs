//! 语音面 socket。
//!
//! 只有一个决定值得写下来：用 `connect` 而不是每次 `send_to`。
//! UDP `connect` 会让内核**只把来自该地址的报文交上来**，于是：
//!
//! - 每包少一次地址比较（省得微乎其微，但代码少一段分支）；
//! - 无关来源的报文在进到我们的解密路径之前就被丢了；
//! - 「端口不可达」这类 ICMP 错误能作为 `recv` 的错误返回，而不是被静默吞掉。

use std::net::SocketAddr;

use tokio::net::UdpSocket;

/// 绑本地端口并连上服务器的语音面。
pub async fn bind(local: &str, server: SocketAddr) -> Result<UdpSocket, String> {
    let socket = UdpSocket::bind(local)
        .await
        .map_err(|e| format!("绑定本地语音端口 {local} 失败：{e}"))?;
    socket
        .connect(server)
        .await
        .map_err(|e| format!("语音面 connect {server} 失败：{e}"))?;
    Ok(socket)
}
