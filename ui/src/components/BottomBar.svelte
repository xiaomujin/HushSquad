<script lang="ts">
  import { backend } from '../lib/backend';
  import { dbfs, levelPct } from '../lib/format';
  import { app } from '../lib/state.svelte';

  // 静音 / 闭听是服务器广播回来的状态（`Member.self_mute`），不是界面自己记的：
  // 之前这里读的是 `app.self?.mute`（本机把某人静音的偏好），对自己永远是 false，
  // 于是按钮点下去什么都不变，看起来像「没有恢复按钮」。
  const selfMute = $derived(app.selfMute);
  const deafen = $derived(app.deafen);
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
    <div class="mic-dev">48 kHz · 单声道 · 10 ms 帧</div>
  </div>

  <div class="ptt-wrap">
    <button
      class="ptt"
      class:on={sending}
      class:off={reconnecting || selfMute}
      onclick={() => backend.setSelf({ mute: !selfMute })}
    >
      <i class="vad"></i>{reconnecting ? modeLabel : selfMute ? '已静音' : sending ? '正在发送' : modeLabel}
    </button>
    {#if reconnecting}
      <div class="ptt-hint warn">重连中，暂不发送</div>
    {:else if selfMute}
      <div class="ptt-hint">静音优先于任何按键 · 点这里取消</div>
    {:else if app.voice.mode === 'ptt'}
      <!-- 按住说话：提示里给的是**真注册上的那个键**（全局热键，窗口没焦点也算数）。 -->
      <div class="ptt-hint" class:warn={!app.pttRegistered}>
        {#if app.pttRegistered}
          按住 <kbd>{app.pttKey}</kbd> 说话 · 松手就停
        {:else}
          按键没注册上（可能被别的程序占了）· 去设置里换一个
        {/if}
      </div>
    {:else if app.voice.mode === 'open'}
      <div class="ptt-hint">常开 · 一直在发，对方听得到你的键盘和风扇</div>
    {:else}
      <div class="ptt-hint">
        安静 · 门限 {app.voice.threshold_db} dBFS · 说话才发
      </div>
    {/if}
  </div>

  <div class="actions">
    <button
      class="act"
      class:on={selfMute}
      aria-pressed={selfMute}
      onclick={() => backend.setSelf({ mute: !selfMute })}
    >
      {selfMute ? '取消静音' : '静音'}
    </button>
    <button
      class="act"
      class:on={deafen}
      aria-pressed={deafen}
      onclick={() => backend.setSelf({ deafen: !deafen })}
    >
      {deafen ? '取消闭听' : '闭听'}
    </button>
  </div>
</footer>
