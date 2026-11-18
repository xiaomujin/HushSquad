<script lang="ts">
  import ConnectScreen from './components/ConnectScreen.svelte';
  import RoomScreen from './components/RoomScreen.svelte';
  import SettingsScreen from './components/SettingsScreen.svelte';
  import { backend } from './lib/backend';
  import { app } from './lib/state.svelte';

  // 后端事件 → 镜像。低频事件走这里；高频电平也在同一条流上（Tauri 侧是 Channel<T>）。
  backend.subscribe({
    onPhase(phase, error) {
      app.phase = phase;
      app.error = error;
    },
    onChannels(channels, selfChannel) {
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
    onNet(net) {
      app.net = net;
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
      });
    }
  });
  let settingsOpen = $state(false);
  const inRoom = $derived(app.phase === 'in_channel' || app.phase === 'joining' || app.phase === 'reconnecting');
</script>

{#if settingsOpen}
  <SettingsScreen onclose={() => (settingsOpen = false)} />
{:else if inRoom}
  <RoomScreen onsettings={() => (settingsOpen = true)} />
{:else}
  <ConnectScreen />
{/if}
