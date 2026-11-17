//! 客户端主循环：握手 → 绑定语音 → 进频道 → 10 ms 节奏的收发。
//!
//! # 为什么握手是顺序的、不是状态机
//!
//! `client-core` 是状态机（sans-io，可离线测），但**这个二进制**不需要把状态机摊平到事件循环里：
//! 启动只有五个「等一件事发生」的步骤，写成顺序代码比写成一堆 `select!` 分支容易看懂，
//! 也容易在出错时指出是哪一步。每一步都有超时。
//!
//! 握手之后才进事件循环：一条 TLS 读任务（读到的字节经通道送回）、一条 UDP、一个 10 ms 定时器。
//!
//! # 三条不变量
//!
//! 1. **epoch 永远是新的**：`Client` 在每条控制面消息上刷新频道快照，所以别人进出频道时
//!    我们发的包不会带着过期 epoch 被服务器丢掉（这是 e2e 里踩过的坑）；
//! 2. **语音包与音频帧同节奏**：一个 tick 最多取 2 帧，既跟上设备时钟，又不制造突发；
//! 3. **计数不许静默**：丢包、重放、解不开、环满、饿死、混音欠载，全部进统计行。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use audio_core::AudioCodec;
use audio_core::receive::{IncomingPacket, PcmReceiver, ReceivePath};
use client_core::{Client, ClientAction, ClientState, Incoming};
use protocol::ControlMessage;
use protocol::encode_frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::client::TlsStream;

use crate::audio::{Capture, Playout};
use crate::config::{Config, FRAME_MS};
use crate::control::{self, Trust};

/// 收发节奏。
const TICK: Duration = Duration::from_millis(FRAME_MS);
/// 握手每一步的超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 最多同时听几个人（与混音器槽位一致）。
const PEER_SLOTS: usize = audio_core::MAX_TRACKS;
/// RTT 样本上限（1 秒一个点 ⇒ 约 8.5 分钟）。
const RTT_WINDOW: usize = 512;
// 抖动缓冲的目标延迟区间由 `--jitter-min/--jitter-max` 给（见 config.rs）。
/// 连续多少帧低于静音门限才判定「没在说话」（DTX 用）。
const DTX_QUIET_FRAMES: u32 = 30;
/// 静音门限（约 −70 dBFS）。
const SILENCE_PEAK: f32 = 3.2e-4;
/// 每个说话人的入站包环深度。
const PACKET_RING: usize = 64;
/// 每个说话人的 PCM 环深度（帧）。它直接等于接收侧的额外延迟。
const PCM_FRAMES: usize = 4;
const CONTROL_BUF: usize = 8 * 1024;
const VOICE_BUF: usize = 1500;
const OUT_BUF: usize = protocol::MAX_ON_WIRE;
/// 一个 tick 最多取几帧上行（吸收设备时钟与定时器的差）。
const MAX_FRAMES_PER_TICK: usize = 2;
/// 多久做一次成员/轨的清理。
const GC_INTERVAL: Duration = Duration::from_secs(5);
/// 控制面心跳间隔。
const PING_INTERVAL: Duration = Duration::from_secs(10);
/// 多久没包就认为这个人不在了（且不在成员表里时才回收）。
const PEER_IDLE_MS: u64 = 60_000;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// 自进程启动起的毫秒数（单调）。
#[must_use]
pub fn now_ms() -> u64 {
    u64::try_from(epoch().elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// 自进程启动起的微秒数（单调）。打点用它。
#[must_use]
pub fn now_us() -> u64 {
    u64::try_from(epoch().elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// 一个正在说话（或说过话）的对方。
struct Peer {
    slot: u8,
    tx: audio_core::receive::PacketSender,
    path: ReceivePath,
    /// 没有扬声器时 PCM 没人取，由我们自己抽干，否则统计里的「环满丢弃」全是假的。
    drain: Option<PcmReceiver>,
    packets: u64,
    last_ms: u64,
}

/// 上行音源：真麦克风，或合成正弦音。
///
/// 两种音源各带一个编码器，但同一时刻只存在一个（枚举只能选一个变体），
/// 所以「每帧新建 Opus 编码器」这种事故不会发生。
enum Source {
    Capture(Box<Capture>),
    Tone {
        tone: Box<crate::audio::Tone>,
        codec: Box<audio_core::OpusCodec>,
    },
}

impl Source {
    fn pull(&mut self) -> Option<audio_core::Frame> {
        match self {
            Self::Capture(capture) => capture.pull(),
            Self::Tone { tone, .. } => Some(tone.pull()),
        }
    }

    fn encode(&mut self, frame: &audio_core::Frame, out: &mut [u8]) -> usize {
        match self {
            Self::Capture(capture) => capture.encode(frame, out),
            Self::Tone { codec, .. } => codec.encode(frame, out),
        }
    }
}

/// 一个说话人的汇总。
#[derive(Debug, Clone, Default)]
pub struct PeerSummary {
    /// 混音槽位。
    pub slot: u8,
    /// 进了抖动缓冲的包数。
    pub packets_in: u64,
    /// 解出来的帧数。
    pub frames_decoded: u64,
    /// 序列号空洞推断出的丢包数。
    pub packets_lost: u64,
    /// 抖动缓冲当前深度（毫秒）。
    pub buffered_ms: u32,
    /// 抖动缓冲的目标延迟（毫秒）。
    pub target_delay_ms: u32,
}

/// 一次会话的汇总。
///
/// 打印出来的是给人看的，这个结构体是给**测试与工具**看的：
/// 联调时可以断言「服务端确实转发了」「接收端确实解出来了」，而不是只靠肉眼看日志。
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// 会话时长（秒）。
    pub seconds: f64,
    /// 发出的语音包数。
    pub frames_out: u64,
    /// 发出的字节数（含 IP/UDP 之外的线格式开销）。
    pub bytes_out: u64,
    /// 收到的语音包数。
    pub frames_in: u64,
    /// 收到的字节数。
    pub bytes_in: u64,
    /// 解不开 / 重放的包数。
    pub recv_errors: u64,
    /// 发送失败次数。
    pub send_errors: u64,
    /// 序列号空洞丢包总数。
    pub packets_lost: u64,
    /// 语音面 RTT 的 p50（微秒）。
    pub rtt_p50_us: Option<u32>,
    /// 语音面 RTT 的 p95（微秒）。
    pub rtt_p95_us: Option<u32>,
    /// 每个说话人一条。
    pub peers: Vec<PeerSummary>,
    /// 采集：取到的帧数。
    pub capture_frames: u64,
    /// 采集：环里不足一帧的次数。
    pub capture_starved: u64,
    /// 采集：环满丢弃的样本数。
    pub capture_dropped_samples: u64,
    /// 混音：混过的帧数。
    pub mix_frames: u64,
    /// 混音：有轨取不到数据的帧数。
    pub mix_underruns: u64,
    /// 混音：软限幅之前的峰值。
    pub mix_peak: f32,
    /// 实际 tick 频率（Hz）。10 ms 节奏应该是 100 左右；明显偏低就是定时器粒度问题。
    pub tick_hz: f64,
    /// 发出去的打点包数。
    pub probes_sent: u64,
}

/// 客户端状态。
pub struct App {
    cfg: Config,
    client: Client,
    source: Option<Source>,
    playout: Option<Playout>,
    events: Receiver<audio_io::StreamEvent>,
    peers: BTreeMap<u32, Peer>,
    /// sender_id → 最后一次听到他语音的时刻（毫秒）。
    last_voice: BTreeMap<u32, u64>,
    frames_out: u64,
    bytes_out: u64,
    frames_in: u64,
    bytes_in: u64,
    send_errors: u64,
    recv_errors: u64,
    /// 发出去的打点包数（它们各占用一个语音序号，接收端会算成丢包）。
    probes_sent: u64,
    dtx_quiet: u32,
    dtx_silent: bool,
    rtt_us: std::collections::VecDeque<u32>,
    started: Instant,
    last_stats: Instant,
    last_probe: Instant,
    last_ping: Instant,
    last_gc: Instant,
    /// 走过多少个 tick（用来算实际节奏）。
    ticks: u64,
    /// 上一次 tick 的时刻。用来量「循环有没有迟到」——采集环满丢样本只可能由它引起。
    last_tick: Instant,
    /// 相邻两次 tick 间隔超过 LATE_TICK_MS 的次数。
    late_ticks: u64,
    /// 最长的一次 tick 间隔（毫秒）。
    max_late_ms: u64,
}

/// 一个 tick 超过这个间隔就算「迟到」。10 ms 的节奏留 5 ms 余量。
const LATE_TICK_MS: u64 = 15;
/// 跑一次完整会话：起音频、连服务器、进频道、收发，直到退出。
pub async fn run(cfg: Config) -> Result<Summary, String> {
    // 必须最先做：Windows 默认定时器粒度 15.6 ms，会把 10 ms 的收发节奏拉成 64 Hz（见 timer 模块）。
    let timer = crate::timer::TimerResolution::raise();
    if !cfg.quiet {
        println!(
            "定时器精度：{}",
            if timer.is_raised() {
                "1 ms"
            } else {
                "没能提升（节奏会偏慢，见 --stats 的 hz）"
            }
        );
    }

    let (events_tx, events_rx) = channel();

    // ---- 1. 先把音频起来：起不来就别连了 ----
    let playout = if cfg.no_speaker {
        None
    } else {
        Some(Playout::start(cfg.speaker.as_deref(), events_tx.clone())?)
    };
    // --tone 优先于麦克风：它就是为了「没有声卡也能验证链路」而存在的。
    let source = if let Some(hz) = cfg.tone {
        Some(Source::Tone {
            tone: Box::new(crate::audio::Tone::new(hz)),
            codec: Box::new(audio_core::OpusCodec::with_defaults().map_err(|e| e.to_string())?),
        })
    } else if cfg.no_mic {
        None
    } else {
        Some(Source::Capture(Box::new(Capture::start(
            cfg.mic.as_deref(),
            &cfg.chain,
            events_tx.clone(),
        )?)))
    };
    if !cfg.quiet {
        if let Some(playout) = &playout {
            let format = playout.format();
            println!(
                "扬声器：{} Hz / {} 声道 / {:?}",
                format.sample_rate, format.channels, format.sample_format
            );
        }
        match &source {
            Some(Source::Capture(capture)) => {
                let format = capture.format();
                println!(
                    "麦克风：{} Hz / {} 声道 / {:?}，处理链延迟 {:.1} ms",
                    format.sample_rate,
                    format.channels,
                    format.sample_format,
                    capture.chain_latency_ms()
                );
            }
            Some(Source::Tone { .. }) => {
                println!(
                    "音源：合成正弦 {} Hz（不开麦克风）",
                    cfg.tone.unwrap_or(0.0)
                );
            }
            None => println!("音源：无（只听）"),
        }
    }

    // ---- 2. 控制面 + TLS ----
    let local = crate::local::open(cfg.db.as_deref())?;
    let local = std::sync::Arc::new(std::sync::Mutex::new(local));
    let device_id = {
        let guard = local.lock().expect("锁没中毒");
        crate::local::resolve_device_id(&guard, cfg.device_id)?
    };
    if cfg.reset_trust {
        let guard = local.lock().expect("锁没中毒");
        guard
            .forget(&cfg.server)
            .map_err(|e| format!("忘记信任记录失败：{e}"))?;
        guard
            .forget_server_token(&cfg.server)
            .map_err(|e| format!("忘记免密 token 失败：{e}"))?;
        if !cfg.quiet {
            println!("已忘掉 {} 的证书指纹与免密 token", cfg.server);
        }
    }

    let trust = if cfg.insecure {
        Trust::Insecure
    } else if let Some(ca) = &cfg.ca {
        Trust::Ca(ca.clone())
    } else {
        Trust::Tofu {
            store: std::sync::Arc::clone(&local),
            host: cfg.server.clone(),
        }
    };
    let mut control = control::connect(&cfg.server, &trust, cfg.server_name()).await?;
    if cfg.insecure {
        eprintln!("警告：--insecure 已关闭证书校验。只在本机/内网联调用，中间人可以冒充服务器。");
    }

    let mut client = Client::new();
    let mut cbuf = vec![0u8; CONTROL_BUF];

    write_actions(&mut control, vec![client.hello()]).await?;
    pump_until(&mut control, &mut client, &mut cbuf, "ServerHello", |c| {
        c.state() == ClientState::AwaitAuth
    })
    .await?;

    // 语音密钥：TLS 导出器 → HKDF。上下文是 session_id || voice_token，服务器算的是同一个。
    let exporter = control::exporter(&control, client.session_id(), client.voice_token())?;
    client.set_exporter(&exporter, client.key_epoch());

    // ---- 认证：匿名只要设备号，账号要密码 ----
    let anonymous = cfg.is_anonymous();
    let request = if anonymous {
        if !cfg.quiet {
            println!(
                "以设备号匿名登录（{}）",
                store::client::format_device_id(&device_id)
            );
        }
        client.auth_device_token("", device_id.to_vec())
    } else {
        // 先看看上次密码登录有没有留下免密 token：有就先试它，省一次输密码。
        let stored = {
            let guard = local.lock().expect("锁没中毒");
            guard.server_token(&cfg.server).ok().flatten()
        };
        let password = if cfg.password.is_empty() {
            stored
                .as_ref()
                .map_or_else(|| prompt_password(&cfg.user), |_| Ok(String::new()))?
        } else {
            cfg.password.clone()
        };
        if !password.is_empty() {
            client.auth_password(&cfg.user, &password)
        } else if let Some(token) = stored {
            client.auth_device_token(&cfg.user, token)
        } else {
            client.auth_password(&cfg.user, &password)
        }
    };
    write_actions(&mut control, vec![request]).await?;
    pump_until(&mut control, &mut client, &mut cbuf, "AuthResult", |c| {
        matches!(c.state(), ClientState::Ready | ClientState::Closed)
    })
    .await?;
    if client.state() != ClientState::Ready {
        return Err("认证没通过".to_owned());
    }
    // 频道列表是**另**一条消息（ChannelSync），可能在下一批字节里才到。
    // 等它而不是假设它已经来了 —— 这两条在同一个 TCP 段里纯属运气。
    pump_until(&mut control, &mut client, &mut cbuf, "频道列表", |c| {
        !c.roster().channels().is_empty()
    })
    .await?;
    // 服务器在密码登录成功时会下发一个设备 token，存下来下次免密。
    if let Some(token) = client.take_device_token() {
        let guard = local.lock().expect("锁没中毒");
        let _ = guard.set_server_token(&cfg.server, &token);
    }
    if !cfg.quiet {
        let who = client
            .roster()
            .myself()
            .map_or_else(|| cfg.user.clone(), |me| me.name.clone());
        println!(
            "已登录：{who}（会话 {}，语音标识 {}）",
            client.session_id(),
            client.sender_id()
        );
    }

    // ---- 3. 语音面绑定 ----
    let (voice_host, voice_port) = {
        let (host, port) = client.voice_endpoint();
        (host.to_owned(), port)
    };
    let voice_addr = resolve(&voice_host, voice_port).await?;
    let udp = crate::voice::bind(&cfg.bind, voice_addr).await?;
    send_bind(&udp, &client).await?;
    wait_bind_ack(&udp, &mut client).await?;
    if !cfg.quiet {
        println!("语音面已绑定：{voice_addr}");
    }

    // ---- 4. 进频道 ----
    let channel_id = pick_channel(&client, &cfg.channel)?;
    write_actions(&mut control, vec![client.join_channel(channel_id)]).await?;
    pump_until(&mut control, &mut client, &mut cbuf, "频道快照", |c| {
        c.channel_epoch() != 0
    })
    .await?;
    if !cfg.quiet {
        let name = client
            .roster()
            .channels()
            .get(&channel_id)
            .map_or("?", |c| c.name.as_str());
        println!(
            "已进频道 {name}（id {channel_id}，epoch {}）",
            client.channel_epoch()
        );
    }

    // ---- 5. 事件循环 ----
    let mut app = App {
        cfg,
        client,
        source,
        playout,
        events: events_rx,
        peers: BTreeMap::new(),
        last_voice: BTreeMap::new(),
        frames_out: 0,
        bytes_out: 0,
        frames_in: 0,
        bytes_in: 0,
        send_errors: 0,
        recv_errors: 0,
        probes_sent: 0,
        dtx_quiet: 0,
        dtx_silent: false,
        rtt_us: std::collections::VecDeque::with_capacity(RTT_WINDOW),
        started: Instant::now(),
        last_stats: Instant::now(),
        last_probe: Instant::now(),
        last_ping: Instant::now(),
        last_gc: Instant::now(),
        ticks: 0,
        last_tick: Instant::now(),
        late_ticks: 0,
        max_late_ms: 0,
    };

    let outcome = app.drive(control, udp).await;
    app.report();
    // 报告打印给人看，`Summary` 交给测试与工具断言。失败时也要把汇总带出去，
    // 否则「跑挂了」这一条最有用的现场就没了。
    let summary = app.summary();
    outcome.map(|()| summary)
}
impl App {
    /// 事件循环。
    async fn drive(&mut self, control: TlsStream<TcpStream>, udp: UdpSocket) -> Result<(), String> {
        let (mut reader, mut writer) = tokio::io::split(control);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            let mut buf = vec![0u8; CONTROL_BUF];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        let mut tick = tokio::time::interval(TICK);
        // 卡过一次之后不要「补」一串 tick：那会在瞬间灌进几十帧，听感是爆音。
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        let deadline = self.cfg.run_for.map(|d| Instant::now() + d);
        let mut rbuf = [0u8; VOICE_BUF];

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.on_tick(&udp, &mut writer).await?;
                }
                message = rx.recv() => {
                    let Some(bytes) = message else {
                        return Err("服务器关闭了控制面连接".to_owned());
                    };
                    self.on_control(&bytes, &mut writer).await?;
                }
                received = udp.recv(&mut rbuf) => {
                    match received {
                        Ok(n) => self.on_datagram(&mut rbuf[..n]),
                        Err(e) => {
                            self.recv_errors += 1;
                            eprintln!("收语音包失败：{e}");
                        }
                    }
                }
                _ = &mut ctrl_c => {
                    if !self.cfg.quiet {
                        println!("收到 Ctrl-C，退出");
                    }
                    return Ok(());
                }
            }
            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                if !self.cfg.quiet {
                    println!("--run-for 到期，退出");
                }
                return Ok(());
            }
        }
    }

    /// 10 ms 一次：上行取帧发包、下行推进抖动缓冲、定期打点/心跳/统计。
    async fn on_tick(
        &mut self,
        udp: &UdpSocket,
        writer: &mut WriteHalf<TlsStream<TcpStream>>,
    ) -> Result<(), String> {
        self.ticks += 1;
        // 量一下「上一 tick 到这一 tick 隔了多久」。采集环满丢样本只可能由它引起：
        // 环里有 16 帧（160 ms）余量，只有循环卡住才会溢出。
        let now = Instant::now();
        let gap_ms =
            u64::try_from(now.duration_since(self.last_tick).as_millis()).unwrap_or(u64::MAX);
        self.last_tick = now;
        if gap_ms > LATE_TICK_MS {
            self.late_ticks += 1;
            self.max_late_ms = self.max_late_ms.max(gap_ms);
        }
        self.on_stream_events();
        self.send_voice(udp);
        for peer in self.peers.values_mut() {
            peer.path.tick();
            if let Some(drain) = peer.drain.as_mut() {
                let mut frame: audio_core::Frame = [0.0; audio_core::FRAME_SAMPLES];
                let _ = drain.pull_frame(&mut frame);
            }
        }

        if let Some(interval) = self.cfg.probe_interval
            && self.last_probe.elapsed() >= interval
        {
            self.last_probe = Instant::now();
            self.send_probe(udp);
        }
        if self.last_ping.elapsed() >= PING_INTERVAL {
            self.last_ping = Instant::now();
            let ping = self.client.ping(now_us());
            write_actions(writer, vec![ping]).await?;
        }
        if self.last_gc.elapsed() >= GC_INTERVAL {
            self.last_gc = Instant::now();
            self.collect_peers();
        }
        if let Some(interval) = self.cfg.stats_interval
            && self.last_stats.elapsed() >= interval
        {
            self.last_stats = Instant::now();
            if !self.cfg.quiet {
                println!("{}", self.stats_line());
            }
        }
        Ok(())
    }

    /// 上行：取至多两帧 → 处理链 → Opus → 语音面。
    fn send_voice(&mut self, udp: &UdpSocket) {
        let Some(source) = self.source.as_mut() else {
            return;
        };
        let mut packet = [0u8; OUT_BUF];
        let mut opus = [0u8; audio_core::MAX_PACKET_BYTES];
        let max_frames = match source {
            // 采集环里可能积了不止一帧（设备时钟比定时器快一点），允许一次取两帧追上。
            Source::Capture(_) => MAX_FRAMES_PER_TICK,
            // 合成音源由 tick 驱动：一个 tick 一帧，取多了就是凭空把包率翻倍。
            Source::Tone { .. } => 1,
        };
        for _ in 0..max_frames {
            let Some(frame) = source.pull() else {
                break;
            };
            let peak = frame.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));

            // DTX：安静够久就停发，只在边界发一个静音标记。默认关闭 ——
            // 一直发也有好处（NAT 保活、抖动统计连续），要不要省流量由用户决定。
            if self.cfg.dtx {
                if peak < SILENCE_PEAK {
                    self.dtx_quiet += 1;
                    if self.dtx_quiet >= DTX_QUIET_FRAMES && !self.dtx_silent {
                        self.dtx_silent = true;
                        if let Ok(n) = self.client.encode_silence(&mut packet) {
                            transmit(
                                udp,
                                &packet[..n],
                                &mut self.frames_out,
                                &mut self.bytes_out,
                                &mut self.send_errors,
                            );
                        }
                    }
                    continue;
                }
                self.dtx_quiet = 0;
                self.dtx_silent = false;
            }

            // 合成音也走 Opus：否则测的就不是真实链路。
            let len = source.encode(&frame, &mut opus);
            if len == 0 {
                continue;
            }
            match self.client.encode_voice(&opus[..len], &mut packet) {
                Ok(n) => transmit(
                    udp,
                    &packet[..n],
                    &mut self.frames_out,
                    &mut self.bytes_out,
                    &mut self.send_errors,
                ),
                Err(e) => {
                    self.send_errors += 1;
                    eprintln!("加密语音失败：{e}");
                }
            }
        }
    }

    /// 打点：测的是**语音面**的往返，而不是控制面的。
    fn send_probe(&mut self, udp: &UdpSocket) {
        let mut packet = [0u8; OUT_BUF];
        match self.client.encode_probe(now_us(), &mut packet) {
            Ok(n) => {
                // 打点包也占一个语音序号（nonce 的唯一性靠这一个计数器），
                // 所以接收端会把它算成 1 个空洞丢包 —— 报告里要把这笔账说清楚。
                self.probes_sent += 1;
                transmit(
                    udp,
                    &packet[..n],
                    &mut self.frames_out,
                    &mut self.bytes_out,
                    &mut self.send_errors,
                );
            }
            Err(e) => {
                self.send_errors += 1;
                eprintln!("加密打点包失败：{e}");
            }
        }
    }

    /// 控制面消息：全部交给 `client-core`，然后把它的动作执行掉。
    async fn on_control(
        &mut self,
        bytes: &[u8],
        writer: &mut WriteHalf<TlsStream<TcpStream>>,
    ) -> Result<(), String> {
        let actions = self
            .client
            .on_bytes(bytes)
            .map_err(|e| format!("控制面分帧失败：{e}"))?;
        write_actions(writer, actions).await
    }

    /// 一个入站语音包。
    fn on_datagram(&mut self, buf: &mut [u8]) {
        self.bytes_in += buf.len() as u64;
        match self.client.receive(buf) {
            Ok(Incoming::Voice {
                sender_id,
                seq,
                timestamp,
                payload,
            }) => {
                self.frames_in += 1;
                self.last_voice.insert(sender_id, now_ms());
                let packet = IncomingPacket::new(sender_id, seq as u16, timestamp, payload);
                self.route(sender_id, packet);
            }
            Ok(Incoming::Silence { sender_id }) => {
                self.last_voice.remove(&sender_id);
            }
            Ok(Incoming::BindAck { server_time_ms, .. }) => {
                self.client.note_server_time(server_time_ms, now_ms());
            }
            Ok(Incoming::ProbeAck { ts_us }) => {
                let rtt = now_us().saturating_sub(ts_us);
                if self.rtt_us.len() >= RTT_WINDOW {
                    self.rtt_us.pop_front();
                }
                self.rtt_us
                    .push_back(u32::try_from(rtt).unwrap_or(u32::MAX));
            }
            Err(e) => {
                self.recv_errors += 1;
                if self.recv_errors <= 3 {
                    eprintln!("丢掉一个语音包：{e}");
                }
            }
        }
    }

    /// 把包投给这个说话人的接收路径（第一次见他就建一条）。
    fn route(&mut self, sender_id: u32, packet: IncomingPacket) {
        if !self.peers.contains_key(&sender_id) {
            let used: Vec<u8> = self.peers.values().map(|peer| peer.slot).collect();
            let Some(slot) = (0..PEER_SLOTS as u8).find(|slot| !used.contains(slot)) else {
                return;
            };
            let Ok((tx, mut path, pcm)) = ReceivePath::new(sender_id, PACKET_RING, PCM_FRAMES)
            else {
                return;
            };
            path.set_delay_limits(self.cfg.jitter_min_ms, self.cfg.jitter_max_ms);
            if self.cfg.no_drift {
                path.set_drift_config(audio_core::DriftConfig {
                    enabled: false,
                    ..audio_core::DriftConfig::default()
                });
            }
            // 有扬声器就把 PCM 交给混音器；没有就自己抽干，别让环满污染统计。
            let drain = match &self.playout {
                Some(playout) => {
                    if playout
                        .mix()
                        .add_track(usize::from(slot), Box::new(pcm))
                        .is_err()
                    {
                        return;
                    }
                    None
                }
                None => Some(pcm),
            };
            self.peers.insert(
                sender_id,
                Peer {
                    slot,
                    tx,
                    path,
                    drain,
                    packets: 0,
                    last_ms: now_ms(),
                },
            );
        }
        if let Some(peer) = self.peers.get_mut(&sender_id) {
            if peer.tx.send(packet) {
                peer.packets += 1;
            }
            peer.last_ms = now_ms();
        }
    }

    /// 回收已经不存在的说话人（成员表里没有、且很久没包）。
    fn collect_peers(&mut self) {
        let now = now_ms();
        let stale: Vec<u32> = {
            let roster = self.client.roster();
            self.peers
                .iter()
                .filter(|(sender_id, peer)| {
                    roster.name_of_sender(**sender_id).is_none()
                        && now.saturating_sub(peer.last_ms) > PEER_IDLE_MS
                })
                .map(|(sender_id, _)| *sender_id)
                .collect()
        };
        for sender_id in stale {
            if let Some(peer) = self.peers.remove(&sender_id)
                && let Some(playout) = &self.playout
            {
                let _ = playout.mix().remove_track(usize::from(peer.slot));
            }
        }
        let cutoff = now.saturating_sub(500);
        self.last_voice.retain(|_, at| *at >= cutoff);
    }

    /// 音频流事件：xrun 只计数，Fatal 大声报出来（v1 不自动重建流，见文档「已知缺口」）。
    fn on_stream_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                audio_io::StreamEvent::Warning { kind, message } => {
                    eprintln!("音频流警告（{kind}）：{message}");
                }
                audio_io::StreamEvent::Fatal { kind, message } => {
                    eprintln!(
                        "音频流失效（{kind}）：{message}\n\
                         这一路已经不能用了；v1 不会自动重建，请检查设备后重开客户端。"
                    );
                }
            }
        }
    }
    /// 一行统计，键值对，方便直接 grep / 贴进报告。
    fn stats_line(&self) -> String {
        let mut lost = 0_u64;
        let mut jitter = 0_u32;
        let mut target = 0_u32;
        let mut slip = 0_u64;
        // 前馈量出来的时钟漂移（ppm）。30 s 之后才有意义，所以起播那几行会显示 +0ppm。
        let mut drift_ppm = 0.0_f32;
        for peer in self.peers.values() {
            let stats = peer.path.stats();
            lost += stats.packets_lost;
            jitter = jitter.max(peer.path.buffered_ms());
            target = target.max(peer.path.target_delay_ms());
            let drift = peer.path.drift_stats();
            slip += drift.excised_samples + drift.inserted_samples;
            let ppm = peer.path.drift_estimate() * 1e6;
            if ppm.abs() > drift_ppm.abs() {
                drift_ppm = ppm;
            }
        }
        let (mix_under, mix_clipped, mix_peak) =
            self.playout.as_ref().map_or((0, 0, 0.0), |playout| {
                let counters = playout.counters();
                (
                    counters
                        .underruns
                        .load(std::sync::atomic::Ordering::Relaxed),
                    counters.clipped.load(std::sync::atomic::Ordering::Relaxed),
                    counters.peak(),
                )
            });
        format!(
            "[stats] t={}s out={}f/{}B in={}f/{}B lost={lost} jitter={jitter}ms \
             target={target}ms slip={slip} drift={drift_ppm:+.0}ppm mix_under={mix_under} \
             clip={mix_clipped} peak={mix_peak:.3} rtt={} speaking={}",
            self.started.elapsed().as_secs(),
            self.frames_out,
            self.bytes_out,
            self.frames_in,
            self.bytes_in,
            self.describe_rtt(),
            self.speaking(),
        )
    }

    /// 谁在说话（最近 500 ms 有语音包的人）。
    fn speaking(&self) -> String {
        let roster = self.client.roster();
        let names: Vec<&str> = self
            .last_voice
            .keys()
            .filter_map(|sender_id| roster.name_of_sender(*sender_id))
            .collect();
        if names.is_empty() {
            "-".to_owned()
        } else {
            names.join("+")
        }
    }

    fn describe_rtt(&self) -> String {
        let Some(p50) = percentile(&self.rtt_us, 0.5) else {
            return "-".to_owned();
        };
        let p95 = percentile(&self.rtt_us, 0.95).unwrap_or(p50);
        format!(
            "{:.1}/{:.1}ms",
            f64::from(p50) / 1000.0,
            f64::from(p95) / 1000.0
        )
    }

    /// 把这一次会话的数字收成一份结构体（`report` 与测试都从这里取）。
    fn summary(&self) -> Summary {
        let peers: Vec<PeerSummary> = self
            .peers
            .values()
            .map(|peer| {
                let stats = peer.path.stats();
                PeerSummary {
                    slot: peer.slot,
                    packets_in: stats.packets_in,
                    frames_decoded: stats.frames_decoded,
                    packets_lost: stats.packets_lost,
                    buffered_ms: peer.path.buffered_ms(),
                    target_delay_ms: peer.path.target_delay_ms(),
                }
            })
            .collect();
        let (capture_frames, capture_starved, capture_dropped_samples) = match &self.source {
            Some(Source::Capture(capture)) => {
                let (frames, starved, dropped, _peak) = capture.stats();
                (frames, starved, dropped)
            }
            _ => (0, 0, 0),
        };
        let (mix_frames, mix_underruns, mix_peak) =
            self.playout.as_ref().map_or((0, 0, 0.0), |playout| {
                let counters = playout.counters();
                (
                    counters.frames.load(std::sync::atomic::Ordering::Relaxed),
                    counters
                        .underruns
                        .load(std::sync::atomic::Ordering::Relaxed),
                    counters.peak(),
                )
            });
        Summary {
            seconds: self.started.elapsed().as_secs_f64(),
            frames_out: self.frames_out,
            bytes_out: self.bytes_out,
            frames_in: self.frames_in,
            bytes_in: self.bytes_in,
            recv_errors: self.recv_errors,
            send_errors: self.send_errors,
            packets_lost: peers.iter().map(|peer| peer.packets_lost).sum(),
            rtt_p50_us: percentile(&self.rtt_us, 0.5),
            rtt_p95_us: percentile(&self.rtt_us, 0.95),
            peers,
            capture_frames,
            capture_starved,
            capture_dropped_samples,
            mix_frames,
            mix_underruns,
            mix_peak,
            probes_sent: self.probes_sent,
            tick_hz: {
                let seconds = self.started.elapsed().as_secs_f64();
                if seconds > 0.0 {
                    self.ticks as f64 / seconds
                } else {
                    0.0
                }
            },
        }
    }

    /// 退出汇总。
    fn report(&self) {
        let seconds = self.started.elapsed().as_secs_f64().max(0.001);
        println!("\n===== HushSquad 客户端汇总 =====");
        println!(
            "时长 {:.1} s；上行 {} 帧 / {:.1} kB / {:.1} kbps；下行 {} 帧 / {:.1} kB / {:.1} kbps",
            seconds,
            self.frames_out,
            self.bytes_out as f64 / 1000.0,
            self.bytes_out as f64 * 8.0 / 1000.0 / seconds,
            self.frames_in,
            self.bytes_in as f64 / 1000.0,
            self.bytes_in as f64 * 8.0 / 1000.0 / seconds,
        );
        println!(
            "收发节奏 {:.1} Hz（10 ms 节奏应当接近 100）；迟到 {} 次，最长 {} ms",
            self.summary().tick_hz,
            self.late_ticks,
            self.max_late_ms
        );
        println!(
            "序列号空洞丢包 {} 个（其中最多 {} 个是打点包占用的序号，不是真丢）；\
             解不开/重放 {} 个；发送失败 {} 次",
            self.peers
                .values()
                .map(|peer| peer.path.stats().packets_lost)
                .sum::<u64>(),
            self.probes_sent,
            self.recv_errors,
            self.send_errors,
        );
        if self.rtt_us.is_empty() {
            println!("语音面 RTT：没有样本（--probe-interval 0？）");
        } else {
            for (label, q) in [("p50", 0.5), ("p95", 0.95), ("p99", 0.99)] {
                if let Some(value) = percentile(&self.rtt_us, q) {
                    println!("语音面 RTT {label} = {:.1} ms", f64::from(value) / 1000.0);
                }
            }
        }
        for peer in self.peers.values() {
            let stats = peer.path.stats();
            println!(
                "  speaker {}：收 {} 包，解 {} 帧，丢 {} 包，拒 {} 包，缓冲 {} ms（目标 {} ms）",
                peer.slot,
                stats.packets_in,
                stats.frames_decoded,
                stats.packets_lost,
                stats.packets_rejected,
                peer.path.buffered_ms(),
                peer.path.target_delay_ms(),
            );
        }
        if let Some(Source::Capture(capture)) = &self.source {
            let (frames, starved, dropped, peak) = capture.stats();
            println!(
                "采集：取 {frames} 帧，饿死 {starved} 次，环满丢 {dropped} 样本，峰值 {peak:.3}"
            );
        }
        if let Some(playout) = &self.playout {
            let counters = playout.counters();
            println!(
                "混音：{} 帧，欠载 {} 帧，软限幅 {} 样本，峰值 {:.3}",
                counters.frames.load(std::sync::atomic::Ordering::Relaxed),
                counters
                    .underruns
                    .load(std::sync::atomic::Ordering::Relaxed),
                counters.clipped.load(std::sync::atomic::Ordering::Relaxed),
                counters.peak(),
            );
        }
    }
}
/// 发一个语音面包，并记账。
///
/// 做成自由函数（而不是 `&mut self` 的方法）是为了让调用点能**分开借**：
/// 上行路径里 `self.capture` 已经被借走了，再借整个 `self` 编译器不让，
/// 但借几个互不相干的计数字段是可以的。
fn transmit(
    udp: &UdpSocket,
    bytes: &[u8],
    frames_out: &mut u64,
    bytes_out: &mut u64,
    send_errors: &mut u64,
) {
    match udp.try_send(bytes) {
        Ok(_) => {
            *frames_out += 1;
            *bytes_out += bytes.len() as u64;
        }
        Err(e) => {
            *send_errors += 1;
            if *send_errors <= 3 {
                eprintln!("发语音包失败：{e}");
            }
        }
    }
}

/// 分位数（线性插值）。样本为空时返回 `None`。
fn percentile(samples: &std::collections::VecDeque<u32>, q: f64) -> Option<u32> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<u32> = samples.iter().copied().collect();
    sorted.sort_unstable();
    let position = q * (sorted.len() - 1) as f64;
    let index = position.round() as usize;
    sorted.get(index.min(sorted.len() - 1)).copied()
}

/// 交互式问密码（不回显）。非交互时读一行，方便脚本里 echo pw | client。
fn prompt_password(user: &str) -> Result<String, String> {
    use std::io::{BufRead, IsTerminal};
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password(format!("{user} 的密码："))
            .map_err(|e| format!("读密码失败：{e}"));
    }
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("读密码失败：{e}"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// 把动作变成字节写进控制面。
///
/// 泛型是因为握手阶段写的是整条 `TlsStream`，事件循环里写的是它的 `WriteHalf`。
async fn write_actions<W>(writer: &mut W, actions: Vec<ClientAction>) -> Result<(), String>
where
    W: AsyncWriteExt + Unpin,
{
    for action in actions {
        match action {
            ClientAction::Send(message) => {
                let mut frame = Vec::new();
                encode_frame(&message, &mut frame)
                    .map_err(|e| format!("编码控制面消息失败：{e}"))?;
                writer
                    .write_all(&frame)
                    .await
                    .map_err(|e| format!("写控制面失败：{e}"))?;
            }
            ClientAction::ServerError {
                code,
                message,
                fatal,
            } => {
                eprintln!("服务器错误 {code}：{message}");
                if fatal {
                    return Err(format!("服务器拒绝了会话：{message}"));
                }
            }
            ClientAction::Closed => return Err("服务器关闭了会话".to_owned()),
            // 成员/频道变化：`Client` 自己已经更新过镜像，这里没有副作用要做。
            ClientAction::RosterChanged => {}
            // 绑定由 `run` 显式做（它要发 UDP，而这里只碰 TLS）。
            ClientAction::BindVoice { .. } => {}
        }
    }
    Ok(())
}

/// 握手超时的报错：把现场一起说出来，省得下一次还要加打印。
fn timeout_error(what: &str, client: &Client) -> String {
    let roster = client.roster();
    let channels = roster
        .channels()
        .values()
        .map(|channel| {
            format!(
                "{}（id {}，epoch {}）",
                channel.name, channel.id, channel.epoch
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let members = roster
        .members()
        .values()
        .map(|member| {
            format!(
                "{}（user {} → sender {}，频道 {}）",
                member.name, member.user_id, member.sender_id, member.channel_id
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let head = format!(
        "等 {what} 超时（{} 秒）：状态 {:?}，我是 user {}，我在频道 {}",
        HANDSHAKE_TIMEOUT.as_secs(),
        client.state(),
        roster.self_user_id(),
        roster.my_channel()
    );
    format!(
        "{head}；频道表 [{channels}]；成员 {} 人 [{members}]",
        roster.members().len()
    )
}

/// 反复读控制面直到 `ready` 成立。
async fn pump_until<F>(
    control: &mut TlsStream<TcpStream>,
    client: &mut Client,
    buf: &mut [u8],
    what: &str,
    ready: F,
) -> Result<(), String>
where
    F: Fn(&Client) -> bool,
{
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if ready(client) {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timeout_error(what, client));
        }
        let n = match tokio::time::timeout(left, control.read(buf)).await {
            Err(_) => return Err(timeout_error(what, client)),
            Ok(Err(e)) => return Err(format!("读控制面失败：{e}")),
            Ok(Ok(n)) => n,
        };
        if n == 0 {
            return Err(format!("等 {what} 时服务器关闭了连接"));
        }
        let actions = client
            .on_bytes(&buf[..n])
            .map_err(|e| format!("控制面分帧失败：{e}"))?;
        for action in actions {
            match action {
                ClientAction::Send(message) => {
                    let mut frame = Vec::new();
                    encode_frame(&message, &mut frame)
                        .map_err(|e| format!("编码控制面消息失败：{e}"))?;
                    control
                        .write_all(&frame)
                        .await
                        .map_err(|e| format!("写控制面失败：{e}"))?;
                }
                ClientAction::ServerError {
                    code,
                    message,
                    fatal,
                } => {
                    eprintln!("服务器错误 {code}：{message}");
                    if fatal {
                        return Err(format!("服务器拒绝了会话：{message}"));
                    }
                }
                ClientAction::Closed => return Err("服务器关闭了会话".to_owned()),
                ClientAction::RosterChanged | ClientAction::BindVoice { .. } => {}
            }
        }
    }
}

/// 发 Bind（明文，唯一一个不需要密钥的包）。
async fn send_bind(udp: &UdpSocket, client: &Client) -> Result<(), String> {
    let mut buf = [0u8; 64];
    let n = client.bind_packet(&mut buf);
    udp.send(&buf[..n])
        .await
        .map_err(|e| format!("发 Bind 失败：{e}"))?;
    Ok(())
}

/// 等 BindAck：它是**加密**的，解不开就说明导出器/密钥派生两端不一致，直接失败。
async fn wait_bind_ack(udp: &UdpSocket, client: &mut Client) -> Result<(), String> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut buf = [0u8; VOICE_BUF];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "等 BindAck 超时（{} 秒）：语音面可能被防火墙挡住了",
                HANDSHAKE_TIMEOUT.as_secs()
            ));
        }
        let n = match tokio::time::timeout(left, udp.recv(&mut buf)).await {
            Err(_) => continue,
            Ok(Err(e)) => return Err(format!("收 BindAck 失败：{e}")),
            Ok(Ok(n)) => n,
        };
        match client.receive(&mut buf[..n]) {
            Ok(Incoming::BindAck { server_time_ms, .. }) => {
                client.note_server_time(server_time_ms, now_ms());
                return Ok(());
            }
            Ok(_) => continue,
            Err(e) => {
                return Err(format!(
                    "BindAck 解不开（{e}）：两端的语音密钥派生结果不一致"
                ));
            }
        }
    }
}

/// 按名字或 id 挑频道。
fn pick_channel(client: &Client, want: &str) -> Result<u32, String> {
    let channels = client.roster().channels();
    if channels.is_empty() {
        return Err("服务器上一个频道都没有（在服务器端用 --channel 预建）".to_owned());
    }
    if want.is_empty() {
        return Ok(channels.keys().next().copied().unwrap_or(0));
    }
    if let Ok(id) = want.parse::<u32>()
        && channels.contains_key(&id)
    {
        return Ok(id);
    }
    if let Some(channel) = channels.values().find(|channel| channel.name == want) {
        return Ok(channel.id);
    }
    let names = channels
        .values()
        .map(|channel| format!("{}（id {}）", channel.name, channel.id))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!("找不到频道 {want:?}；现有：{names}"))
}

/// 解析语音面地址（服务器可能给的是主机名）。
async fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("解析语音面地址 {host}:{port} 失败：{e}"))?;
    addrs
        .next()
        .ok_or_else(|| format!("语音面地址 {host}:{port} 解析不出结果"))
}

/// 一条控制面消息的类型名（日志用）。
#[must_use]
pub fn message_name(message: &ControlMessage) -> &'static str {
    message.kind_name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::control::proto::control_message::Kind;

    #[test]
    fn percentiles_are_interpolated_and_empty_is_none() {
        assert_eq!(percentile(&std::collections::VecDeque::new(), 0.5), None);
        let mut samples = std::collections::VecDeque::new();
        for value in [10_u32, 20, 30, 40, 50] {
            samples.push_back(value);
        }
        assert_eq!(percentile(&samples, 0.5), Some(30));
        assert_eq!(percentile(&samples, 1.0), Some(50));
        assert_eq!(percentile(&samples, 0.0), Some(10));
    }

    #[test]
    fn channel_selection_prefers_name_then_id() {
        let mut client = Client::new();
        assert!(pick_channel(&client, "").is_err(), "没有频道时必须报错");

        let sync = ControlMessage {
            protocol: protocol::PROTOCOL_VERSION,
            request_id: 0,
            kind: Some(Kind::ChannelSync(protocol::control::proto::ChannelSync {
                channels: vec![protocol::control::proto::ChannelState {
                    channel_id: 7,
                    name: "大厅".to_owned(),
                    channel_epoch: 3,
                    ..Default::default()
                }],
                users: Vec::new(),
                self_channel_id: 0,
            })),
        };
        client.on_message(sync);
        assert_eq!(pick_channel(&client, "").unwrap(), 7);
        assert_eq!(pick_channel(&client, "大厅").unwrap(), 7);
        assert_eq!(pick_channel(&client, "7").unwrap(), 7);
        assert!(pick_channel(&client, "不存在").is_err());
    }

    #[test]
    fn message_name_survives_for_logging() {
        let message = ControlMessage {
            protocol: protocol::PROTOCOL_VERSION,
            request_id: 0,
            kind: Some(Kind::Ping(protocol::control::proto::Ping { ts_us: 1 })),
        };
        assert_eq!(message_name(&message), "ping");
    }
}
