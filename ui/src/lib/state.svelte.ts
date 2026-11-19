import { FLAG_CONTAINER, type Channel, type Member, type Meters, type Net, type Phase, type View, type VoiceSettings } from './ipc';

/** 前端持有的**唯一**一份镜像（ADR-0013：真相在 Rust，这里只是最近一次快照）。 */
class AppState {
  phase = $state<Phase>('disconnected');
  error = $state<string | undefined>(undefined);

  channels = $state<Channel[]>([]);
  members = $state<Member[]>([]);
  selfUserId = $state(0);
  selfChannel = $state(0);

  meters = $state<Meters>({ talking: false, self_level: 0, tx: false, gate: false, vad: 0, frames_sent: 0, chain_ms: 10, levels: {} });
  net = $state<Net | null>(null);

  view = $state<View>('grid');
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

  toggleCollapsed(id: number): void {
    this.collapsed = this.collapsed.includes(id)
      ? this.collapsed.filter((x) => x !== id)
      : [...this.collapsed, id];
  }

  isCollapsed(id: number): boolean {
    return this.collapsed.includes(id);
  }
}

export const app = new AppState();

export { FLAG_CONTAINER };
export type { Channel, Member, Meters, Net, Phase, View };
