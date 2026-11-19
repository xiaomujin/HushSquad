<script lang="ts">
  import type { Channel } from '../lib/ipc';
  import { backend } from '../lib/backend';
  import { menu, type MenuItem } from '../lib/menu.svelte';
  import { app, FLAG_CONTAINER, PERM } from '../lib/state.svelte';
  import {
    buildTree,
    descendantCount,
    moveTarget,
    subtreeMembers,
    type DropTarget,
    type TreeRow,
  } from '../lib/tree';
  import ChannelDialog from './ChannelDialog.svelte';

  // 渲染用 `treeChannels`（镜像 + 拖拽覆盖层），不用 `app.channels`：
  // 拖完必须立刻看到新顺序，而服务器广播会晚一拍。
  const rows = $derived(buildTree(app.treeChannels, app.collapsed));

  /** 打开着的对话框。四个动作共用一个组件，所以这里只记「哪一个 + 目标是谁」。 */
  type Dialog =
    | { kind: 'create'; parent: number; parentName: string }
    | { kind: 'rename'; channel: Channel }
    | { kind: 'limit'; channel: Channel }
    | { kind: 'delete'; channel: Channel };

  let dialog = $state<Dialog | null>(null);
  /** 正在拖的频道 id（`null` = 没在拖）。 */
  let dragging = $state<number | null>(null);
  /** 当前高亮的落点；`null` 表示鼠标下不是合法落点。 */
  let drop = $state<DropTarget | null>(null);
  /** 鼠标正压在非法落点上（用来把光标变成 not-allowed）。 */
  let refuse = $state(false);

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

  /** 灰掉一项：**显示但不可点**，并说明要什么权限（ADR-0016 §6.4）。 */
  function gated(label: string, allowed: boolean, run: () => void, hint = '需要管理员权限'): MenuItem {
    return { label, enabled: allowed, hint: allowed ? undefined : hint, run };
  }

  function openCreate(parent: number): void {
    dialog = { kind: 'create', parent, parentName: app.channels.find((c) => c.id === parent)?.name ?? '' };
  }

  function channelMenu(event: MouseEvent, channel: Channel): void {
    event.preventDefault();
    event.stopPropagation();
    const isContainer = (channel.flags & FLAG_CONTAINER) !== 0;
    const canCreate = app.can(PERM.CREATE_CHANNEL);
    const canManage = app.can(PERM.MANAGE_CHANNEL);
    const full = channel.max_users > 0 && channel.members >= channel.max_users;
    // 「加入」灰掉时要说清楚原因，否则用户会以为点了没反应。
    const blocked = isContainer
      ? '分组不可加入'
      : channel.id === app.selfChannel
        ? '已经在这个频道'
        : full
          ? '频道已满'
          : '';
    const items: MenuItem[] = [
      {
        label: '加入频道',
        enabled: blocked === '',
        hint: blocked === '' ? undefined : blocked,
        run: () => void backend.joinChannel(channel.id),
      },
      { separator: true, label: '' },
      gated('新建子频道…', canCreate, () => openCreate(channel.id)),
      gated('重命名…', canManage, () => (dialog = { kind: 'rename', channel })),
      gated(isContainer ? '取消分组' : '设为分组', canManage, () => {
        const flags = isContainer ? channel.flags & ~FLAG_CONTAINER : channel.flags | FLAG_CONTAINER;
        void backend.updateChannel(channel.id, { flags }).catch((error) => app.notify(String(error)));
      }),
      gated('人数上限…', canManage, () => (dialog = { kind: 'limit', channel })),
      { separator: true, label: '' },
      {
        ...gated('删除频道…', canManage, () => (dialog = { kind: 'delete', channel })),
        danger: true,
      },
    ];
    menu.show(event, items);
  }

  /** 「频道」区头：只有根级新建。区头的 `+` 按钮做同一件事。 */
  function headerMenu(event: MouseEvent): void {
    event.preventDefault();
    menu.show(event, [gated('新建频道…', app.can(PERM.CREATE_CHANNEL), () => openCreate(0))]);
  }

  // ------------------------------------------------------------ 拖拽

  /** 落点算成请求参数（`null` = 非法）。用 `treeChannels`：用户看到的是覆盖后的顺序。 */
  function resolve(target: DropTarget): { parent: number; position: number } | null {
    if (dragging === null) return null;
    return moveTarget(app.treeChannels, dragging, target);
  }

  /**
   * 行内的落点判定：上沿 30% 是「插到这一行前面」（间隙），其余是「成为它的子节点」。
   *
   * 用行内的位置而不是单独铺一层细窄的间隙元素：30px 高的行里，2px 的元素几乎点不中，
   * 而横线照样画在行的上沿，看起来还是「行与行之间」。
   */
  function targetOf(event: DragEvent, row: TreeRow): DropTarget {
    const element = event.currentTarget as HTMLElement;
    const rect = element.getBoundingClientRect();
    const ratio = rect.height > 0 ? (event.clientY - rect.top) / rect.height : 1;
    return ratio < 0.3 ? { kind: 'gap', id: row.channel.id } : { kind: 'body', id: row.channel.id };
  }

  function sameTarget(a: DropTarget | null, b: DropTarget | null): boolean {
    if (a === null || b === null) return a === b;
    if (a.kind !== b.kind) return false;
    if (a.kind === 'root' || b.kind === 'root') return true;
    return a.id === b.id;
  }

  /** dragover 每帧都来，状态没变就别写：否则整棵树每帧重渲染。 */
  function setDrop(next: DropTarget | null, denied: boolean): void {
    if (!sameTarget(drop, next)) drop = next;
    if (refuse !== denied) refuse = denied;
  }

  function onDragStart(event: DragEvent, id: number): void {
    dragging = id;
    drop = null;
    refuse = false;
    if (event.dataTransfer) {
      event.dataTransfer.effectAllowed = 'move';
      // 内容用不到，但没写 data 的拖拽在部分浏览器里根本不会开始。
      event.dataTransfer.setData('text/plain', String(id));
    }
  }

  function onRowDragOver(event: DragEvent, row: TreeRow): void {
    if (dragging === null) return;
    event.preventDefault();
    event.stopPropagation();
    const target = targetOf(event, row);
    const legal = resolve(target) !== null;
    if (event.dataTransfer) event.dataTransfer.dropEffect = legal ? 'move' : 'none';
    setDrop(legal ? target : null, !legal);
  }

  /** 空白区 / 区头：成为根级，追加到末尾。 */
  function onRootDragOver(event: DragEvent): void {
    if (dragging === null) return;
    event.preventDefault();
    const legal = resolve({ kind: 'root' }) !== null;
    if (event.dataTransfer) event.dataTransfer.dropEffect = legal ? 'move' : 'none';
    setDrop(legal ? { kind: 'root' } : null, !legal);
  }

  function onDragLeave(event: DragEvent): void {
    // dragleave 在子元素之间移动时也会冒上来，只有真的离开整棵树才清掉高亮。
    const next = event.relatedTarget;
    if (next instanceof Node && (event.currentTarget as HTMLElement).contains(next)) return;
    drop = null;
    refuse = false;
  }

  function endDrag(): void {
    dragging = null;
    drop = null;
    refuse = false;
  }

  function onDrop(event: DragEvent, target: DropTarget): void {
    event.preventDefault();
    event.stopPropagation();
    const id = dragging;
    const move = id === null ? null : resolve(target);
    endDrag();
    if (id === null || !move) return;
    // 先盖覆盖层：等服务器广播会有肉眼可见的延迟，拖完必须立刻看到新顺序。
    app.setPendingMove({ channelId: id, parent: move.parent, position: move.position });
    void backend.moveChannel(id, move.parent, move.position).catch((error) => {
      // 命令本身失败（没连上、参数被拒）：丢掉覆盖层回到服务器给的样子。
      app.dropPendingTree();
      app.notify(String(error));
    });
  }
</script>

<div
  class="ctree"
  class:dragging={dragging !== null}
  class:refuse
  role="group"
  aria-label="频道列表"
  ondragover={onRootDragOver}
  ondrop={(event) => onDrop(event, { kind: 'root' })}
  ondragleave={onDragLeave}
>
  <div class="side-h tree-h" role="group" oncontextmenu={headerMenu}>
    <span>频道</span>
    <button class="tree-add" type="button" title="新建频道" aria-label="新建频道" onclick={() => openCreate(0)}>+</button>
  </div>

  {#each rows as row (row.channel.id)}
    <div
      class="ch"
      class:on={row.channel.id === app.selfChannel}
      class:cat={row.isContainer}
      class:drop-body={drop?.kind === 'body' && drop.id === row.channel.id}
      class:drop-gap={drop?.kind === 'gap' && drop.id === row.channel.id}
      class:lifted={dragging === row.channel.id}
      style="--d:{row.depth}"
      role="button"
      tabindex="0"
      draggable="true"
      onclick={() => activate(row.channel.id, row.isContainer, row.hasChildren)}
      onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && activate(row.channel.id, row.isContainer, row.hasChildren)}
      oncontextmenu={(event) => channelMenu(event, row.channel)}
      ondragstart={(event) => onDragStart(event, row.channel.id)}
      ondragover={(event) => onRowDragOver(event, row)}
      ondrop={(event) => onDrop(event, targetOf(event, row))}
      ondragend={endDrag}
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

  <!-- 根级末尾的落点提示：与行间隙是同一条横线，只是缩进为 0。 -->
  {#if drop?.kind === 'root'}
    <div class="ch-gap" style="--d:0"></div>
  {/if}
</div>

{#if dialog}
  {#if dialog.kind === 'create'}
    <ChannelDialog
      mode="create"
      parentId={dialog.parent}
      parentName={dialog.parentName}
      onclose={() => (dialog = null)}
    />
  {:else if dialog.kind === 'delete'}
    <ChannelDialog
      mode="delete"
      channel={dialog.channel}
      childCount={descendantCount(app.channels, dialog.channel.id)}
      memberCount={subtreeMembers(app.channels, dialog.channel.id)}
      onclose={() => (dialog = null)}
    />
  {:else}
    <ChannelDialog mode={dialog.kind} channel={dialog.channel} onclose={() => (dialog = null)} />
  {/if}
{/if}
