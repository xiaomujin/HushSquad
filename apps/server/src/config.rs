//! 命令行：一条命令就能跑，其余都是可选项。
//!
//! ```text
//! hushsquad-server                       # 全部用默认值，第一次跑会自动建库、建证书
//! hushsquad-server adduser alice         # 加账号（密码不回显，问两遍）
//! hushsquad-server ls                    # 看有哪些账号
//! hushsquad-server passwd alice          # 改密码
//! hushsquad-server del alice             # 删账号
//! hushsquad-server cert                  # 重新生成证书
//! ```
//!
//! 设计取向：**默认值必须是对的**。第一次跑不要求任何一个参数，
//! 不该让用户先去学 openssl 和 argon2 的 PHC 格式。

use std::path::PathBuf;
use std::time::Duration;

/// 一个子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 跑服务器（默认）。
    Serve,
    /// 加账号。
    AddUser(String),
    /// 改密码。
    Passwd(String),
    /// 删账号。
    DelUser(String),
    /// 列账号。
    List,
    /// 重新生成证书。
    Cert,
    /// 打印用法。
    Help,
}

/// 用法说明。
pub const USAGE: &str = r"HushSquad 服务器

用法：
  hushsquad-server [选项]                 跑服务器（全部有默认值）
  hushsquad-server adduser <用户名>       加账号（会问两遍密码）
  hushsquad-server passwd <用户名>        改密码
  hushsquad-server del <用户名>           删账号
  hushsquad-server ls                     列出账号
  hushsquad-server cert                   重新生成自签证书

选项（都有默认值，一般一个都不用给）：
  --db <文件>            账号库（默认 hushsquad.db，第一次跑自动建）
  --cert <pem>           证书（默认 hushsquad-cert.pem，不存在就自动生成）
  --key <pem>            私钥（默认 hushsquad-key.pem）
  --bind <地址>          控制面监听（默认 0.0.0.0:64738）
  --voice-bind <地址>    语音面监听（默认 0.0.0.0:64739）
  --voice-host <主机>    对外宣告的语音地址（默认自动取本机局域网 IP）
  --voice-port <端口>    对外宣告的语音端口（默认同 --voice-bind）
  --channel <名字>       预建频道（可重复；默认建一个「大厅」）
  --no-anonymous         不允许「只用设备号」的匿名登录
  --max-anonymous <数量> 匿名账号上限（默认 200）
  --metrics-interval <秒> 指标打印间隔（默认 60，0 表示不打印）
  --help                 打印这段说明

典型用法：
  1. 服务器上直接跑 `hushsquad-server`（会自动建库与证书，打印出证书指纹）
  2. `hushsquad-server adduser 你的名字` 建一个账号
  3. 客户端 `hushsquad-client --server <服务器IP> --user 你的名字`，按提示输密码

客户端第一次连会记住证书指纹（TOFU），所以不用把证书拷过去。
";

/// 配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 子命令。
    pub command: Command,
    /// 账号库路径。
    pub db: PathBuf,
    /// 证书路径。
    pub cert: PathBuf,
    /// 私钥路径。
    pub key: PathBuf,
    /// 控制面监听地址。
    pub control_bind: String,
    /// 语音面监听地址。
    pub voice_bind: String,
    /// 对外宣告的语音主机；`None` = 自动探测本机局域网地址。
    pub voice_host: Option<String>,
    /// 对外宣告的语音端口；`None` = 取 `voice_bind` 的端口。
    pub voice_port: Option<u16>,
    /// 预建频道。
    pub channels: Vec<String>,
    /// 是否允许设备号匿名登录。
    pub allow_anonymous: bool,
    /// 匿名账号上限。
    pub max_anonymous: u32,
    /// 指标间隔；`None` 表示不打印。
    pub metrics_interval: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            command: Command::Serve,
            db: PathBuf::from("hushsquad.db"),
            cert: PathBuf::from("hushsquad-cert.pem"),
            key: PathBuf::from("hushsquad-key.pem"),
            control_bind: "0.0.0.0:64738".to_owned(),
            voice_bind: "0.0.0.0:64739".to_owned(),
            voice_host: None,
            voice_port: None,
            channels: vec!["大厅".to_owned()],
            allow_anonymous: true,
            max_anonymous: 200,
            metrics_interval: Some(Duration::from_secs(60)),
        }
    }
}

impl Config {
    /// 解析参数。
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut config = Self::default();
        // 用户显式给了 --channel 就不再塞默认的「大厅」。
        let mut channels_given = false;

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = || -> Result<String, String> {
                args.next().ok_or_else(|| format!("{arg} 缺少取值"))
            };
            match arg.as_str() {
                "--db" => config.db = PathBuf::from(value()?),
                "--cert" => config.cert = PathBuf::from(value()?),
                "--key" => config.key = PathBuf::from(value()?),
                "--bind" => config.control_bind = value()?,
                "--voice-bind" => config.voice_bind = value()?,
                "--voice-host" => config.voice_host = Some(value()?),
                "--voice-port" => {
                    config.voice_port = Some(
                        value()?
                            .parse()
                            .map_err(|_| "--voice-port 需要端口号".to_owned())?,
                    );
                }
                "--channel" => {
                    if !channels_given {
                        config.channels.clear();
                        channels_given = true;
                    }
                    config.channels.push(value()?);
                }
                "--no-anonymous" => config.allow_anonymous = false,
                "--max-anonymous" => {
                    config.max_anonymous = value()?
                        .parse()
                        .map_err(|_| "--max-anonymous 需要数量".to_owned())?;
                }
                "--metrics-interval" => {
                    let seconds: u64 = value()?
                        .parse()
                        .map_err(|_| "--metrics-interval 需要秒数".to_owned())?;
                    config.metrics_interval = (seconds > 0).then(|| Duration::from_secs(seconds));
                }
                "--help" | "-h" => config.command = Command::Help,
                "adduser" => config.command = Command::AddUser(value()?),
                "passwd" => config.command = Command::Passwd(value()?),
                "del" => config.command = Command::DelUser(value()?),
                "ls" => config.command = Command::List,
                "cert" => config.command = Command::Cert,
                other => return Err(format!("未知参数 {other:?}")),
            }
        }
        Ok(config)
    }

    /// 对外宣告的语音端口。
    #[must_use]
    pub fn voice_port(&self) -> u16 {
        self.voice_port.unwrap_or_else(|| {
            self.voice_bind
                .rsplit(':')
                .next()
                .and_then(|port| port.parse().ok())
                .unwrap_or(64_739)
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
    fn no_arguments_at_all_is_a_valid_server_config() {
        // 这是这次改动的核心：第一次跑不该要求任何参数。
        let config = parse(&[]).expect("裸跑必须可用");
        assert_eq!(config.command, Command::Serve);
        assert_eq!(config.db, PathBuf::from("hushsquad.db"));
        assert_eq!(config.control_bind, "0.0.0.0:64738");
        assert_eq!(config.voice_port(), 64_739);
        assert!(config.allow_anonymous);
        assert_eq!(config.channels, vec!["大厅".to_owned()]);
    }

    #[test]
    fn subcommands_are_recognised() {
        assert_eq!(
            parse(&["adduser", "alice"]).unwrap().command,
            Command::AddUser("alice".to_owned())
        );
        assert_eq!(
            parse(&["passwd", "alice"]).unwrap().command,
            Command::Passwd("alice".to_owned())
        );
        assert_eq!(
            parse(&["del", "alice"]).unwrap().command,
            Command::DelUser("alice".to_owned())
        );
        assert_eq!(parse(&["ls"]).unwrap().command, Command::List);
        assert_eq!(parse(&["cert"]).unwrap().command, Command::Cert);
        assert_eq!(parse(&["--help"]).unwrap().command, Command::Help);
        assert!(parse(&["adduser"]).is_err(), "缺用户名要报错");
    }

    #[test]
    fn giving_any_channel_drops_the_default_one() {
        let config = parse(&["--channel", "游戏", "--channel", "开会"]).unwrap();
        assert_eq!(config.channels, vec!["游戏".to_owned(), "开会".to_owned()]);
    }

    #[test]
    fn voice_port_falls_back_to_the_bind_port() {
        let config = parse(&["--voice-bind", "0.0.0.0:50000"]).unwrap();
        assert_eq!(config.voice_port(), 50_000);
        let config = parse(&["--voice-bind", "0.0.0.0:50000", "--voice-port", "6000"]).unwrap();
        assert_eq!(config.voice_port(), 6_000);
    }

    #[test]
    fn zero_turns_metrics_off_and_anonymous_can_be_disabled() {
        let config = parse(&["--metrics-interval", "0", "--no-anonymous"]).unwrap();
        assert!(config.metrics_interval.is_none());
        assert!(!config.allow_anonymous);
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert!(parse(&["--nope"]).is_err());
    }
}
