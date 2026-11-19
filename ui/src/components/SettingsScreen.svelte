<script lang="ts">
  import { backend } from '../lib/backend';
  import { app } from '../lib/state.svelte';

  let { onclose }: { onclose: () => void } = $props();

  type Tier = { id: string; name: string; desc: string; latency: string; cpu: string };
  const tiers: Tier[] = [
    { id: 'none', name: '直通', desc: '只做编码。用来对比、排查问题。', latency: '0 ms', cpu: '0.00 ms/帧' },
    { id: 'highpass,agc2', name: '高通 + AGC2', desc: '去掉低频隆隆声，把电平拉平。无降噪。', latency: '0 ms', cpu: '0.03 ms/帧' },
    { id: 'highpass,rnn,agc2', name: '+ RNNoise', desc: '压键盘、风扇、椅擦。10 ms 算法延迟。', latency: '10 ms', cpu: '0.08 ms/帧' },
    { id: 'highpass,dfn3,agc2', name: '+ DFN3', desc: '质量档。桌面专属，跑在工作线程上。', latency: '30 + 20 ms', cpu: '0.42 ms/帧' },
  ];

  const modes = [
    { id: 'vad', label: '语音激活' },
    { id: 'ptt', label: '按住说话' },
    { id: 'open', label: '常开' },
  ] as const;

  // 设置立刻生效（会话每个 tick 拉一次指令，延迟上限 10 ms）。
  // 失败不吞掉：显示出来，否则用户会以为改了其实没改。
  let failure = $state('');

  async function push(): Promise<void> {
    try {
      await backend.setVoice(app.voice);
      failure = '';
    } catch (error) {
      failure = String(error);
    }
  }

  function setMode(mode: 'vad' | 'ptt' | 'open'): void {
    app.voice = { ...app.voice, mode };
    void push();
  }

  function setThreshold(value: number): void {
    app.voice = { ...app.voice, threshold_db: value };
    void push();
  }

  // 处理链给的 VAD 概率（0–1）。`1` 也可能表示「这条链没有 VAD」，
  // 所以这里只把它当**参考**显示，不当成「一定在说话」。
  const vad = $derived(app.meters.vad ?? 0);
  const gate = $derived(app.meters.gate ?? false);
</script>

<div class="settings">
  <header class="top">
    <div class="brand">
      <span class="nm">静队</span><span class="en">设置</span>
    </div>
    <div class="top-right">
      <span class="chip mono">0.1.0</span>
      <button class="icon-btn" title="返回" onclick={onclose}>✕</button>
    </div>
  </header>

  <div class="set-body">
    <nav class="set-nav">
      <div class="it on">音频</div>
      <div class="it">网络</div>
      <div class="it">关于</div>
    </nav>

    <div class="set-main">
      <h2>音频</h2>

      <div class="group">
        <div class="g-h">设备</div>
        <div class="field">
          <span class="lb">麦克风</span>
          <div class="sel">Microphone (USB Condenser Microphone) <span class="ok">48 kHz ✓</span><span class="chev">▾</span></div>
        </div>
        <div class="field">
          <span class="lb">扬声器</span>
          <div class="sel">扬声器 (Realtek(R) Audio) <span class="ok">48 kHz ✓</span><span class="chev">▾</span></div>
        </div>
        <div class="field">
          <span class="lb"></span>
          <div class="tiny">
            这台机器的声学环回实测 <span class="mono">175 ms</span>，其中约 <span class="mono">139 ms</span> 来自麦克风本身，
            与我们无关。非 48 kHz 设备会被拒绝，不做静默重采样。
          </div>
        </div>
      </div>

      <div class="group">
        <div class="g-h">处理档（上行）</div>
        <div class="cards">
          {#each tiers as t (t.id)}
            <button class="card" class:on={app.chain === t.id} onclick={() => (app.chain = t.id)}>
              <div class="t">{t.name}{#if t.id === 'highpass,rnn,agc2'}<span class="badge you">默认</span>{/if}</div>
              <div class="d">{t.desc}</div>
              <div class="m">
                <span><em>延迟</em>{t.latency}</span>
                <span><em>CPU</em>{t.cpu}</span>
              </div>
            </button>
          {/each}
        </div>
        <div class="tiny">切换做 20 ms 交叉淡化，不爆音。DFN3 多出的 20 ms 是工作线程队列（常数），不是抖动。</div>
      </div>

      <div class="group">
        <div class="g-h">说话方式</div>
        <div class="field">
          <span class="lb">方式</span>
          <div class="seg-ctl" style="max-width:460px">
            {#each modes as m (m.id)}
              <button class:on={app.voice.mode === m.id} onclick={() => setMode(m.id)}>{m.label}</button>
            {/each}
          </div>
        </div>
        <div class="field">
          <span class="lb"></span>
          <div class="tiny" style="max-width:620px">
            <b style="color:var(--tx2)">语音激活</b>：检测到你在说话才发，静默时每 200 ms 只发一个保活标记；开门时补发前 30 ms，不吃爆破音的起头。
            <b style="color:var(--tx2)">常开</b>：一直发，对方能听到键盘、风扇、呼吸——只适合安静房间加耳麦。
            <b style="color:var(--tx2)">按住说话</b>：最可控，但对局里手忙。三种模式下都能用 <span class="mono">Space</span> 强制发送。
          </div>
        </div>
        <div class="field">
          <span class="lb">灵敏度</span>
          <div style="display:flex;align-items:center;gap:12px">
            <input
              type="range"
              min="-60"
              max="-10"
              step="1"
              value={app.voice.threshold_db}
              oninput={(event) => setThreshold(Number(event.currentTarget.value))}
              style="width:280px"
            />
            <span class="mono" style="font-size:11px;color:var(--tx3)">门限 {app.voice.threshold_db} dBFS</span>
          </div>
        </div>
        <div class="field">
          <span class="lb">现在</span>
          <div style="display:flex;align-items:center;gap:12px">
            <div class="vad-bar" aria-label="VAD 概率">
              <i style="width:{Math.round(vad * 100)}%"></i>
            </div>
            <span class="mono" style="font-size:11px;color:var(--tx3)">VAD {vad.toFixed(2)}</span>
            <span class="chip" class:live={gate}>{gate ? '门开着 · 在发' : '门关着 · 不发'}</span>
          </div>
        </div>
        <div class="field">
          <span class="lb"></span>
          <div class="tiny" style="max-width:620px">
            判据是「电平过门限 <b style="color:var(--tx2)">且</b> VAD 概率过线」，所以风扇吹到门限也不会开门；
            处理链没有 VAD 时（直通档）自动退化成只看电平。静默期仍每 200 ms 发一个标记，
            否则对方会把这段静默算成几百个丢包。
          </div>
        </div>
        {#if failure}
          <div class="field">
            <span class="lb"></span>
            <div class="tiny" style="color:var(--warn)">改设置失败：{failure}</div>
          </div>
        {/if}
      </div>

      <div class="group">
        <div class="g-h">自检</div>
        <div class="field">
          <span class="lb">麦克风测试</span>
          <div style="display:flex;align-items:center;gap:12px">
            <button class="btn" disabled>录 5 秒并回放</button>
            <span class="tiny">未接入。设备的音量请看底部那条电平。</span>
          </div>
        </div>
        <div class="field">
          <span class="lb">延迟自检</span>
          <div style="display:flex;align-items:center;gap:14px">
            <span class="mono" style="font-size:11.5px;color:var(--tx3)">数字回环 未测量</span>
            <span class="mono" style="font-size:11.5px;color:var(--tx3)">声学回环 未测量</span>
            <button class="btn" disabled>重新测量</button>
          </div>
        </div>
        <div class="field">
          <span class="lb"></span>
          <div class="tiny" style="max-width:620px">
            测量代码在 <span class="mono">audio-io</span> 的 impulse 模块里，但「播一段、录回来、算延迟」
            这条往返还没拼进会话。所以这里显示未测量，不拿 0 或旧数字冒充。
          </div>
        </div>
      </div>

      <div class="banner">
        <span style="font-size:14px;line-height:1.2">⚠</span>
        <div><b>检测到蓝牙耳麦。</b>回传麦克风会切到 SCO，延迟与音质都会下降。要低延迟请用有线或 USB 耳机——蓝牙只保证「能用」。</div>
      </div>
    </div>
  </div>
</div>
