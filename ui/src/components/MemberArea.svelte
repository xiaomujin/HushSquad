<script lang="ts">
  import { backend } from '../lib/backend';
  import { avatarClass, dbfs, initial, levelPct } from '../lib/format';
  import { app } from '../lib/state.svelte';

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
  <div class="tiles">
    {#each app.roomMembers as m (m.user_id)}
      <div
        class="tile"
        class:self={isSelf(m.user_id)}
        class:speaking={talking(m.user_id, m.mute)}
        class:muted={m.mute && !isSelf(m.user_id)}
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
  <div class="list">
    {#each app.roomMembers as m (m.user_id)}
      <div class="lrow" class:speaking={talking(m.user_id, m.mute)}>
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
