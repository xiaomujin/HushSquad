<script lang="ts">
  import { backend } from '../lib/backend';
  import { app } from '../lib/state.svelte';

  let selftestError = $state('');

  // ADR-0010：承诺值只含软件预算；设备段单列。
  // 真后端**给不出**预算分解（要跑一次自检），这时是 `null` —— 不拿 0 冒充。
  const budget = $derived(app.net?.budget ?? null);
  const device = $derived(app.net?.device ?? null);
  const jitter = $derived(app.net?.jitter_ms ?? 0);

  const total = $derived(
    budget
      ? budget.capture + budget.chain + budget.encode + budget.network + budget.jitter + budget.playout
      : 0,
  );

  const rows = $derived(
    budget
      ? [
          { key: '采集缓冲', value: budget.capture, color: '#4a5b6b' },
          { key: '前处理', value: budget.chain, color: '#5c7f6f' },
          { key: '编码', value: budget.encode, color: '#7b93a8' },
          { key: '网络单程', value: budget.network, color: '#8aa4bb' },
          { key: '抖动缓冲', value: budget.jitter, color: '#5fe3a1' },
          { key: '播放缓冲', value: budget.playout, color: '#3fbd83' },
        ]
      : [],
  );

  // 预算区间 15–20 ms 映射到 0–60 ms 的仪表盘上。
  const bandLeft = (15 / 60) * 100;
  const bandRight = 100 - (20 / 60) * 100;
  const nowLeft = $derived(Math.min(100, Math.max(0, (jitter / 60) * 100)));

  async function selftest(kind: 'digital' | 'acoustic'): Promise<void> {
    selftestError = '';
    try {
      await backend.selftest(kind);
    } catch (error) {
      // 自检链路还没接进会话（Rust 侧如实报错），把原因原样显示出来。
      selftestError = String(error);
    }
  }
</script>

<aside class="panel">
  <div class="p-head">这条链路 <span class="tag">承诺口径</span></div>
  <div class="p-body">
    {#if budget}
      <div class="p-sum"><b>{total.toFixed(1)}</b><span>ms 软件合计</span></div>

      <div class="stack" title="按比例：抖动缓冲与播放缓冲占绝大部分">
        {#each rows as r (r.key)}
          <i style="width:{Math.max(1, (r.value / Math.max(total, 0.001)) * 100)}%;background:{r.color}"></i>
        {/each}
      </div>

      <div class="rows">
        {#each rows as r (r.key)}
          <div class="row">
            <span class="sw" style="background:{r.color}"></span>
            <span class="k">{r.key}</span>
            <span class="v">{r.value.toFixed(2)}</span>
          </div>
        {/each}
      </div>

      <div class="note">
        <b>设备段另计。</b>本机声学回环实测 <span class="mono"
          >{device?.acoustic_loopback_ms.toFixed(1) ?? '—'} ms</span
        >，其中约 <span class="mono">139 ms</span> 是这只麦克风本身。它不是我们的处理链，所以不进上面的承诺值（ADR-0010）。
      </div>
    {:else}
      <div class="note">
        <b>预算分解未测量。</b>要跑一次自检（播一段、录回来、算延迟）才有这一栏。在那之前这里不显示数字——0 会被当成「没有延迟」。
      </div>
    {/if}

    <div class="p-sec">实时</div>
    <div class="rows">
      <div class="row"><span class="k">抖动缓冲</span><span class="v">{jitter.toFixed(0)} ms</span></div>
      <div class="gauge">
        <span class="band" style="left:{bandLeft}%;right:{bandRight}%"></span>
        <span class="now" style="left:{nowLeft}%"></span>
      </div>
      <div class="row"><span class="k">预算 15–20 · 上限 60</span><span class="v dim">绿区 = 预算</span></div>
      <div class="row"><span class="k">丢包</span><span class="v">{app.net?.loss ?? 0} 个</span></div>
      <div class="row">
        <span class="k">RTT p50 / p95</span>
        <span class="v">{(app.net?.rtt_p50 ?? 0).toFixed(1)} / {(app.net?.rtt_p95 ?? 0).toFixed(1)}</span>
      </div>
      <div class="row"><span class="k">上行码率</span><span class="v">{app.net?.kbps ?? 0} kbps</span></div>
      <div class="row">
        <span class="k">欠载 / 环满丢</span>
        <span class="v">{app.net?.underruns ?? 0} / {app.net?.ring_drops ?? '—'}</span>
      </div>
      <div class="row">
        <span class="k">tick 频率</span>
        <span class="v">{app.net?.tick_hz ? app.net.tick_hz.toFixed(0) : '—'} Hz</span>
      </div>
    </div>

    <div class="p-sec">自检</div>
    <div class="rows">
      <div class="row">
        <span class="k">数字回环</span>
        <span class="v">{device ? device.digital_loopback_ms.toFixed(1) + ' ms' : '—'}</span>
      </div>
      <div class="row">
        <span class="k">声学回环</span>
        <span class="v">{device ? device.acoustic_loopback_ms.toFixed(1) + ' ms' : '—'}</span>
      </div>
    </div>
    <div class="btns">
      <button class="btn" onclick={() => selftest('digital')}>数字回环</button>
      <button class="btn" onclick={() => selftest('acoustic')}>重新测量（会出声）</button>
    </div>
    {#if selftestError}
      <div class="note warn">{selftestError}</div>
    {/if}
  </div>
</aside>
