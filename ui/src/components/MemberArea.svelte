<script lang="ts">
  import { backend } from '../lib/backend';
  import { avatarClass, dbfs, initial, levelPct } from '../lib/format';
  import { menu, type MenuItem } from '../lib/menu.svelte';
  import { app, PERM } from '../lib/state.svelte';
  import type { Member } from '../lib/ipc';

  /** 灰掉一项：**显示但不可点**，并说明要什么权限（ADR-0016 §6.4）。 */
  function gated(label: string, allowed: boolean, run: () => void, hint = '需要管理员权限'): MenuItem {
    return { label, enabled: allowed, hint: allowed ? undefined : hint, run };
  }

  /** 成员右键菜单。按自己的权限位裁剪，但灰掉的项仍然显示。 */
  function memberMenu(event: MouseEvent, m: Member): void {
    event.preventDefault();
    const items: MenuItem[] = [];
    if (isSelf(m.user_id)) {
      items.push({ label: m.mute ? '取消静音' : '静音自己', run: () => backend.setSelf({ mute: !m.mute }) });
      items.push({ label: app.deafen ? '取消闭听' : '闭听', run: () => backend.setSelf({ deafen: !app.deafen }) });
    } else {
      // 本机设置：谁都能改，只影响自己听。
      items.push({ label: m.mute ? '取消静音（本机）' : '静音他（本机）', run: () => backend.setUser(m.user_id, { mute: !m.mute }) });
      items.push({ label: m.solo ? '取消独听' : '独听他', run: () => backend.setUser(m.user_id, { solo: !m.solo }) });
      items.push({ separator: true, label: '' });
      // 移到频道：不搞子菜单，直接平铺（频道就几个，子菜单要多一层悬停）。
      const canMove = app.can(PERM.MOVE_ANYONE);
      for (const channel of app.channels) {
        if (channel.flags & 8) continue;
        if (channel.id === m.channel_id) continue;
        items.push(
          gated(`移到 #${channel.name}`, canMove, () => backend.moveUser(m.user_id, channel.id), '需要「移动别人」权限'),
        );
      }
      items.push({ separator: true, label: '' });
      items.push(
        gated(
          m.server_mute ? '解除服务器静音' : '服务器静音',
          app.can(PERM.MUTE_OTHERS),
          () => backend.setUserState(m.user_id, { server_mute: !m.server_mute }),
          '需要「静音别人」权限',
        ),
      );
      items.push({
        ...gated('踢出服务器…', app.can(PERM.KICK), () => {
          const reason = window.prompt(`把 ${m.name} 踢出服务器。原因（会显示给他）：`, '');
          if (reason === null) return;
          void backend.kickUser(m.user_id, reason).catch(() => {});
        }, '需要「踢人」权限'),
        danger: true,
      });
      items.push({ separator: true, label: '' });
      items.push(
        gated('权限…', app.can(PERM.ADMIN), () => {
          app.accountsFocus = m.user_id;
          app.openSettings('server');
        }, '需要管理员权限'),
      );
    }
    menu.show(event, items);
  }
  const level = (id: number): number => app.meters.levels[id] ?? 0;
  const isSelf = (id: number): boolean => id === app.selfUserId;
  const talking = (id: number, mute: boolean): boolean => !mute && level(id) > 0.25;

  function subline(id: number, mute: boolean): string {
    if (isSelf(id)) return app.meters.tx ? `${dbfs(level(id))} · 发送中` : '上行 32 kbps';
    if (mute) return '静音';
    const lv = level(id);
    return lv > 0.02 ? dbfs(lv) : '在听';
  }
</script>

{#if app.view === 'grid'}
  <div class="tiles" role="list">
    {#each app.roomMembers as m (m.user_id)}
      <div
        role="listitem"
        class="tile"
        class:self={isSelf(m.user_id)}
        class:speaking={talking(m.user_id, m.mute)}
        class:muted={m.mute && !isSelf(m.user_id)}
        oncontextmenu={(event) => memberMenu(event, m)}
      >
        <div class="ava big {avatarClass(m.user_id)}">{initial(m.name)}</div>
        <div class="t-nm">
          {m.name}
          {#if isSelf(m.user_id)}<span class="badge you">你</span>{/if}
          {#if m.mute}<span class="badge mute">静音</span>{/if}
          {#if m.solo}<span class="badge solo">独听</span>{/if}
        </div>
        <div class="t-sub mono">{subline(m.user_id, m.mute)}</div>
        <div class="bar"><i style="width:{levelPct(level(m.user_id))}%"></i></div>
        <div class="t-ctl">
          {#if isSelf(m.user_id)}
            <button class="mini" class:on={m.mute} onclick={() => backend.setSelf({ mute: !m.mute })}>
              {m.mute ? '取消静音' : '静音自己'}
            </button>
            <button class="mini" onclick={() => backend.setSelf({ deafen: !app.deafen })}>闭听</button>
          {:else}
            <input
              type="range"
              min="0"
              max="200"
              value={m.volume}
              aria-label="{m.name} 音量"
              oninput={(e) => backend.setUser(m.user_id, { volume: Number(e.currentTarget.value) })}
            />
            <button class="mini" onclick={() => backend.setUser(m.user_id, { mute: !m.mute })}>
              {m.mute ? '取消静音' : '静音'}
            </button>
            <button class="mini" class:on={m.solo} onclick={() => backend.setUser(m.user_id, { solo: !m.solo })}>
              {m.solo ? '取消独听' : '独听'}
            </button>
          {/if}
        </div>
      </div>
    {/each}
  </div>
{:else}
  <div class="list" role="list">
    {#each app.roomMembers as m (m.user_id)}
      <div
        role="listitem"
        class="lrow"
        class:speaking={talking(m.user_id, m.mute)}
        oncontextmenu={(event) => memberMenu(event, m)}
      >
        <div class="ava md {avatarClass(m.user_id)}">{initial(m.name)}</div>
        <div class="nm">
          {m.name}
          {#if isSelf(m.user_id)}<span class="badge you">你</span>{/if}
          {#if m.mute}<span class="badge mute">静音</span>{/if}
          {#if m.solo}<span class="badge solo">独听</span>{/if}
        </div>
        <div class="lvl"><i style="width:{levelPct(level(m.user_id))}%"></i></div>
        <div class="vol">
          {#if !isSelf(m.user_id)}
            <input
              type="range"
              min="0"
              max="200"
              value={m.volume}
              aria-label="{m.name} 音量"
              oninput={(e) => backend.setUser(m.user_id, { volume: Number(e.currentTarget.value) })}
            />
            <span class="v mono">{m.volume}</span>
          {:else}
            <span class="v mono">—</span>
          {/if}
        </div>
        <div class="ops">
          {#if !isSelf(m.user_id)}
            <button class="mini" onclick={() => backend.setUser(m.user_id, { mute: !m.mute })}>
              {m.mute ? '取消静音' : '静音'}
            </button>
            <button class="mini" class:on={m.solo} onclick={() => backend.setUser(m.user_id, { solo: !m.solo })}>
              {m.solo ? '取消独听' : '独听'}
            </button>
          {/if}
        </div>
      </div>
    {/each}
  </div>
{/if}
