import { FLAG_CONTAINER, PERM, type Account, type Channel, type Member, type Meters, type Net, type Phase, type View, type VoiceSettings } from './ipc';

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
  chain = $state('highpass,rnn,agc2');
  addr = $state('127.0.0.1:64738');

  /** 折叠的分组 / 有子节点的频道 id。 */
  collapsed = $state<number[]>([]);

  self = $derived(this.members.find((m) => m.user_id === this.selfUserId));
  channelName = $derived(this.channels.find((c) => c.id === this.selfChannel)?.name ?? '');
  /** 在房间里看到的成员：本频道的人（v1 只听本频道）。 */
  roomMembers = $derived(this.members.filter((m) => m.channel_id === this.selfChannel));

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
