<script lang="ts">
  import { backend } from '../lib/backend';
  import { app } from '../lib/state.svelte';

  let busy = $state(false);

  async function connect(): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      await backend.connect({
        addr: app.addr,
        identity: { kind: 'anonymous' },
        // 首次连接：记住指纹。真实实现由 Rust 侧落盘（TOFU）。
        trust: { kind: 'remember', fingerprint: '3A:9F:2C:71:8D' },
      });
    } finally {
      busy = false;
    }
  }
</script>

<div class="connect">
  <div class="c-left">
    <svg width="46" height="46" viewBox="0 0 46 46" aria-hidden="true">
      <rect x="5" y="19" width="5" height="8" rx="2.5" fill="#2b6b4d" />
      <rect x="14" y="12" width="5" height="22" rx="2.5" fill="#5fe3a1" />
      <rect x="23" y="16" width="5" height="14" rx="2.5" fill="#5fe3a1" />
      <rect x="32" y="21" width="5" height="4" rx="2" fill="#2b6b4d" />
    </svg>
    <div>
      <div class="big">静队</div>
      <div class="sub">HUSHSQUAD · 小队语音</div>
    </div>
    <div class="pitch">只传你的声音，<br />不传你的房间。</div>
    <div class="hr"></div>
    <div class="facts">
      <div><i></i>10 ms 帧 · 软件预算 22.5 ms（本机实测）</div>
      <div><i></i>32 kbps 单声道 · ChaCha20-Poly1305</div>
      <div><i></i>每频道最多 32 人 · 按人音量与独听</div>
    </div>
    <div class="ver">v0.1.0 · Windows</div>
  </div>

  <div class="c-right">
    <h2>连接到服务器</h2>

    <label class="f">服务器地址
      <div class="inp"><input bind:value={app.addr} spellcheck="false" /></div>
    </label>

    <label class="f">身份
      <div class="seg-ctl">
        <button class="on">匿名（这台设备）</button>
        <button>账号</button>
      </div>
    </label>

    <div class="cert">
      <div class="h"><span class="dot ok"></span>首次连接这台服务器 · 记住它的指纹</div>
      <div class="fp">
        SHA-256 3A:9F:2C:71:8D:04:B6:E1:52:7C:9A:33:F0:6E:18:BD:<br />
        7B:22:C5:40:9D:31:8E:66:AF:10:47:58:D2:03:9C:4E
      </div>
      <div class="btns">
        <button class="btn">信任并记住</button>
        <button class="btn">不信任</button>
      </div>
    </div>
    <div class="tiny">指纹变了会直接拒绝连接，除非你在这里点「重新信任」。证书不写进系统钥匙串。</div>

    <button class="btn-primary" onclick={connect} disabled={busy}>
      {busy ? '连接中…' : '连接'}
    </button>

    <div class="recent">
      <div class="it"><span class="dot ok"></span>家里 · 局域网<span class="ad">hushsquad.lan:8443</span><span class="ms">14 ms</span></div>
      <div class="it"><span class="dot"></span>公司<span class="ad">hushsquad.internal:8443</span><span class="ms">—</span></div>
    </div>
  </div>
</div>
