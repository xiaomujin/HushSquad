//! 命令行参数。

use std::path::PathBuf;
use std::time::Duration;

/// 用法说明。
pub const USAGE: &str = "\
HushSquad 服务器\n\
\n\
用法：\n\
  hushsquad-server --users <文件> --cert <pem> --key <pem> [选项]\n\
\n\
必填：\n\
  --users <文件>        用户表，每行 `用户名:argon2id PHC 串:权限位`（# 开头为注释）\n\
  --cert <pem>          TLS 证书链\n\
  --key <pem>           TLS 私钥\n\
\n\
选项：\n\
  --control-bind <地址>  控制面 TCP 监听地址（默认 0.0.0.0:64738）\n\
  --voice-bind <地址>    语音面 UDP 监听地址（默认 0.0.0.0:64739）\n\
  --voice-host <主机>    写进 ServerHello 的对外地址（默认 127.0.0.1）\n\
  --voice-port <端口>    写进 ServerHello 的对外端口（默认取 --voice-bind 的端口）\n\
  --channel <名字>       预建频道，可重复\n\
  --metrics-interval <秒> 指标打印间隔（默认 60）\n\
\n\
辅助：\n\
  --hash-password <密码> 打印一条用户记录里要填的 argon2id PHC 串\n\
\n\
生成一个自签证书（一行）：\n\
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \\\n    -keyout key.pem -out cert.pem -days 365 -subj /CN=localhost \\\n    -addext \"subjectAltName=DNS:localhost,IP:127.0.0.1\"\n\
\n\
必须带 -addext（SAN）：客户端用 rustls 校验证书，它**只认 SAN、不认 CN**，\n\
没有 SAN 的证书即使 --ca 给了也会被拒。要连别的地址就把那个 IP 也写进 SAN。\n";

/// 配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 用户表路径。
    pub users: PathBuf,
    /// 证书路径。
    pub cert: PathBuf,
    /// 私钥路径。
    pub key: PathBuf,
    /// 控制面监听地址。
    pub control_bind: String,
    /// 语音面监听地址。
    pub voice_bind: String,
    /// 对外宣告的语音主机。
    pub voice_host: String,
    /// 对外宣告的语音端口。
    pub voice_port: u16,
    /// 预建频道。
    pub channels: Vec<String>,
    /// 指标间隔。
    pub metrics_interval: Duration,
}

impl Config {
    /// 解析参数。
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut users = None;
        let mut cert = None;
        let mut key = None;
        let mut control_bind = "0.0.0.0:64738".to_owned();
        let mut voice_bind = "0.0.0.0:64739".to_owned();
        let mut voice_host = "127.0.0.1".to_owned();
        let mut voice_port: Option<u16> = None;
        let mut channels = Vec::new();
        let mut metrics_interval = 60u64;

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = || -> Result<String, String> {
                args.next().ok_or_else(|| format!("{arg} 缺少取值"))
            };
            match arg.as_str() {
                "--hash-password" => {
                    let password = value()?;
                    let hash = server_core::UserDb::hash_password(&password)
                        .map_err(|e| format!("算哈希失败：{e}"))?;
                    println!("{hash}");
                    std::process::exit(0);
                }
                "--users" => users = Some(PathBuf::from(value()?)),
                "--cert" => cert = Some(PathBuf::from(value()?)),
                "--key" => key = Some(PathBuf::from(value()?)),
                "--control-bind" => control_bind = value()?,
                "--voice-bind" => voice_bind = value()?,
                "--voice-host" => voice_host = value()?,
                "--voice-port" => {
                    voice_port = Some(
                        value()?
                            .parse()
                            .map_err(|_| "--voice-port 需要端口号".to_owned())?,
                    );
                }
                "--channel" => channels.push(value()?),
                "--metrics-interval" => {
                    metrics_interval = value()?
                        .parse()
                        .map_err(|_| "--metrics-interval 需要秒数".to_owned())?;
                }
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("未知参数 {other:?}")),
            }
        }

        let voice_port = match voice_port {
            Some(port) => port,
            None => voice_bind
                .rsplit(':')
                .next()
                .and_then(|port| port.parse().ok())
                .ok_or_else(|| {
                    "--voice-bind 里的端口解析不出来，请显式给 --voice-port".to_owned()
                })?,
        };

        Ok(Self {
            users: users.ok_or("缺少 --users")?,
            cert: cert.ok_or("缺少 --cert")?,
            key: key.ok_or("缺少 --key")?,
            control_bind,
            voice_bind,
            voice_host,
            voice_port,
            channels,
            metrics_interval: Duration::from_secs(metrics_interval.max(1)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, String> {
        Config::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn required_flags_are_required() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--users", "u"]).is_err());
    }

    #[test]
    fn voice_port_defaults_to_the_bind_port() {
        let config = parse(&[
            "--users",
            "u",
            "--cert",
            "c",
            "--key",
            "k",
            "--voice-bind",
            "0.0.0.0:50000",
        ])
        .unwrap();
        assert_eq!(config.voice_port, 50_000);
        assert_eq!(config.metrics_interval, Duration::from_secs(60));
    }

    #[test]
    fn channels_accumulate() {
        let config = parse(&[
            "--users",
            "u",
            "--cert",
            "c",
            "--key",
            "k",
            "--channel",
            "大厅",
            "--channel",
            "游戏",
        ])
        .unwrap();
        assert_eq!(config.channels, vec!["大厅".to_owned(), "游戏".to_owned()]);
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert!(parse(&["--users", "u", "--cert", "c", "--key", "k", "--nope"]).is_err());
    }
}
