import { FLAG_CONTAINER, PERM, type Account, type Channel, type Member, type Meters, type Net, type Phase, type View, type VoiceSettings } from './ipc';
import { applyPending, type PendingMove } from './tree';

/** 前端持有的**唯一**一份镜像（ADR-0013：真相在 Rust，这里只是最近一次快照）。 */
class AppState {
  phase = $state<Phase>('disconnected');
  error = $state<string | undefined>(undefined);

  channels = $state<Channel[]>([]);
  members = $state<Member[]>([]);
  selfUserId = $state(0);
  /** 自己的权限位图（ADR-0016）。只用来决定界面，不当安全边界。 */
  perms = $state(0);
  /** 管理面板拿到的账号列表。 */
  accounts = $state<Account[]>([]);
  selfChannel = $state(0);

  meters = $state<Meters>({ talking: false, self_level: 0, tx: false, gate: false, vad: 0, frames_sent: 0, chain_ms: 10, levels: {} });
  net = $state<Net | null>(null);

  view = $state<View>('grid');
  /** 设置页开着吗，以及开在哪一页（成员右键「权限…」要能直接跳到服务器页）。 */
  settingsOpen = $state(false);
  settingsTab = $state<'audio' | 'server'>('audio');
  /** 权限面板要定位到的账号（0 = 不定位）。 */
  accountsFocus = $state(0);
  panelOpen = $state(false);
  selfMute = $state(false);
  deafen = $state(false);

  // 3 帧 = 30 ms。再多就要拿延迟换（补发是一次性突发，会挤爆对方的抖动缓冲），
  // 见 ADR-0015。
  voice = $state<VoiceSettings>({ mode: 'vad', threshold_db: -38, preroll_frames: 3 });
  /**
   * 按键说话的键（显示写法）。
   *
   * 真机上的真相在 Rust 侧（`apps/app/src/ptt.rs`）：它负责注册全局热键、落库、
   * 换模式时注销。这里只是最近一次读到的镜像，改键走 `backend.setPttKey`。
   */
  pttKey = $state('Ctrl+Shift+Space');
  /** 这个键有没有修饰键。没有就意味着它在所有程序里都没了。 */
  pttHasModifier = $state(true);
  /** 现在注册着吗（= 按住说话模式 + 已连接）。 */
  pttRegistered = $state(false);
  chain = $state('highpass,rnn,agc2');
  addr = $state('127.0.0.1:64738');

  /** 折叠的分组 / 有子节点的频道 id。 */
  collapsed = $state<number[]>([]);

  /**
   * 拖拽的乐观覆盖层：`null` = 没有覆盖层。
   *
   * 它是**可丢弃的补丁**，不是频道表的一部分：`channels` 永远只由服务器快照改，
   * 收到快照或收到拒绝（`onNotice`）就整块丢掉，界面自动回到服务器的样子。
   */
  pendingTree = $state<PendingMove[] | null>(null);

  /** 底部提示条。非致命：几秒后自己消失，不需要用户点。 */
  notice = $state<string | undefined>(undefined);
  /**
   * 提示的序号，每来一条加一。
   *
   * 对话框靠它判断「这条提示是不是我这次提交引来的」——只比字符串的话，连着两次
   * 同样的失败（改回原名再提交）会被当成旧提示忽略掉。
   */
  noticeSeq = $state(0);

  /** 频道树实际渲染用的表：镜像 + 覆盖层。 */
  treeChannels = $derived(applyPending(this.channels, this.pendingTree));

  #noticeTimer: ReturnType<typeof setTimeout> | undefined;
  #overlayTimer: ReturnType<typeof setTimeout> | undefined;

  self = $derived(this.members.find((m) => m.user_id === this.selfUserId));
  channelName = $derived(this.channels.find((c) => c.id === this.selfChannel)?.name ?? '');
  /** 在房间里看到的成员：本频道的人（v1 只听本频道）。 */
  roomMembers = $derived(this.members.filter((m) => m.channel_id === this.selfChannel));

  /** 一条非致命提示：3 秒后自己消失（后一条覆盖前一条，不排队）。 */
  notify(message: string): void {
    clearTimeout(this.#noticeTimer);
    this.notice = message;
    this.noticeSeq += 1;
    this.#noticeTimer = setTimeout(() => {
      this.notice = undefined;
    }, 3000);
  }

  /**
   * 盖上拖拽覆盖层，并起一个 3 秒的兜底计时器。
   *
   * 兜底的理由：服务器既不广播也不拒绝时（掉线、请求丢了），界面不能永远停在
   * 一个假的顺序上——到点就把覆盖层丢掉，并告诉用户没等到回应。
   */
  setPendingMove(move: PendingMove): void {
    clearTimeout(this.#overlayTimer);
    this.pendingTree = [move];
    this.#overlayTimer = setTimeout(() => {
      if (this.pendingTree === null) return;
      this.pendingTree = null;
      this.notify('服务器没有响应');
    }, 3000);
  }

  /** 丢掉覆盖层（收到快照 / 收到拒绝 / 请求本身失败时都调它）。 */
  dropPendingTree(): void {
    clearTimeout(this.#overlayTimer);
    this.pendingTree = null;
  }

  /**
   * 把底部那条提示收回去。
   *
   * 给对话框用：它已经把失败原因用红字显示在自己身上了，同一句话在屏幕上出现两次
   * 只会让人以为自己点重了。
   */
  dismissNotice(): void {
    clearTimeout(this.#noticeTimer);
    this.notice = undefined;
  }

  openSettings(tab: 'audio' | 'server' = 'audio'): void {
    this.settingsTab = tab;
    this.settingsOpen = true;
  }

  toggleCollapsed(id: number): void {
    this.collapsed = this.collapsed.includes(id)
      ? this.collapsed.filter((x) => x !== id)
      : [...this.collapsed, id];
  }

  /** 有没有这些位里的**全部**（与服务器同一条判据）。 */
  can(mask: number): boolean {
    return (this.perms & mask) === mask;
  }

  /** 至少有一位。 */
  canAny(mask: number): boolean {
    return (this.perms & mask) !== 0;
  }

  isCollapsed(id: number): boolean {
    return this.collapsed.includes(id);
  }
}

export const app = new AppState();

export { FLAG_CONTAINER, PERM };
export type { Account, Channel, Member, Meters, Net, Phase, View };
