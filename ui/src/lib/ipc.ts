// IPC 契约：命令与事件的**唯一**定义处（ADR-0013 §决策 2）。
//
// 规则：
// - 前端不碰 PCM、不碰网络包、不算延迟账；这里出现的数字都是后端给的，原样显示。
// - 低频「状态变了」走事件；高频「数字在动」走流（Tauri 侧是 Channel<T>，浏览器侧是 MockBackend 的定时器）。
// - 将来用 ts-rs 从 Rust 结构体生成这些类型，两边就不会再手工对齐。

export type Phase =
  | 'disconnected'
  | 'authenticating'
  | 'connected'
  | 'joining'
  | 'in_channel'
  | 'reconnecting'
  | 'error';

export type View = 'grid' | 'list';

/** 频道。`parent = 0` 表示根；`flags & 8` 表示分组容器（不可加入）。 */
export interface Channel {
  id: number;
  parent: number;
  name: string;
  position: number;
  max_users: number;
  flags: number;
  /** 本频道人数（不含子树）。 */
  members: number;
}

export const FLAG_CONTAINER = 8;

export interface Member {
  user_id: number;
  name: string;
  channel_id: number;
  /** 自己静音（对别人）。 */
  mute: boolean;
  /** 独听（只听他）。 */
  solo: boolean;
  /** 服务器侧静音。 */
  server_mute: boolean;
  /** 0–200 的显示音量，100 = 原样。 */
  volume: number;
}

/**
 * 权限位（ADR-0016）。与 `server-core::perms` 一一对应。
 *
 * 界面**只**用它决定菜单显示什么；真正的校验在服务器，这里算错了最多多显示一个
 * 点了没反应的按钮，不会多给一分权限。
 */
export const PERM = {
  JOIN_CHANNEL: 1,
  MOVE_ANYONE: 2,
  KICK: 4,
  BAN: 8,
  CREATE_CHANNEL: 16,
  MANAGE_CHANNEL: 32,
  ADMIN: 64,
  MUTE_OTHERS: 128,
  OWNER: 256,
} as const;

/** 内置角色：位图的命名组合（与 `server-core::perms` 一致）。 */
export const ROLES: { id: string; label: string; perms: number }[] = [
  { id: 'owner', label: '服主', perms: 511 },
  { id: 'admin', label: '管理员', perms: 255 },
  { id: 'moderator', label: '协管', perms: 135 },
  { id: 'member', label: '成员', perms: 1 },
];

/** 位图 → 角色名；不在内置角色里时返回 null（界面显示「自定义」）。 */
export function roleOf(perms: number): { id: string; label: string; perms: number } | null {
  return ROLES.find((role) => role.perms === perms) ?? null;
}

/** 账号列表里的一行（管理面板用）。 */
export interface Account {
  user_id: number;
  name: string;
  perms: number;
  anonymous: boolean;
  online: boolean;
}

/** 10–20 Hz 合并推送。 */
export interface Meters {
  talking: boolean;
  self_level: number;
  tx: boolean;
  /**
   * 上行门控现在开着（这一帧会发）。
   *
   * `tx === false && gate === true` 是「包没发出去」，不是「你在静音」——
   * 界面必须靠这两个字段区分「没在说」和「说了但发不出去」。
   */
  gate?: boolean;
  /** 处理链给的 VAD 概率 0–1（`1` 表示这条链不提供 VAD）。设置页的灵敏度条靠它。 */
  vad?: number;
  frames_sent: number;
  /** 上行处理链的算法延迟（ms）。 */
  chain_ms: number;
  /** 每个 user_id 的瞬时电平 0–1。 */
  levels: Record<number, number>;
}

/** 5 s 一次。 */
export interface Net {
  jitter_ms: number;
  jitter_target_ms: number;
  jitter_max_ms: number;
  /** 序列号空洞推断出的丢包**个数**（含打点包占用的序号）。 */
  loss: number;
  rtt_p50: number;
  rtt_p95: number;
  underruns: number;
  /** 采集环满丢弃的样本数；后端没报就是 `undefined`。 */
  ring_drops?: number;
  kbps: number;
  /** 实际 tick 频率。10 ms 节奏应当接近 100；明显偏低就是定时器粒度问题。 */
  tick_hz?: number;
  /**
   * 软件预算逐项（ADR-0010：承诺口径只含这些）。
   *
   * 真后端**给不出**这一项：分解要跑一次自检，而自检还没接进会话。
   * 缺省时面板显示「未测量」，不拿 0 冒充。
   */
  budget?: { capture: number; chain: number; encode: number; network: number; jitter: number; playout: number };
  /** 设备段实测，单列，不进承诺值。同样来自自检。 */
  device?: { digital_loopback_ms: number; acoustic_loopback_ms: number };
}

export interface ConnectRequest {
  addr: string;
  identity: { kind: 'anonymous' } | { kind: 'account'; user: string; password: string };
  trust: { kind: 'remember'; fingerprint: string } | { kind: 'reject' };
  /** 上行语音设置。跟着连接一起下去，界面一进来就是语音激活。 */
  voice: VoiceSettings;
}

/**
 * 上行语音设置。
 *
 * `preroll_frames` 是**开门时补发**的帧数（1 帧 = 10 ms）。它不是「越多越好」：
 * 补发是一次性突发，全部挤进对方抖动缓冲的一瞬间，超过缓冲余量就会被裁掉。
 * 上限 10 帧，默认 3 帧（30 ms）—— 见 ADR-0015。
 */
export interface VoiceSettings {
  mode: 'vad' | 'ptt' | 'open';
  threshold_db: number;
  preroll_frames: number;
}

export interface Handlers {
  onPhase: (phase: Phase, error?: string) => void;
  onChannels: (channels: Channel[], selfChannel: number) => void;
  onMembers: (members: Member[], selfUserId: number) => void;
  onMeters: (meters: Meters) => void;
  /** 自己的权限位变了（认证时一次，管理员改动时再来）。 */
  onPerms: (perms: number) => void;
  /** 账号列表（管理面板请求之后来一次）。 */
  onAccounts: (accounts: Account[]) => void;
  onNet: (net: Net) => void;
}

/** 后端接口。Tauri 侧与浏览器侧的 Mock 实现同一份。 */
export interface Backend {
  connect(req: ConnectRequest): Promise<void>;
  disconnect(): Promise<void>;
  joinChannel(channelId: number): Promise<void>;
  setVoice(settings: VoiceSettings): Promise<void>;
  /**
   * 按住 / 松开「强制发送」（Space）。
   *
   * 三种说话方式下按住都强制发送——这是设置页里给用户的承诺。
   */
  setPtt(held: boolean): Promise<void>;
  setSelf(state: { mute?: boolean; deafen?: boolean }): Promise<void>;
  setUser(userId: number, patch: { volume?: number; mute?: boolean; solo?: boolean }): Promise<void>;
  setView(view: View): Promise<void>;
  /** 把别人移到另一个频道（需要 MOVE_ANYONE）。 */
  moveUser(userId: number, channelId: number): Promise<void>;
  /** 服务器静音 / 闭麦别人（需要 MUTE_OTHERS）。 */
  setUserState(userId: number, patch: { server_mute?: boolean; server_deaf?: boolean }): Promise<void>;
  /** 踢出服务器（需要 KICK）。 */
  kickUser(userId: number, reason: string): Promise<void>;
  /** 请求账号列表（需要 ADMIN），结果走 onAccounts。 */
  listUsers(): Promise<void>;
  /** 改某个账号的权限位（需要 ADMIN）。 */
  setUserPerms(userId: number, perms: number): Promise<void>;
  /**
   * 忘掉这台服务器记住的证书指纹。
   *
   * TOFU 冲突（「指纹变了」）时这是唯一的出路——命令行里是 `--reset-trust`。
   */
  forgetTrust(addr: string): Promise<void>;
  selftest(kind: 'digital' | 'acoustic'): Promise<void>;
  subscribe(handlers: Handlers): () => void;
}

// ---------------------------------------------------------------- Mock

const HALL = 1;
const DUO = 2;
const DUO_A = 3;
const DUO_B = 4;
const IDLE = 5;
const MUSIC = 6;

const GROUP_COMMON = 10;
const GROUP_IDLE = 11;
const GROUP_EVENT = 12;
const EVENT_FRIDAY = 13;

// 分组用 flags & 8（FLAG_CONTAINER）：不可加入，只用来聚合。
const CHANNELS: Channel[] = [
  { id: GROUP_COMMON, parent: 0, name: '常用', position: 0, max_users: 0, flags: FLAG_CONTAINER, members: 0 },
  { id: HALL, parent: GROUP_COMMON, name: '大厅', position: 0, max_users: 0, flags: 0, members: 3 },
  { id: DUO, parent: GROUP_COMMON, name: '团战', position: 1, max_users: 0, flags: 0, members: 3 },
  { id: DUO_A, parent: DUO, name: '一队', position: 0, max_users: 0, flags: 0, members: 2 },
  { id: DUO_B, parent: DUO, name: '二队', position: 1, max_users: 0, flags: 0, members: 1 },
  { id: GROUP_IDLE, parent: 0, name: '挂机区', position: 1, max_users: 0, flags: FLAG_CONTAINER, members: 0 },
  { id: IDLE, parent: GROUP_IDLE, name: '挂机', position: 0, max_users: 0, flags: 0, members: 1 },
  { id: MUSIC, parent: GROUP_IDLE, name: '听歌', position: 1, max_users: 0, flags: 0, members: 0 },
  { id: GROUP_EVENT, parent: 0, name: '活动', position: 2, max_users: 0, flags: FLAG_CONTAINER, members: 2 },
  { id: EVENT_FRIDAY, parent: GROUP_EVENT, name: '周五局', position: 0, max_users: 0, flags: 0, members: 2 },
];

const SELF = 1;

/**
 * 浏览器里的假后端：让 `pnpm dev` 不开 Tauri 也能看界面、点交互。
 *
 * 它只模拟**契约**，不模拟音质；数字是为了让布局有真实密度，不是测量值。
 */
export class MockBackend implements Backend {
  #handlers: Handlers | null = null;
  #members: Member[] = [];
  #levels: Record<number, number> = {};
  #meters: Meters = { talking: false, self_level: 0, tx: false, gate: false, vad: 0, frames_sent: 0, chain_ms: 10, levels: {} };
  #frames = 0;
  #timers: ReturnType<typeof setInterval>[] = [];
  #voice: VoiceSettings = { mode: 'vad', threshold_db: -38, preroll_frames: 3 };
  #ptt = false;
  // 单独存一份：`Net.budget` / `Net.device` 是可选字段，展开前得有个确定的类型。
  #budget = { capture: 0, chain: 0.08, encode: 0.14, network: 0.3, jitter: 14, playout: 8 };
  #device = { digital_loopback_ms: 34.3, acoustic_loopback_ms: 175.3 };
  #net: Net = {
    jitter_ms: 14,
    jitter_target_ms: 20,
    jitter_max_ms: 60,
    loss: 0,
    rtt_p50: 0.6,
    rtt_p95: 0.8,
    underruns: 0,
    ring_drops: 0,
    kbps: 32,
    tick_hz: 100,
    budget: this.#budget,
    device: this.#device,
  };

  async connect(req: ConnectRequest): Promise<void> {
    this.#voice = req.voice;
    this.#handlers?.onPhase('authenticating');
    await sleep(280);
    this.#members = [
      { user_id: SELF, name: '小木', channel_id: HALL, mute: false, solo: false, server_mute: false, volume: 100 },
      { user_id: 2, name: '老K', channel_id: HALL, mute: false, solo: false, server_mute: false, volume: 100 },
      { user_id: 3, name: '阿岚', channel_id: HALL, mute: true, solo: false, server_mute: false, volume: 80 },
    ];
    // Mock 里自己是服主：这样评审时能看见管理面板与右键菜单。
    this.#handlers?.onPerms(511);
    this.#handlers?.onPhase('connected');
    this.#handlers?.onChannels(CHANNELS, 0);
    this.#handlers?.onMembers(this.#members, SELF);
    this.#handlers?.onNet(this.#net);
    this.#start();
    await this.joinChannel(HALL);
  }

  async disconnect(): Promise<void> {
    this.#stop();
    this.#handlers?.onPhase('disconnected');
  }

  async joinChannel(channelId: number): Promise<void> {
    this.#handlers?.onPhase('joining');
    await sleep(120);
    for (const m of this.#members) if (m.user_id === SELF) m.channel_id = channelId;
    this.#handlers?.onPhase('in_channel');
    this.#handlers?.onChannels(CHANNELS, channelId);
    this.#handlers?.onMembers(this.#members, SELF);
  }

  async setVoice(settings: VoiceSettings): Promise<void> {
    this.#voice = settings;
  }

  async setPtt(held: boolean): Promise<void> {
    this.#ptt = held;
  }

  async setSelf(state: { mute?: boolean; deafen?: boolean }): Promise<void> {
    for (const m of this.#members) if (m.user_id === SELF && state.mute !== undefined) m.mute = state.mute;
    this.#handlers?.onMembers(this.#members, SELF);
  }

  async setUser(userId: number, patch: { volume?: number; mute?: boolean; solo?: boolean }): Promise<void> {
    for (const m of this.#members) {
      if (m.user_id !== userId) continue;
      if (patch.volume !== undefined) m.volume = patch.volume;
      if (patch.mute !== undefined) m.mute = patch.mute;
      if (patch.solo !== undefined) m.solo = patch.solo;
    }
    this.#handlers?.onMembers(this.#members, SELF);
  }

  async setView(_view: View): Promise<void> {}

  // Mock 只模拟**契约**：这几个管理动作在浏览器里没有真服务器可打，
  // 所以只把本地那一份成员表改掉，让界面能看出「点下去有反应」。
  async moveUser(userId: number, channelId: number): Promise<void> {
    for (const m of this.#members) if (m.user_id === userId) m.channel_id = channelId;
    this.#handlers?.onMembers(this.#members, SELF);
  }

  async setUserState(userId: number, patch: { server_mute?: boolean; server_deaf?: boolean }): Promise<void> {
    for (const m of this.#members) {
      if (m.user_id === userId && patch.server_mute !== undefined) m.server_mute = patch.server_mute;
    }
    this.#handlers?.onMembers(this.#members, SELF);
  }

  async kickUser(userId: number, _reason: string): Promise<void> {
    this.#members = this.#members.filter((m) => m.user_id !== userId);
    this.#handlers?.onMembers(this.#members, SELF);
  }

  async listUsers(): Promise<void> {
    this.#handlers?.onAccounts(
      this.#members.map((m) => ({
        user_id: m.user_id,
        name: m.name,
        perms: m.user_id === SELF ? 511 : 1,
        anonymous: false,
        online: true,
      })),
    );
  }

  async setUserPerms(_userId: number, _perms: number): Promise<void> {}

  async forgetTrust(_addr: string): Promise<void> {}

  async selftest(_kind: 'digital' | 'acoustic'): Promise<void> {}

  subscribe(handlers: Handlers): () => void {
    this.#handlers = handlers;
    return () => {
      this.#handlers = null;
    };
  }

  #start(): void {
    this.#stop();
    // 20 Hz：电平与 VAD。
    this.#timers.push(
      setInterval(() => {
        const t = performance.now() / 1000;
        // 老K 每 6 s 说 3 s；自己按 0.3 的节奏“说话”。
        const kai = t % 6 < 3;
        this.#levels[2] = kai ? 0.45 + 0.3 * Math.abs(Math.sin(t * 3.1)) : 0;
        this.#levels[3] = 0;
        const self = 0.18 + 0.1 * Math.abs(Math.sin(t * 1.7));
        this.#levels[SELF] = self;
        // 假的 VAD：把电平映射成 0–1，让设置页的灵敏度条在浏览器里也会动。
        // 它不模拟音质，只模拟**契约**——门控的判据和真后端是同一套。
        const vad = Math.min(1, self * 2.4);
        const loud = 20 * Math.log10(Math.max(self, 1e-6));
        const gate =
          this.#voice.mode === 'open' ||
          (this.#voice.mode === 'ptt' ? this.#ptt : this.#ptt || (vad >= 0.5 && loud >= this.#voice.threshold_db));
        this.#meters = {
          ...this.#meters,
          self_level: self,
          talking: kai,
          vad,
          gate,
          tx: gate,
          levels: this.#levels,
        };
        if (this.#meters.tx) this.#frames += 1;
        this.#meters.frames_sent = this.#frames;
        this.#handlers?.onMeters(this.#meters);
      }, 50),
    );
    // 1 Hz：网络数字轻微抖动，让面板看起来是活的。
    this.#timers.push(
      setInterval(() => {
        const jitter = 14 + Math.sin(performance.now() / 9000) * 1.5;
        this.#budget = { ...this.#budget, jitter: Math.round(jitter * 10) / 10 };
        this.#net = {
          ...this.#net,
          jitter_ms: this.#budget.jitter,
          budget: this.#budget,
        };
        this.#handlers?.onNet(this.#net);
      }, 1000),
    );
  }

  #stop(): void {
    for (const t of this.#timers) clearInterval(t);
    this.#timers = [];
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
