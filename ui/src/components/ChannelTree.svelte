<script lang="ts">
  import { backend } from '../lib/backend';
  import { app } from '../lib/state.svelte';
  import { buildTree } from '../lib/tree';

  const rows = $derived(buildTree(app.channels, app.collapsed));

  function speakingIn(channelId: number): boolean {
    return app.members.some((m) => m.channel_id === channelId && (app.meters.levels[m.user_id] ?? 0) > 0.25);
  }

  async function activate(id: number, isContainer: boolean, hasChildren: boolean): Promise<void> {
    if (isContainer) {
      app.toggleCollapsed(id);
      return;
    }
    if (hasChildren && app.isCollapsed(id)) {
      app.toggleCollapsed(id);
      return;
    }
    await backend.joinChannel(id);
  }
</script>

{#each rows as row (row.channel.id)}
  <div
    class="ch"
    class:on={row.channel.id === app.selfChannel}
    class:cat={row.isContainer}
    style="--d:{row.depth}"
    onclick={() => activate(row.channel.id, row.isContainer, row.hasChildren)}
    role="button"
    tabindex="0"
    onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && activate(row.channel.id, row.isContainer, row.hasChildren)}
  >
    {#if row.hasChildren}
      <button
        type="button"
        class="caret"
        class:down={!row.collapsed}
        class:right={row.collapsed}
        aria-label={row.collapsed ? '展开' : '收起'}
        onclick={(e) => {
          e.stopPropagation();
          app.toggleCollapsed(row.channel.id);
        }}
      ></button>
    {:else}
      <span class="caret"></span>
    {/if}
    {#if !row.isContainer}<span class="hash">#</span>{/if}
    {row.channel.name}
    {#if speakingIn(row.channel.id) && !row.collapsed}
      <span class="spk"><i style="height:4px"></i><i style="height:9px"></i><i style="height:6px"></i></span>
    {/if}
    {#if row.collapsed && row.hasChildren}
      <span class="n">{row.subtreeMembers}</span>
    {:else if !row.isContainer}
      <span class="n">{row.channel.members}</span>
    {/if}
  </div>
{/each}
