<script lang="ts">
  import ConnectScreen from './components/ConnectScreen.svelte';
  import ContextMenu from './components/ContextMenu.svelte';
  import RoomScreen from './components/RoomScreen.svelte';
  import SettingsScreen from './components/SettingsScreen.svelte';
  import { backend } from './lib/backend';
  import { app } from './lib/state.svelte';

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
      if (me) app.selfMute = me.mute;
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

  // Space = 强制发送（三种说话方式下都有效，这是设置页给用户的承诺）。
  //
  // 这里**抢走**了空格：焦点在按钮上时空格不再「按下那个按钮」。语音软件里这是通行
  // 做法（按一下空格就把整个房间的按键节奏打乱更糟），但代价要说清楚：只用键盘的人
  // 只能用 Enter 触发按钮。输入框里打字不受影响。
  $effect(() => {
    const typing = (target: EventTarget | null): boolean =>
      target instanceof HTMLElement && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName);
    const down = (event: KeyboardEvent): void => {
      if (event.code !== 'Space' || event.repeat || typing(event.target)) return;
      event.preventDefault();
      if (!inRoom) return;
      // 没连上时后端会报「还没有连接」；这不是错误，用户只是还没进房间。
      void backend.setPtt(true).catch(() => {});
    };
    const up = (event: KeyboardEvent): void => {
      if (event.code !== 'Space' || typing(event.target)) return;
      event.preventDefault();
      if (!inRoom) return;
      void backend.setPtt(false).catch(() => {});
    };
    window.addEventListener('keydown', down);
    window.addEventListener('keyup', up);
    return () => {
      window.removeEventListener('keydown', down);
      window.removeEventListener('keyup', up);
    };
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
