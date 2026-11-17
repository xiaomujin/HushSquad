//! 命令行参数。

use std::path::PathBuf;
use std::time::Duration;

/// 用法说明。
pub const USAGE: &str = r"HushSquad 客户端

用法：
  hushsquad-client --server <主机:端口> --user <用户名> [选项]

必填：
  --server <主机:端口>    服务器控制面地址（默认 127.0.0.1:64738）
  --user <用户名>         登录用户名

身份：
  --password <密码>       密码。也可以放在环境变量 HUSHSQUAD_PASSWORD 里，
                          避免出现在进程列表与 shell 历史中（推荐）

证书：
  --ca <pem>              信任这张证书（自签证书就用它）
  --insecure              不校验服务器证书。只用于本机/内网联调，
                          它让中间人可以冒充服务器
  --server-name <名字>    证书里该匹配的名字（默认取 --server 的主机部分）

频道与音频：
  --channel <名字或 id>   进哪个频道（默认第一个频道）
  --mic <设备名>          麦克风（子串匹配；默认系统默认设备）
  --speaker <设备名>      扬声器（同上）
  --no-mic                不开麦克风（只听）
  --no-speaker            不开扬声器（只发）
  --chain <档位串>        上行处理链（默认 highpass,agc2；none 表示直通）
  --dtx                   静音时不发包（省流量，但对方的抖动缓冲要能撑住）
  --tone <Hz>             用合成正弦音代替麦克风（验证链路用，不需要声卡）
  --jitter-min <毫秒>     抖动缓冲目标延迟下限（默认 20）
  --jitter-max <毫秒>     抖动缓冲目标延迟上限（默认 60）。调大更抗抖动、延迟更高
  --bind <地址>           本地 UDP 绑定地址（默认 0.0.0.0:0）

运行与观测：
  --stats-interval <秒>   指标打印间隔（默认 5，0 表示不打印）
  --probe-interval <秒>   语音面打点间隔（默认 5，0 表示不打点）。打点会占用语音序号
  --run-for <秒>          跑够这么久就退出（默认一直跑到 Ctrl-C）
  --list-devices          列出音频设备后退出
  --quiet                 只打错误与最终报告
  --help                  打印这段说明

退出时总会打印一份汇总（含语音面 RTT 的 p50/p95/p99），可直接贴进报告。

提示：设备必须是 48 kHz。不是的话用 --mic / --speaker 换一个
（--list-devices 会列出每个设备支持的采样率）。";

/// 一帧的时长（毫秒）。语音面按这个节奏收发，与 `audio-core::FRAME_MS` 一致。
pub const FRAME_MS: u64 = 10;

/// 配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 控制面地址（主机:端口）。
    pub server: String,
    /// 用户名。
    pub user: String,
    /// 密码。
    pub password: String,
    /// 要进的频道：名字或 id。
    pub channel: String,
    /// 信任的证书（PEM）。
    pub ca: Option<PathBuf>,
    /// 跳过证书校验。
    pub insecure: bool,
    /// 证书里该匹配的名字。
    pub server_name: Option<String>,
    /// 麦克风设备名（子串匹配）。
    pub mic: Option<String>,
    /// 扬声器设备名（子串匹配）。
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
    /// 抖动缓冲的目标延迟下限（毫秒）。
    pub jitter_min_ms: u32,
    /// 抖动缓冲的目标延迟上限（毫秒）。上限就是延迟与抗抖动的旋钮。
    pub jitter_max_ms: u32,
    /// 本地 UDP 绑定地址。
    pub bind: String,
    /// 指标打印间隔；`None` 表示不打印。
    pub stats_interval: Option<Duration>,
    /// 打点间隔；`None` 表示不打点。
    pub probe_interval: Option<Duration>,
    /// 跑多久就退出；`None` 表示跑到 Ctrl-C。
    pub run_for: Option<Duration>,
    /// 只列设备。
    pub list_devices: bool,
    /// 只打错误与最终报告。
    pub quiet: bool,
}

impl Config {
    /// 解析参数。
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut server = "127.0.0.1:64738".to_owned();
        let mut user: Option<String> = None;
        let mut password = String::new();
        let mut channel = String::new();
        let mut ca: Option<PathBuf> = None;
        let mut insecure = false;
        let mut server_name: Option<String> = None;
        let mut mic: Option<String> = None;
        let mut speaker: Option<String> = None;
        let mut no_mic = false;
        let mut no_speaker = false;
        let mut chain = "highpass,agc2".to_owned();
        let mut dtx = false;
        let mut tone: Option<f32> = None;
        let mut jitter_min_ms = 20_u32;
        let mut jitter_max_ms = 60_u32;
        let mut bind = "0.0.0.0:0".to_owned();
        let mut stats_interval = Some(Duration::from_secs(5));
        // 5 秒一次：打点包**会占用一个语音序号**，接收端会把它算成 1 个丢包（见 app.rs 的报告）。
        // 1 秒一次会让 30 分钟的通话凭空多出 1% 的「丢包」。
        let mut probe_interval = Some(Duration::from_secs(5));
        let mut run_for: Option<Duration> = None;
        let mut list_devices = false;
        let mut quiet = false;

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = || -> Result<String, String> {
                args.next().ok_or_else(|| format!("{arg} 缺少取值"))
            };
            match arg.as_str() {
                "--server" => server = value()?,
                "--user" => user = Some(value()?),
                "--password" => password = value()?,
                "--channel" => channel = value()?,
                "--ca" => ca = Some(PathBuf::from(value()?)),
                "--insecure" => insecure = true,
                "--server-name" => server_name = Some(value()?),
                "--mic" => mic = Some(value()?),
                "--speaker" => speaker = Some(value()?),
                "--no-mic" => no_mic = true,
                "--no-speaker" => no_speaker = true,
                "--chain" => chain = value()?,
                "--dtx" => dtx = true,
                "--jitter-min" => {
                    jitter_min_ms = value()?
                        .parse()
                        .map_err(|_| "--jitter-min 需要毫秒数".to_owned())?;
                }
                "--jitter-max" => {
                    jitter_max_ms = value()?
                        .parse()
                        .map_err(|_| "--jitter-max 需要毫秒数".to_owned())?;
                }
                "--tone" => {
                    let value = value()?;
                    tone = Some(
                        value
                            .parse()
                            .map_err(|_| "--tone 需要频率（Hz）".to_owned())?,
                    );
                }
                "--bind" => bind = value()?,
                "--stats-interval" => stats_interval = seconds(&value()?, "--stats-interval")?,
                "--probe-interval" => probe_interval = seconds(&value()?, "--probe-interval")?,
                "--run-for" => run_for = seconds(&value()?, "--run-for")?,
                "--list-devices" => list_devices = true,
                "--quiet" => quiet = true,
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("未知参数 {other:?}")),
            }
        }

        Ok(Self {
            server,
            user: user.unwrap_or_default(),
            password,
            channel,
            ca,
            insecure,
            server_name,
            mic,
            speaker,
            no_mic,
            no_speaker,
            chain,
            dtx,
            tone,
            jitter_min_ms,
            jitter_max_ms,
            bind,
            stats_interval,
            probe_interval,
            run_for,
            list_devices,
            quiet,
        })
    }

    /// 校验真正连服务器才需要的必填项。
    ///
    /// `parse` 刻意宽松（`--list-devices` 不该要求登录信息），必填项集中在这里，
    /// 由 `main` 在确定要连服务器之后调一次。
    pub fn validate(&self) -> Result<(), String> {
        if self.user.is_empty() {
            return Err("缺少 --user".to_owned());
        }
        if self.password.is_empty() {
            return Err(
                "缺少密码：用 --password，或把密码放进环境变量 HUSHSQUAD_PASSWORD".to_owned(),
            );
        }
        Ok(())
    }

    /// 密码允许只放在环境变量里。
    pub fn with_env_password(mut self) -> Self {
        if self.password.is_empty()
            && let Ok(password) = std::env::var("HUSHSQUAD_PASSWORD")
        {
            self.password = password;
        }
        self
    }

    /// 证书里该匹配的名字：显式给的优先，否则取 `--server` 的主机部分。
    #[must_use]
    pub fn server_name(&self) -> &str {
        self.server_name
            .as_deref()
            .unwrap_or_else(|| host_of(&self.server))
    }
}

/// 把 `主机:端口` 里的主机切出来（IPv6 也按最后一个冒号切）。
#[must_use]
pub fn host_of(addr: &str) -> &str {
    addr.rsplit_once(':').map_or(addr, |(host, _)| host)
}

/// 把秒数解析成 `Duration`；`0` 表示关闭。
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
    fn required_fields_are_checked_by_validate_not_parse() {
        // parse 宽松：`--list-devices` 不该要求登录信息。
        let bare = parse(&[]).unwrap();
        assert!(bare.validate().is_err(), "没有 --user 必须报错");
        assert!(
            parse(&["--user", "alice"]).unwrap().validate().is_err(),
            "没有密码必须报错"
        );
        assert!(parse(&["--password", "pw"]).unwrap().validate().is_err());
        // 密码可以留空等环境变量补，所以 validate 只看最终值。
        let ok = parse(&["--user", "alice", "--password", "pw"]).unwrap();
        assert!(ok.validate().is_ok());
        assert_eq!(ok.password, "pw");
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let config = parse(&["--user", "alice", "--password", "pw"]).unwrap();
        assert_eq!(config.server, "127.0.0.1:64738");
        assert_eq!(config.chain, "highpass,agc2");
        assert_eq!(config.stats_interval, Some(Duration::from_secs(5)));
        assert_eq!(config.probe_interval, Some(Duration::from_secs(5)));
        assert!(config.run_for.is_none(), "默认跑到 Ctrl-C");
        assert!(!config.insecure, "默认必须校验证书");
        assert!(!config.dtx);
    }

    #[test]
    fn zero_turns_a_period_off() {
        let config = parse(&[
            "--user",
            "alice",
            "--password",
            "pw",
            "--stats-interval",
            "0",
            "--probe-interval",
            "0",
        ])
        .unwrap();
        assert!(config.stats_interval.is_none());
        assert!(config.probe_interval.is_none());
    }

    #[test]
    fn server_name_falls_back_to_the_host_part() {
        let config = parse(&[
            "--user",
            "a",
            "--password",
            "p",
            "--server",
            "voice.example:1234",
        ])
        .unwrap();
        assert_eq!(config.server_name(), "voice.example");
        let config = parse(&["--user", "a", "--password", "p", "--server-name", "other"]).unwrap();
        assert_eq!(config.server_name(), "other");
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert!(parse(&["--user", "a", "--password", "p", "--nope"]).is_err());
    }
}
