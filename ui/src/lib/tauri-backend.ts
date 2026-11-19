// Tauri 壳里的真后端：把 `ui/src/lib/ipc.ts` 的 `Backend` 接口接到 Rust 上。
//
// 这里只做三件事：命令转发、事件分派、**字段改名**（Rust 用 `_ms` 后缀，界面不用）。
// 任何业务逻辑都不该长在这一层——它一长，就说明 Rust 侧漏了东西。

import { Channel, invoke } from '@tauri-apps/api/core';
import type {
  Backend,
  Channel as ChannelInfo,
  ConnectRequest,
  Handlers,
  Member,
  Meters,
  Net,
  Phase,
  View,
  VoiceSettings,
} from './ipc';

// ---------------------------------------------------------------- 线上类型
//
// 对应 `apps/client/src/host.rs` 的 `StateView` / `MetersView` / `NetView`，
// 以及 `apps/app/src/session.rs` 的 `Event`（内部标记 `kind`）。

interface WireState {
  phase: Phase;
  error: string | null;
  self_user_id: number;
  self_channel: number;
  muted: boolean;
  deafened: boolean;
  channels: ChannelInfo[];
  members: Member[];
}

interface WireNet {
  jitter_ms: number;
  jitter_target_ms: number;
  jitter_max_ms: number;
  loss: number;
  rtt_p50_ms: number;
  rtt_p95_ms: number;
  underruns: number;
  ring_drops: number;
  kbps: number;
  tick_hz: number;
}

type WireEvent =
  | ({ kind: 'state' } & WireState)
  | ({ kind: 'meters' } & Meters)
  | ({ kind: 'net' } & WireNet);

/** 真后端。 */
export class TauriBackend implements Backend {
  #handlers: Handlers | null = null;
  #events: Channel<WireEvent> | null = null;

  async connect(req: ConnectRequest): Promise<void> {
    // 重新连接时先把上一条通道摘掉：旧会话已经停了，它的推送不该再进界面。
    if (this.#events) {
      this.#events.onmessage = () => {};
      this.#events = null;
    }
    this.#handlers?.onPhase('authenticating');
    const events = new Channel<WireEvent>();
    events.onmessage = (event) => this.#dispatch(event);
    this.#events = events;
    try {
      // `trust` 不传：v1 的信任策略是 TOFU（第一次记住指纹、变了就拒绝），
      // 由 Rust 侧落盘，界面上还没有「手抄指纹」这一步。
      await invoke('connect', {
        request: { addr: req.addr, identity: req.identity, voice: req.voice },
        events,
      });
    } catch (error) {
      this.#handlers?.onPhase('error', String(error));
      throw error;
    }
  }

  async disconnect(): Promise<void> {
    await invoke('disconnect');
    this.#events = null;
    this.#handlers?.onPhase('disconnected');
  }

  async joinChannel(channelId: number): Promise<void> {
    await invoke('join_channel', { channelId });
  }

  /**
   * 上行语音模式：真的改门控（`audio_core::TxGate`）。
   *
   * 它只改**本机**的发包时机，与服务器无关；连接之前改也有效（值会跟着
   * `connect` 的 `voice` 一起下去）。
   */
  async setVoice(settings: VoiceSettings): Promise<void> {
    await invoke('set_voice', {
      mode: settings.mode,
      thresholdDb: settings.threshold_db,
      prerollFrames: settings.preroll_frames,
    });
  }

  async setPtt(held: boolean): Promise<void> {
    await invoke('set_ptt', { held });
  }

  async setSelf(state: { mute?: boolean; deafen?: boolean }): Promise<void> {
    await invoke('set_self', { mute: state.mute, deafen: state.deafen });
  }

  async setUser(
    userId: number,
    patch: { volume?: number; mute?: boolean; solo?: boolean },
  ): Promise<void> {
    await invoke('set_user', {
      userId,
      volume: patch.volume,
      mute: patch.mute,
      solo: patch.solo,
    });
  }

  /** 视图偏好是纯界面状态，不进 Rust。 */
  async setView(_view: View): Promise<void> {}

  async forgetTrust(addr: string): Promise<void> {
    await invoke('forget_trust', { addr });
  }

  async selftest(kind: 'digital' | 'acoustic'): Promise<void> {
    // Rust 侧如实返回一条错误（自检链路还没拼进会话），由调用方显示出来。
    await invoke('selftest', { kind });
  }

  subscribe(handlers: Handlers): () => void {
    this.#handlers = handlers;
    return () => {
      this.#handlers = null;
    };
  }

  #dispatch(event: WireEvent): void {
    const handlers = this.#handlers;
    if (!handlers) return;
    switch (event.kind) {
      case 'state':
        handlers.onPhase(event.phase, event.error ?? undefined);
        handlers.onChannels(event.channels, event.self_channel);
        handlers.onMembers(event.members, event.self_user_id);
        break;
      case 'meters':
        handlers.onMeters(event);
        break;
      case 'net':
        handlers.onNet(toNet(event));
        break;
    }
  }
}

/** Rust 的 `NetView` → 界面的 `Net`（改名 + 舍入）。 */
function toNet(wire: WireNet): Net {
  return {
    jitter_ms: wire.jitter_ms,
    jitter_target_ms: wire.jitter_target_ms,
    jitter_max_ms: wire.jitter_max_ms,
    loss: wire.loss,
    rtt_p50: wire.rtt_p50_ms,
    rtt_p95: wire.rtt_p95_ms,
    underruns: wire.underruns,
    ring_drops: wire.ring_drops,
    kbps: Math.round(wire.kbps * 10) / 10,
    tick_hz: wire.tick_hz,
  };
}
