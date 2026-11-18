<script lang="ts">
  import { backend } from '../lib/backend';
  import { dbfs, levelPct } from '../lib/format';
  import { app } from '../lib/state.svelte';

  const selfMute = $derived(app.self?.mute ?? false);
  const sending = $derived(app.meters.tx && !selfMute);
  const reconnecting = $derived(app.phase === 'reconnecting');

  const modeLabel = $derived(
    app.voice.mode === 'vad' ? '语音激活' : app.voice.mode === 'ptt' ? '按住说话' : '常开',
  );
</script>

<footer class="bottom">
  <div class="mic">
    <div class="mic-h">
      麦克风 <span class="lv mono">{dbfs(app.meters.self_level)}</span>
    </div>
    <div class="seg"><i style="width:{levelPct(app.meters.self_level)}%"></i></div>
    <div class="mic-dev">Microphone (USB Condenser Microphone) · 48 kHz</div>
  </div>

  <div class="ptt-wrap">
    <button
      class="ptt"
      class:on={sending}
      class:off={reconnecting || selfMute}
      onclick={() => backend.setUser(app.selfUserId, { mute: !selfMute })}
    >
      <i class="vad"></i>{reconnecting ? modeLabel : selfMute ? '已静音' : sending ? '正在发送' : modeLabel}
    </button>
    {#if reconnecting}
      <div class="ptt-hint warn">重连中，暂不发送</div>
    {:else if selfMute}
      <div class="ptt-hint">静音优先于任何按键 · 点这里取消</div>
    {:else}
      <div class="ptt-hint">
        安静 · 门限 {app.voice.threshold_db} dBFS · 按住 <kbd>Space</kbd> 强制发送
      </div>
    {/if}
  </div>

  <div class="actions">
    <button class="act" class:on={selfMute} onclick={() => backend.setUser(app.selfUserId, { mute: !selfMute })}>
      静音
    </button>
    <button class="act" class:on={app.deafen} onclick={() => backend.setSelf({ deafen: !app.deafen })}>闭听</button>
  </div>
</footer>
