<script lang="ts">
  import { backend } from '../lib/backend';
  import { app } from '../lib/state.svelte';

  let busy = $state(false);
  let failure = $state('');
  let info = $state('');
  /** 用账号登录（管理员必须有账号：匿名账号拿不到权限）。 */
  let account = $state(false);
  let user = $state('');
  let password = $state('');

  // 握手在后台线程上做：`busy` 只覆盖「把命令发出去」这一瞬间，
  // 真正在等的是阶段变成 authenticating 之后、connected 之前。
  const connecting = $derived(busy || app.phase === 'authenticating');

  async function connect(): Promise<void> {
    if (connecting) return;
    busy = true;
    failure = '';
    info = '';
    try {
      await backend.connect({
        addr: app.addr,
        identity: account
          ? { kind: 'account', user: user.trim(), password }
          : { kind: 'anonymous' },
        // 信任策略由 Rust 侧的 TOFU 决定：第一次记住指纹，变了就拒绝。
        trust: { kind: 'remember', fingerprint: 'tofu' },
        // 语音设置跟着连接一起下去：界面一进来就是语音激活，不用等用户去设置页点一下。
        voice: app.voice,
      });
      // 密码不留在内存里：下次连接要么从设备 token 免密，要么重新输。
      password = '';
    } catch (error) {
      failure = String(error);
    } finally {
      busy = false;
    }
  }

  async function forget(): Promise<void> {
    failure = '';
    info = '';
    try {
      await backend.forgetTrust(app.addr);
      info = '已忘掉这台服务器的指纹，再点一次「连接」会重新记住新的。';
    } catch (error) {
      failure = String(error);
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
        <button class:on={!account} onclick={() => (account = false)}>匿名（这台设备）</button>
        <button class:on={account} onclick={() => (account = true)}>账号</button>
      </div>
    </label>

    {#if account}
      <label class="f">用户名
        <div class="inp"><input bind:value={user} spellcheck="false" autocomplete="username" /></div>
      </label>
      <label class="f">密码
        <div class="inp">
          <input
            type="password"
            bind:value={password}
            autocomplete="current-password"
            onkeydown={(event) => event.key === 'Enter' && connect()}
          />
        </div>
      </label>
      <div class="tiny" style="margin-top:-4px">
        账号由服务器管理员用 <span class="mono">hushsquad-server adduser</span> 创建。
        密码只走 TLS，服务器存的是 argon2id 哈希。首次登录成功后服务器会下发一个设备
        token，下次自动免密。
      </div>
    {:else}
      <div class="tiny" style="margin-top:-4px">
        按这台设备的设备号建号，不需要密码。权限最低（只能进频道）——
        要让别人能管频道，得让管理员给你分配角色。
      </div>
    {/if}

    <div class="cert">
      <div class="h"><span class="dot ok"></span>首次连接这台服务器 · 会记住它的指纹</div>
      <div class="tiny">
        连上之后由 Rust 侧把证书指纹存进本地库；下次指纹变了会直接拒绝并提示你确认。
        这里不预先显示指纹——连之前它还不存在。
      </div>
    </div>

    {#if app.phase === 'error' && app.error}
      <div class="note warn">{app.error}</div>
    {/if}
    {#if failure}
      <div class="note warn">{failure}</div>
    {/if}
    {#if info}
      <div class="note">{info}</div>
    {/if}

    <button class="btn-primary" onclick={connect} disabled={connecting}>
      {connecting ? '连接中…' : '连接'}
    </button>

    {#if failure || app.phase === 'error'}
      <div class="btns">
        <button class="btn" onclick={forget}>忘掉这台服务器的指纹并重试</button>
      </div>
    {/if}
  </div>
</div>
