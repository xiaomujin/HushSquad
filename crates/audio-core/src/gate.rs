//! 上行门控：决定「这一帧要不要发出去」。
//!
//! 语音激活、按住说话、常开三种方式，差别**只**在这里。它们不改编码、不改混音、
//! 不改网络，只决定这一帧进不进 `encode_voice`。把这件事收在一个纯函数式状态机里，
//! 是因为它最容易出那种「听感上说不清、日志里看不出」的毛病（切字头、抽搐、漏尾音），
//! 而这些毛病全都能用「喂一串电平/VAD，断言开门关门的帧号」测出来。
//!
//! # 两个判据，不是一个
//!
//! `loud`（能量过门限）和 `speechy`（VAD 概率过线）**同时**成立才算「在说话」：
//!
//! - 只看能量：风扇、键盘、空调都会被当成语音发出去。降噪档存在的意义就没了。
//! - 只看 VAD：链上没有 VAD 时（`--chain none`，或整条链都不提供概率）概率恒为
//!   `1.0`（「我没有意见」，见 [`crate::Stage::process`]），那时门控就退化成常开——
//!   这正是想要的降级：没有判据就不要假装有。
//!
//! 所以 `1.0` 既是「确定是语音」也是「没有意见」，两者都放行，语义上不冲突。
//!
//! # 为什么要滞回，而不是单门限
//!
//! 单门限在门限附近会**每帧翻一次**：发 10 ms、停 10 ms、发 10 ms……听感是嗡嗡的
//! 断续，而且每次重新开门都要重新走一遍编码器状态。所以：
//!
//! - **开门要连续 `attack_frames` 帧**（默认 2 = 20 ms）：单帧的咔哒声打不开门；
//! - **关门要连续 `hold_frames` 帧**（默认 30 = 300 ms）：吃掉词尾，也让一句话里的
//!   字间停顿不至于把流切碎。
//!
//! 代价是「说完之后还会再发 300 ms 的安静」——那是真实存在的安静，不是噪声，也不占
//! 什么带宽（Opus 对静音帧的编码只有十几个字节）。
//!
//! # 按键属于「按住说话」，不是全局旁路
//!
//! 按住说话模式只认按键（[`TxGate::set_ptt`]）；语音激活与常开**完全不看**它。
//! 曾经的做法是「三种模式下按住都强制发送」，代价是语音激活里按一下键就能顶开
//! 一扇本该由 VAD 决定的门 —— 那样「说话方式」这四个字就名不副实了。现在的规矩：
//! 按键是按住说话这个模式的输入设备，换到别的模式它就不存在（界面上那颗热键也会
//! 被注销，见 `apps/app/src/ptt.rs`）。
//!
//! # 预缓冲不在这个模块里
//!
//! 开门前的 30 ms 要补发（否则切掉爆破音的起头），但**补发是发送侧的事**：它要一个
//! 环形缓冲和一套「先补旧的再发新的」的顺序保证。放进状态机里会让这个模块同时管
//! 决策和缓冲两件事，测起来也说不清。见 `apps/client/src/app.rs` 的 `preroll`。

/// 说话方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceMode {
    /// 语音激活：检测到说话才发（产品默认）。
    #[default]
    Vad,
    /// 按住说话：按住才发，松手就停。
    Ptt,
    /// 常开：一直发。对方的抖动缓冲最舒服，但会听到你的键盘和风扇。
    Open,
}

impl VoiceMode {
    /// 命令行 / IPC 用的名字。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vad => "vad",
            Self::Ptt => "ptt",
            Self::Open => "open",
        }
    }

    /// 从名字解析（`vad` / `ptt` / `open`）。
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim() {
            "vad" | "语音激活" => Ok(Self::Vad),
            "ptt" | "按住说话" => Ok(Self::Ptt),
            "open" | "常开" => Ok(Self::Open),
            other => Err(format!(
                "说话方式 {other:?} 不认识（可选：vad / ptt / open）"
            )),
        }
    }
}

/// 门控参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateConfig {
    /// 说话方式。
    pub mode: VoiceMode,
    /// 能量门限（dBFS）。低于它的帧一律不算语音。
    pub threshold_db: f32,
    /// 连续多少帧达标才开门。`0` 按 1 处理。
    pub attack_frames: u32,
    /// 关门后还继续发多少帧。
    pub hold_frames: u32,
    /// VAD 概率低于它就不算语音。`0.0` 表示不看 VAD（只看能量）。
    pub vad_floor: f32,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            mode: VoiceMode::Vad,
            // −50 dBFS：安静房间里正常说话大约在 −30 ~ −20，底噪在 −60 以下。
            // 这只是**地板**，真正把风扇挡在外面的是 VAD 概率。
            threshold_db: -50.0,
            attack_frames: 2,
            hold_frames: 30,
            vad_floor: 0.5,
        }
    }
}

/// 一帧的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateStep {
    /// 这一帧要不要发。
    pub send: bool,
    /// 这一帧把门打开了（发送侧要在这里补发预缓冲）。
    pub opened: bool,
    /// 这一帧把门关上了。
    pub closed: bool,
}

/// 门控状态机。
#[derive(Debug, Clone)]
pub struct TxGate {
    cfg: GateConfig,
    open: bool,
    /// 连续达标的帧数（开门计数）。
    above: u32,
    /// 连续不达标的帧数（关门计数）。
    below: u32,
    /// 「按住」是不是按着。**只有按住说话模式看它**。
    ptt: bool,
    /// 上一帧的 VAD 概率与电平，给界面看。
    vad: f32,
    peak_db: f32,
}

impl TxGate {
    /// 按配置新建。
    #[must_use]
    pub fn new(cfg: GateConfig) -> Self {
        Self {
            cfg,
            open: false,
            above: 0,
            below: 0,
            ptt: false,
            vad: 0.0,
            peak_db: -100.0,
        }
    }

    /// 当前参数。
    #[must_use]
    pub const fn config(&self) -> &GateConfig {
        &self.cfg
    }

    /// 换说话方式。换过去的时候门先关上，由下一帧重新判——
    /// 否则从常开切到语音激活会带着「开着」的状态一直发到下一个停顿。
    pub fn set_mode(&mut self, mode: VoiceMode) {
        if self.cfg.mode != mode {
            self.cfg.mode = mode;
            self.open = false;
            self.above = 0;
            self.below = 0;
        }
    }

    /// 换能量门限（dBFS）。
    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.cfg.threshold_db = threshold_db;
    }

    /// 换 VAD 判据下限（`0.0` = 不看 VAD）。
    pub fn set_vad_floor(&mut self, vad_floor: f32) {
        self.cfg.vad_floor = vad_floor;
    }

    /// 按住 / 松开。
    ///
    /// **只在 [`VoiceMode::Ptt`] 下有意义**：语音激活与常开模式不看这个标志，按住键
    /// 也不会多发一个字节（`the_key_does_nothing_outside_ptt_mode`）。曾经的做法是
    /// 「三种模式下按住都强制发送」，那让「语音激活」变成一个随时能被按键顶开的门——
    /// 说话方式就名不副实了。按键是按住说话的输入设备，不是全局的旁路。
    pub fn set_ptt(&mut self, held: bool) {
        self.ptt = held;
    }

    /// 强制关门并清计数（自己静音时用：解静音之后要重新判，不能带着旧状态继续发）。
    ///
    /// **不动**「按住」的状态：按着键去点了静音、再取消静音，手还在键上，
    /// 那就该继续发。`reset` 清的是判定，不是按键。
    pub fn reset(&mut self) {
        self.open = false;
        self.above = 0;
        self.below = 0;
    }

    /// 门现在开着吗。
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// 上一帧的 VAD 概率。
    #[must_use]
    pub const fn vad(&self) -> f32 {
        self.vad
    }

    /// 上一帧的电平（dBFS）。
    #[must_use]
    pub const fn peak_db(&self) -> f32 {
        self.peak_db
    }

    /// 判一帧。`peak_db` 是这一帧的峰值电平，`vad` 是处理链给的语音概率。
    pub fn step(&mut self, peak_db: f32, vad: f32) -> GateStep {
        let was_open = self.open;
        self.peak_db = peak_db;
        self.vad = vad;

        match self.cfg.mode {
            // 常开：不看电平也不看 VAD。按键在这里也没有意义（本来就是一直发）。
            VoiceMode::Open => self.open = true,
            // 按住说话：只看按键。松手立刻停——那 300 ms 的尾巴在这里是噪音。
            VoiceMode::Ptt => self.open = self.ptt,
            VoiceMode::Vad => {
                let speech = peak_db >= self.cfg.threshold_db && vad >= self.cfg.vad_floor;
                if self.open {
                    if speech {
                        self.below = 0;
                    } else {
                        self.below += 1;
                        if self.below > self.cfg.hold_frames {
                            self.open = false;
                        }
                    }
                } else if speech {
                    self.above += 1;
                    if self.above >= self.cfg.attack_frames.max(1) {
                        self.open = true;
                        self.above = 0;
                        self.below = 0;
                    }
                } else {
                    self.above = 0;
                }
            }
        }

        GateStep {
            send: self.open,
            opened: self.open && !was_open,
            closed: was_open && !self.open,
        }
    }
}

/// 一帧的峰值电平（dBFS）。全零帧给 −100（不是负无穷：界面要能算，也要能比大小）。
#[must_use]
pub fn peak_db(frame: &[f32]) -> f32 {
    peak_to_db(frame.iter().fold(0.0_f32, |acc, s| acc.max(s.abs())))
}

/// 峰值（线性）→ dBFS。调用方已经算过峰值时用它，别为了取 dB 再扫一遍 480 个样本。
#[must_use]
pub fn peak_to_db(peak: f32) -> f32 {
    if peak <= 1e-10 {
        -100.0
    } else {
        20.0 * peak.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 喂一帧，返回「发不发」。
    fn step(gate: &mut TxGate, db: f32, vad: f32) -> bool {
        gate.step(db, vad).send
    }

    #[test]
    fn open_mode_always_sends() {
        let mut gate = TxGate::new(GateConfig {
            mode: VoiceMode::Open,
            ..GateConfig::default()
        });
        for _ in 0..100 {
            assert!(step(&mut gate, -90.0, 0.0), "常开模式连全静音都要发");
        }
    }

    #[test]
    fn ptt_sends_only_while_held_and_stops_immediately() {
        let mut gate = TxGate::new(GateConfig {
            mode: VoiceMode::Ptt,
            ..GateConfig::default()
        });
        assert!(!step(&mut gate, -20.0, 1.0), "没按就不发，再大声也不发");
        gate.set_ptt(true);
        assert!(step(&mut gate, -20.0, 1.0));
        gate.set_ptt(false);
        // 松手立刻停：PTT 没有 hold，否则松开之后会把房间里的声音发出去 300 ms。
        assert!(!step(&mut gate, -20.0, 1.0), "松手必须当帧就停");
    }

    #[test]
    fn vad_opens_after_the_attack_window_and_stays_open_for_the_hold() {
        let cfg = GateConfig {
            mode: VoiceMode::Vad,
            attack_frames: 2,
            hold_frames: 3,
            ..GateConfig::default()
        };
        let mut gate = TxGate::new(cfg);
        assert!(!step(&mut gate, -20.0, 0.9), "第 1 帧还在 attack 窗口里");
        assert!(step(&mut gate, -20.0, 0.9), "第 2 帧开门");
        // 安静下来：hold_frames 帧内还发（吃掉词尾），之后关。
        for _ in 0..3 {
            assert!(step(&mut gate, -80.0, 0.0), "hold 期内要发");
        }
        assert!(!step(&mut gate, -80.0, 0.0), "hold 用完就该关");
    }

    #[test]
    fn a_single_loud_frame_does_not_open_the_gate() {
        // 咔哒声、鼠标点击：一帧就过去。开门必须连续达标。
        let mut gate = TxGate::new(GateConfig::default());
        assert!(!step(&mut gate, -20.0, 0.9));
        assert!(!step(&mut gate, -80.0, 0.0), "第 2 帧没达标，计数清零");
        assert!(!step(&mut gate, -20.0, 0.9), "计数从第 1 帧重新开始");
        assert!(step(&mut gate, -20.0, 0.9));
    }

    #[test]
    fn energy_floor_alone_can_hold_the_gate_shut() {
        // VAD 说「是语音」但电平低到听不见：不发。否则对方的抖动缓冲里全是空帧。
        let mut gate = TxGate::new(GateConfig::default());
        assert!(!step(&mut gate, -90.0, 1.0));
        assert!(!step(&mut gate, -90.0, 1.0));
        assert!(!gate.is_open());
    }

    #[test]
    fn vad_alone_can_hold_the_gate_shut() {
        // 大声但是稳态噪声（风扇直吹）：VAD 概率低，不发。这就是降噪档存在的意义。
        let mut gate = TxGate::new(GateConfig::default());
        assert!(!step(&mut gate, -20.0, 0.1));
        assert!(!step(&mut gate, -20.0, 0.1));
        assert!(!gate.is_open());
    }

    #[test]
    fn a_chain_without_vad_degrades_to_an_energy_gate() {
        // 链上没有 VAD 时概率恒为 1.0（「没有意见」）。这时门控必须还能用：
        // 靠能量门限。否则 `--chain none` 会让语音激活变成常开。
        let mut gate = TxGate::new(GateConfig::default());
        assert!(!step(&mut gate, -20.0, 1.0), "第 1 帧还在 attack 窗口里");
        assert!(step(&mut gate, -20.0, 1.0), "有声就该发");
        for _ in 0..40 {
            step(&mut gate, -90.0, 1.0);
        }
        assert!(!gate.is_open(), "没声就该关");
    }

    #[test]
    fn the_key_does_nothing_outside_ptt_mode() {
        // 语音激活：按住不放也照样由 VAD 说了算。这条钉的是「按键不是全局旁路」。
        let mut gate = TxGate::new(GateConfig::default());
        gate.set_ptt(true);
        for _ in 0..40 {
            assert!(!step(&mut gate, -90.0, 0.0), "静音时按住也不该开门");
        }
        assert!(!step(&mut gate, -20.0, 0.9), "第 1 帧还在 attack 窗口里");
        assert!(step(&mut gate, -20.0, 0.9), "有声才开，与按键无关");

        // 常开：松开也不会停。按键在这个模式下没有任何含义。
        let mut gate = TxGate::new(GateConfig {
            mode: VoiceMode::Open,
            ..GateConfig::default()
        });
        gate.set_ptt(false);
        assert!(step(&mut gate, -90.0, 0.0));
    }

    #[test]
    fn switching_mode_closes_the_gate_first() {
        // 从常开切到语音激活：不能带着「开着」的状态一路发下去。
        let mut gate = TxGate::new(GateConfig {
            mode: VoiceMode::Open,
            ..GateConfig::default()
        });
        assert!(step(&mut gate, -90.0, 0.0));
        gate.set_mode(VoiceMode::Vad);
        assert!(!gate.is_open(), "换模式要先把门关上");
        assert!(!step(&mut gate, -90.0, 0.0));
    }

    #[test]
    fn reset_forgets_the_decision_but_keeps_the_key() {
        let mut gate = TxGate::new(GateConfig {
            mode: VoiceMode::Ptt,
            ..GateConfig::default()
        });
        gate.set_ptt(true);
        assert!(gate.step(-20.0, 1.0).send);
        gate.reset();
        assert!(!gate.is_open(), "reset 之后要重新判");
        // 手还按在键上：reset 不该把按键状态也清掉。
        assert!(gate.step(-90.0, 0.0).send, "reset 不清「按住」");
    }

    #[test]
    fn opened_and_closed_fire_exactly_once_per_transition() {
        let cfg = GateConfig {
            attack_frames: 1,
            hold_frames: 0,
            ..GateConfig::default()
        };
        let mut gate = TxGate::new(cfg);
        let first = gate.step(-20.0, 1.0);
        assert!(first.send && first.opened && !first.closed);
        let second = gate.step(-20.0, 1.0);
        assert!(second.send && !second.opened, "还开着，不该再报 opened");
        let third = gate.step(-90.0, 0.0);
        assert!(!third.send && third.closed, "hold=0 时当帧就关");
        let fourth = gate.step(-90.0, 0.0);
        assert!(!fourth.closed, "已经关了，不该再报 closed");
    }

    #[test]
    fn peak_db_is_finite_for_silence() {
        assert_eq!(peak_db(&[0.0; 480]), -100.0);
        assert!((peak_db(&[0.5; 480]) + 6.020_6).abs() < 0.01);
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [VoiceMode::Vad, VoiceMode::Ptt, VoiceMode::Open] {
            assert_eq!(VoiceMode::parse(mode.as_str()).unwrap(), mode);
        }
        assert!(VoiceMode::parse("常开").is_ok());
        assert!(VoiceMode::parse("nope").is_err());
    }
}
