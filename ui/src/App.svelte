<script lang="ts">
  import ConnectScreen from './components/ConnectScreen.svelte';
  import ContextMenu from './components/ContextMenu.svelte';
  import RoomScreen from './components/RoomScreen.svelte';
  import SettingsScreen from './components/SettingsScreen.svelte';
  import { backend } from './lib/backend';
  import { app } from './lib/state.svelte';

  /** 把 Rust 侧的按键状态读回来。注册与否只有那边知道（它掌握说话方式与会话）。 */
  async function refreshPtt(): Promise<void> {
    try {
      const settings = await backend.pttSettings();
      app.pttKey = settings.key;
      app.pttHasModifier = settings.has_modifier;
      app.pttRegistered = settings.registered;
    } catch {
      // 读不到就当没有：界面上不显示注册状态，不影响说话。
    }
  }

  // 后端事件 → 镜像。低频事件走这里；高频电平也在同一条流上（Tauri 侧是 Channel<T>）。
  backend.subscribe({
    onPhase(phase, error) {
      app.phase = phase;
      app.error = error;
      // 断线后本机的静音/闭听不会保留（新会话从「开麦、不闭听」开始），
      // 界面跟着复位，否则按钮会显示一个已经不存在的状态。
      if (phase === 'disconnected' || phase === 'error') {
        app.selfMute = false;
        app.deafen = false;
        // 会话没了，Rust 侧会把全局热键注销掉（见 apps/app/src/ptt.rs）。
        app.pttRegistered = false;
      } else if (phase === 'in_channel') {
        // 连上了才会注册按键：按住说话需要一条活着的会话。
        void refreshPtt();
      }
    },
    onChannels(channels, selfChannel) {
      // 服务器给了新快照：以它为准。丢掉拖拽的覆盖层，这就是「回滚」的全部。
      app.dropPendingTree();
      app.channels = channels;
      app.selfChannel = selfChannel;
    },
    onMembers(members, selfUserId) {
      app.members = members;
      app.selfUserId = selfUserId;
      const me = members.find((m) => m.user_id === selfUserId);
      if (me) {
        // 静音 / 闭听是**服务器广播回来的**（`SetSelfState` → `UserUpdated`），
        // 不是本机自己记的：这样别人也看得见你静音了，两边不会各说各话。
        // 读 `self_mute` 而不是 `mute`——后者是「我这边不播他的声音」，对自己没意义。
        app.selfMute = me.self_mute;
        app.deafen = me.self_deaf;
      }
    },
    onMeters(meters) {
      app.meters = meters;
    },
    onPerms(perms) {
      app.perms = perms;
    },
    onAccounts(accounts) {
      app.accounts = accounts;
    },
    onNet(net) {
      app.net = net;
    },
    onNotice(message) {
      // 服务器拒绝了一次操作：提示一下就好，连接和语音都不受影响。
      // 顺手丢掉拖拽的覆盖层——拒绝意味着「你以为的顺序」不成立，回到服务器的样子。
      app.dropPendingTree();
      app.notify(message);
    },
  });

  // 开发/评审用：?autoconnect=1&view=list&panel=1 直接把界面推进到房间状态。
  // 只影响浏览器里的 Mock，Tauri 侧不会读到这些参数。
  $effect(() => {
    const q = new URLSearchParams(location.search);
    if (q.get('view') === 'list') app.view = 'list';
    if (q.get('panel') === '1') app.panelOpen = true;
    const collapse = q.get('collapse');
    if (collapse) app.collapsed = collapse.split(',').map(Number);
    if (q.get('autoconnect') === '1') {
      void backend.connect({
        addr: app.addr,
        identity: { kind: 'anonymous' },
        trust: { kind: 'remember', fingerprint: 'dev' },
        voice: app.voice,
      });
    }
  });
  const inRoom = $derived(app.phase === 'in_channel' || app.phase === 'joining' || app.phase === 'reconnecting');

  // 按键说话（B1）：界面**不监听键盘**。
  //
  // 窗口没焦点时（人在游戏里）前端根本收不到 keydown，而那正是按住说话要解决的场景。
  // 所以按键的注册与「按下 / 松开」都在 Rust 侧（`apps/app/src/ptt.rs` 的全局热键），
  // 这里只把当前绑定的键读回来显示。顺带一提，原来那段「任何模式下 Space 都强制发送」
  // 的监听已经删掉——按键现在只属于按住说话，见 `audio_core::gate` 的模块文档。
  $effect(() => {
    void refreshPtt();
  });
</script>

{#if app.settingsOpen}
  <SettingsScreen onclose={() => (app.settingsOpen = false)} />
{:else if inRoom}
  <RoomScreen onsettings={() => app.openSettings('audio')} />
{:else}
  <ConnectScreen />
{/if}

<ContextMenu />

{#if app.notice}
  <!-- 非致命提示：浮在底部中央，3 秒后自己消失（计时在 app.notify 里）。
       `role="status"` 让读屏软件念出来，但不用抢焦点。 -->
  <div class="toast" role="status">{app.notice}</div>
{/if}
