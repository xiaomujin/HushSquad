<script lang="ts">
  import BottomBar from './BottomBar.svelte';
  import ChannelTree from './ChannelTree.svelte';
  import MemberArea from './MemberArea.svelte';
  import NetworkPanel from './NetworkPanel.svelte';
  import { initial } from '../lib/format';
  import { roleOf } from '../lib/ipc';
  import { app } from '../lib/state.svelte';

  // 自己的角色。匿名账号只有「成员」——这也顺便说明了「为什么我看不到管理菜单」。
  const roleLabel = $derived(roleOf(app.perms)?.label ?? '自定义');

  let { onsettings }: { onsettings: () => void } = $props();

  const chainLabel = $derived(
    app.chain.includes('dfn3') ? '高通 + DFN3' : app.chain.includes('rnn') ? '高通 + RNNoise' : '高通 + AGC2',
  );
  const jitter = $derived(app.net ? Math.round(app.net.jitter_ms) : 0);
  const reconnecting = $derived(app.phase === 'reconnecting');
  const joining = $derived(app.phase === 'joining');
</script>

<div class="app" class:panel-open={app.panelOpen} class:reconnect={reconnecting}>
  <header class="top">
    <div class="brand">
      <svg width="18" height="18" viewBox="0 0 46 46" aria-hidden="true">
        <rect x="5" y="19" width="5" height="8" rx="2.5" fill="#2b6b4d" />
        <rect x="14" y="12" width="5" height="22" rx="2.5" fill="#5fe3a1" />
        <rect x="23" y="16" width="5" height="14" rx="2.5" fill="#5fe3a1" />
        <rect x="32" y="21" width="5" height="4" rx="2" fill="#2b6b4d" />
      </svg>
      <span class="nm">静队</span><span class="en">HushSquad</span>
    </div>
    <div class="top-mid">
      {#if reconnecting}
        <span class="chip warn"><span class="dot warn"></span>重连中</span>
      {:else if app.meters.tx}
        <span class="chip live"><span class="dot ok"></span>正在发送</span>
      {:else}
        <span class="chip"><span class="dot ok"></span>已连接</span>
      {/if}
      <span class="chip mono">{app.addr}</span>
    </div>
    <div class="top-right">
      <span class="chip mono" class:dim={!app.net}>RTT {app.net ? app.net.rtt_p50.toFixed(1) : '—'} ms</span>
      <button class="icon-btn" title="设置" onclick={onsettings}>⚙</button>
    </div>
  </header>

  {#if reconnecting}
    <div class="recon">
      <span class="t">与服务器的连接中断</span>
      <span class="sub">正在重连 · 你的声音现在传不出去</span>
      <div class="acts">
        <button class="btn">立即重试</button>
        <button class="btn">断开</button>
      </div>
    </div>
  {/if}

  <aside class="side">
    <div>
      <div class="side-h">服务器</div>
      <div class="server">
        <div class="s-nm">家里 · 局域网</div>
        <div class="s-ad mono">{app.addr}</div>
        <div class="s-mt">
          <span class="dot ok"></span>{joining ? '加入中' : '已连接'} · <span class="mono">TLS 1.3</span>
        </div>
      </div>
    </div>

    <div class="tree">
      <!-- 「频道」区头由 ChannelTree 自己渲染：区头要能右键新建、还能当根级拖拽落点，
           放在这里就得把菜单和拖拽状态往上提一层，没有好处。 -->
      <ChannelTree />
    </div>

    <div class="me">
      <div class="ava sm a1">{initial(app.self?.name ?? '')}</div>
      <div>
        <div class="me-nm">{app.self?.name ?? '我'}</div>
        <div class="me-sb">{roleLabel}</div>
      </div>
    </div>
  </aside>

  <main class="main">
    <div class="ch-head">
      <h1>{app.channelName || '大厅'}</h1>
      <div class="meta">
        <span class="chip">{app.roomMembers.length} 人</span>
        <span class="chip">{chainLabel}</span>
        <span class="chip mono">10 ms</span>
      </div>
      <button type="button" class="chip net" class:on={app.panelOpen} onclick={() => (app.panelOpen = !app.panelOpen)}>
        网络 {jitter} ms {app.panelOpen ? '▾' : '▸'}
      </button>
      <div class="viewtoggle">
        <button class:on={app.view === 'grid'} onclick={() => (app.view = 'grid')}>网格</button>
        <button class:on={app.view === 'list'} onclick={() => (app.view = 'list')}>列表</button>
      </div>
    </div>

    <MemberArea />
  </main>

  {#if app.panelOpen}
    <NetworkPanel />
  {/if}

  <BottomBar />
</div>
