<script lang="ts">
  import { menu } from '../lib/menu.svelte';

  // Esc 关菜单：菜单挡住半个屏幕时，用户第一反应是按键。
  function onkey(event: KeyboardEvent): void {
    if (event.key === 'Escape') menu.close();
  }
</script>

<svelte:window onkeydown={onkey} />

{#if menu.open}
  <!-- 透明遮罩：点空白处关菜单。右键也关（避免叠两层）。 -->
  <button
    class="ctx-scrim"
    aria-label="关闭菜单"
    onclick={() => menu.close()}
    oncontextmenu={(event) => {
      event.preventDefault();
      menu.close();
    }}
  ></button>
  <div class="ctx" style="left:{menu.x}px; top:{menu.y}px" role="menu" tabindex="-1">
    {#each menu.items as item, index (index)}
      {#if item.separator}
        <div class="ctx-sep"></div>
      {:else}
        <button
          class="ctx-item"
          class:off={item.enabled === false}
          class:danger={item.danger}
          role="menuitem"
          disabled={item.enabled === false}
          title={item.enabled === false ? (item.hint ?? '') : ''}
          onclick={() => {
            if (item.enabled === false) return;
            menu.close();
            item.run?.();
          }}
        >
          {item.label}
          {#if item.enabled === false && item.hint}
            <span class="ctx-hint">{item.hint}</span>
          {/if}
        </button>
      {/if}
    {/each}
  </div>
{/if}
