//! 命令行参数：默认值必须是对的，常见的两种用法各自只要一个参数。
//!
//! ```text
//! hushsquad-client --server 192.168.1.10              # 匿名进（设备号登录）
//! hushsquad-client --server 192.168.1.10 --user 张三   # 账号登录，会问密码
//! ```
//!
//! 证书不用拷：第一次连会记住指纹（TOFU），之后指纹变了就拒绝。
//! 本地状态（设备号、记住的指纹）存在一个小 SQLite 库里，默认在用户数据目录。

use std::path::PathBuf;
use std::time::Duration;

/// 用法说明。
pub const USAGE: &str = r"HushSquad 客户端

用法：
  hushsquad-client --server <主机[:端口]>                 匿名进入（按设备号）
  hushsquad-client --server <主机[:端口]> --user <用户名>  账号登录（会问密码）

必填：
  --server <主机[:端口]>  服务器地址（端口默认 64738）

身份（二选一，都不给就是匿名）：
  --user <用户名>         账号；密码会提示输入，也可放环境变量 HUSHSQUAD_PASSWORD
  --anonymous             显式要求匿名（按设备号建号，不需要密码）

证书：
  （默认）                第一次连记住服务器证书指纹，之后指纹变了就拒绝
  --ca <pem>              只信这一张证书
  --insecure              完全不校验（只用于本机调试，中间人可冒充服务器）
  --server-name <名字>    证书里该匹配的名字（默认取 --server 的主机部分）
  --reset-trust           忘掉这台服务器记住的指纹（服务器换了证书时用）

设备号：
  --device-id <十六进制>  指定设备号（32 个十六进制字符，可带 - 分隔）
  --show-device-id        打印本机设备号后退出（匿名身份就是它）

本地状态：
  --db <文件>             设备号与信任记录的库（默认在用户数据目录）

频道与音频：
  --channel <名字或 id>   进哪个频道（默认第一个频道）
  --mic <设备名>          麦克风（子串匹配；默认系统默认设备）
  --speaker <设备名>      扬声器（同上）
  --no-mic                不开麦克风（只听）
  --no-speaker            不开扬声器（只发）
  --chain <档位串>        上行处理链（默认 highpass,rnn,agc2；none 表示直通）
  --dtx                   静音时不发包（省流量，但对方的抖动缓冲要能撑住）
  --tone <Hz>             用合成正弦音代替麦克风（验证链路用，不需要声卡）
  --jitter-min <毫秒>     抖动缓冲目标延迟下限（默认 20）
  --jitter-max <毫秒>     抖动缓冲目标延迟上限（默认 60）。调大更抗抖动、延迟更高
  --no-drift              关掉漂移补偿（拼接会少一些，但两端晶振差要自己扛；诊断用）
  --bind <地址>           本地 UDP 绑定地址（默认 0.0.0.0:0）

运行与观测：
  --stats-interval <秒>   指标打印间隔（默认 5，0 表示不打印）
  --probe-interval <秒>   语音面打点间隔（默认 5，0 表示不打点）。打点会占用语音序号
  --run-for <秒>          跑够这么久就退出（默认一直跑到 Ctrl-C）
  --list-devices          列出音频设备后退出
  --quiet                 只打错误与最终报告
  --help                  打印这段说明

退出时总会打印一份汇总（含语音面 RTT 的 p50/p95/p99），可直接贴进报告。
";

/// 一帧的时长（毫秒）。语音面按这个节奏收发，与 `audio-core::FRAME_MS` 一致。
pub const FRAME_MS: u64 = 10;

/// 配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 服务器地址（主机[:端口]）。
    pub server: String,
    /// 用户名；空 = 匿名。
    pub user: String,
    /// 密码（只在账号登录时用）。
    pub password: String,
    /// 显式要求匿名登录。
    pub anonymous: bool,
    /// 要进的频道：名字或 id。
    pub channel: String,
    /// 只信这张证书。
    pub ca: Option<PathBuf>,
    /// 完全不校验证书。
    pub insecure: bool,
    /// 证书里该匹配的名字。
    pub server_name: Option<String>,
    /// 忘掉这台服务器记住的指纹。
    pub reset_trust: bool,
    /// 指定设备号。
    pub device_id: Option<[u8; 16]>,
    /// 只打印设备号。
    pub show_device_id: bool,
    /// 本地状态库。
    pub db: Option<PathBuf>,
    /// 麦克风设备名。
    pub mic: Option<String>,
    /// 扬声器设备名。
    pub speaker: Option<String>,
    /// 不开麦克风。
    pub no_mic: bool,
    /// 不开扬声器。
    pub no_speaker: bool,
    /// 上行处理链。
    pub chain: String,
    /// 静音时不发包。
    pub dtx: bool,
    /// 用合成正弦音代替麦克风（Hz）。
    pub tone: Option<f32>,
    /// 抖动缓冲下限（毫秒）。
    pub jitter_min_ms: u32,
    /// 抖动缓冲上限（毫秒）。
    pub jitter_max_ms: u32,
    /// 关掉漂移补偿（诊断用：对比「声音里的加工感」是不是它造成的）。
    pub no_drift: bool,
    /// 本地 UDP 绑定地址。
    pub bind: String,
    /// 指标打印间隔。
    pub stats_interval: Option<Duration>,
    /// 打点间隔。
    pub probe_interval: Option<Duration>,
    /// 跑多久退出。
    pub run_for: Option<Duration>,
    /// 只列设备。
    pub list_devices: bool,
    /// 只打错误与最终报告。
    pub quiet: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: "127.0.0.1:64738".to_owned(),
            user: String::new(),
            password: String::new(),
            anonymous: false,
            channel: String::new(),
            ca: None,
            insecure: false,
            server_name: None,
            reset_trust: false,
            device_id: None,
            show_device_id: false,
            db: None,
            mic: None,
            speaker: None,
            no_mic: false,
            no_speaker: false,
            chain: "highpass,rnn,agc2".to_owned(),
            dtx: false,
            tone: None,
            jitter_min_ms: 20,
            jitter_max_ms: 60,
            no_drift: false,
            bind: "0.0.0.0:0".to_owned(),
            stats_interval: Some(Duration::from_secs(5)),
            // 5 秒一次：打点包会占用一个语音序号，接收端会把它算成 1 个空洞丢包。
            probe_interval: Some(Duration::from_secs(5)),
            run_for: None,
            list_devices: false,
            quiet: false,
        }
    }
}

impl Config {
    /// 解析参数。
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut config = Self::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = || -> Result<String, String> {
                args.next().ok_or_else(|| format!("{arg} 缺少取值"))
            };
            match arg.as_str() {
                "--server" => config.server = server_with_default_port(&value()?),
                "--user" => config.user = value()?,
                "--password" => config.password = value()?,
                "--anonymous" => config.anonymous = true,
                "--channel" => config.channel = value()?,
                "--ca" => config.ca = Some(PathBuf::from(value()?)),
                "--insecure" => config.insecure = true,
                "--server-name" => config.server_name = Some(value()?),
                "--reset-trust" => config.reset_trust = true,
                "--device-id" => {
                    config.device_id = Some(store::client::parse_device_id(&value()?)?)
                }
                "--show-device-id" => config.show_device_id = true,
                "--db" => config.db = Some(PathBuf::from(value()?)),
                "--mic" => config.mic = Some(value()?),
                "--speaker" => config.speaker = Some(value()?),
                "--no-mic" => config.no_mic = true,
                "--no-speaker" => config.no_speaker = true,
                "--chain" => config.chain = value()?,
                "--dtx" => config.dtx = true,
                "--tone" => {
                    config.tone = Some(
                        value()?
                            .parse()
                            .map_err(|_| "--tone 需要频率（Hz）".to_owned())?,
                    );
                }
                "--jitter-min" => {
                    config.jitter_min_ms = value()?
                        .parse()
                        .map_err(|_| "--jitter-min 需要毫秒数".to_owned())?;
                }
                "--jitter-max" => {
                    config.jitter_max_ms = value()?
                        .parse()
                        .map_err(|_| "--jitter-max 需要毫秒数".to_owned())?;
                }
                "--no-drift" => config.no_drift = true,
                "--bind" => config.bind = value()?,
                "--stats-interval" => {
                    config.stats_interval = seconds(&value()?, "--stats-interval")?
                }
                "--probe-interval" => {
                    config.probe_interval = seconds(&value()?, "--probe-interval")?
                }
                "--run-for" => config.run_for = seconds(&value()?, "--run-for")?,
                "--list-devices" => config.list_devices = true,
                "--quiet" => config.quiet = true,
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("未知参数 {other:?}")),
            }
        }
        Ok(config)
    }

    /// 密码允许只放在环境变量里；账号模式下调一次。`parse` 保持纯函数，测试才封闭。
    pub fn with_env_password(mut self) -> Self {
        if self.password.is_empty()
            && let Ok(password) = std::env::var("HUSHSQUAD_PASSWORD")
        {
            self.password = password;
        }
        self
    }

    /// 是否匿名登录：没给用户名就算匿名。
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.anonymous || self.user.is_empty()
    }

    /// 证书里该匹配的名字：显式给的优先，否则取 `--server` 的主机部分。
    #[must_use]
    pub fn server_name(&self) -> &str {
        self.server_name
            .as_deref()
            .unwrap_or_else(|| host_of(&self.server))
    }
}

/// `主机` 后面没写端口就补 64738，省得每次都要打。
#[must_use]
pub fn server_with_default_port(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "127.0.0.1:64738".to_owned();
    }
    // 已经有端口（最后一个冒号后面是数字）就原样用。
    if let Some((_, port)) = trimmed.rsplit_once(':')
        && port.parse::<u16>().is_ok()
    {
        return trimmed.to_owned();
    }
    format!("{trimmed}:64738")
}

/// 把 `主机:端口` 里的主机切出来。
#[must_use]
pub fn host_of(addr: &str) -> &str {
    addr.rsplit_once(':').map_or(addr, |(host, _)| host)
}

fn seconds(text: &str, flag: &str) -> Result<Option<Duration>, String> {
    let value: u64 = text.parse().map_err(|_| format!("{flag} 需要秒数"))?;
    Ok((value > 0).then(|| Duration::from_secs(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, String> {
        Config::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn only_a_server_is_needed_and_it_means_anonymous() {
        let config = parse(&["--server", "192.168.1.10"]).expect("解析");
        assert_eq!(config.server, "192.168.1.10:64738", "端口要自动补上");
        assert!(config.is_anonymous(), "没有 --user 就是匿名");
    }

    #[test]
    fn a_given_user_switches_to_account_login() {
        let config = parse(&["--server", "h:1", "--user", "alice"]).expect("解析");
        assert!(!config.is_anonymous());
    }

    #[test]
    fn explicit_anonymous_wins_over_a_leftover_user() {
        let config = parse(&["--user", "alice", "--anonymous"]).expect("解析");
        assert!(config.is_anonymous());
    }

    #[test]
    fn ports_are_only_added_when_missing() {
        assert_eq!(server_with_default_port("host"), "host:64738");
        assert_eq!(server_with_default_port("host:1234"), "host:1234");
        assert_eq!(server_with_default_port("127.0.0.1"), "127.0.0.1:64738");
    }

    #[test]
    fn zero_turns_a_period_off() {
        let config = parse(&["--stats-interval", "0", "--probe-interval", "0"]).unwrap();
        assert!(config.stats_interval.is_none());
        assert!(config.probe_interval.is_none());
    }

    #[test]
    fn server_name_falls_back_to_the_host_part() {
        let config = parse(&["--server", "voice.example:1234"]).unwrap();
        assert_eq!(config.server_name(), "voice.example");
        let config = parse(&["--server-name", "other"]).unwrap();
        assert_eq!(config.server_name(), "other");
    }

    #[test]
    fn a_device_id_can_be_given_on_the_command_line() {
        let config = parse(&["--device-id", "A1B2-C3D4-E5F6-7890-1122-3344-5566-7788"]).unwrap();
        assert_eq!(config.device_id.expect("解析出设备号")[0], 0xA1);
        assert!(parse(&["--device-id", "nope"]).is_err());
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert!(parse(&["--nope"]).is_err());
    }
}
