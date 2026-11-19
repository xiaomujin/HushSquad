import { FLAG_CONTAINER, type Channel } from './ipc';

export interface TreeRow {
  channel: Channel;
  depth: number;
  hasChildren: boolean;
  collapsed: boolean;
  /** 折叠时显示的子树人数合计（不含自己）。 */
  subtreeMembers: number;
  /** 分组容器：不可加入，只用来聚合。 */
  isContainer: boolean;
}

/**
 * 把扁平的频道表变成可渲染的行（ADR / UI 设计 §2.1）。
 *
 * 规则：
 * - 同级按 `position`，相同则按名字；
 * - 最多渲染 `maxDepth` 层（设计上限 3 层），更深的忽略而不是崩掉；
 * - 折叠的节点不展开子节点，但在行上给 `subtreeMembers`；
 * - `flags & 8` 是分组容器，点击只折叠、不加入。
 */
export function buildTree(
  channels: Channel[],
  collapsed: readonly number[],
  maxDepth = 3,
): TreeRow[] {
  const byParent = new Map<number, Channel[]>();
  for (const c of channels) {
    const list = byParent.get(c.parent) ?? [];
    list.push(c);
    byParent.set(c.parent, list);
  }
  for (const list of byParent.values()) {
    list.sort((a, b) => (a.position - b.position) || a.name.localeCompare(b.name));
  }

  const rows: TreeRow[] = [];
  const seen = new Set<number>();

  const walk = (parent: number, depth: number): void => {
    if (depth >= maxDepth) return;
    for (const channel of byParent.get(parent) ?? []) {
      if (seen.has(channel.id)) continue; // 防环：坏数据不该让界面卡死
      seen.add(channel.id);

      const children = byParent.get(channel.id) ?? [];
      const isCollapsed = collapsed.includes(channel.id);
      rows.push({
        channel,
        depth,
        hasChildren: children.length > 0,
        collapsed: isCollapsed,
        subtreeMembers: countSubtree(channel.id, byParent),
        isContainer: (channel.flags & FLAG_CONTAINER) !== 0,
      });
      if (!isCollapsed) walk(channel.id, depth + 1);
    }
  };

  walk(0, 0);
  return rows;
}

function countSubtree(id: number, byParent: Map<number, Channel[]>): number {
  let total = 0;
  const stack = [...(byParent.get(id) ?? [])];
  const guard = new Set<number>();
  while (stack.length > 0) {
    const c = stack.pop();
    if (!c || guard.has(c.id)) continue;
    guard.add(c.id);
    total += c.members;
    stack.push(...(byParent.get(c.id) ?? []));
  }
  return total;
}

// ---------------------------------------------------------------- 拖拽与覆盖层

/**
 * 乐观覆盖层的一步：把 `channelId` 挪到 `parent` 下的第 `position` 位。
 *
 * 它只是**显示用**的补丁，随时可以整块丢掉——真正的频道表只由服务器的快照改，
 * 所以「丢掉覆盖层」就自动回到服务器给的样子，不需要写回滚。
 */
export interface PendingMove {
  channelId: number;
  parent: number;
  position: number;
}

/**
 * 拖拽落点（ChannelTree 按鼠标位置判出来，再交给 `moveTarget` 算成请求参数）。
 *
 * - `body`：落到这一行的主体上 → 成为它的子节点，追加到末尾；
 * - `gap`：落到这一行上沿那条线 → 成为它的兄弟，插在它**前面**；
 * - `root`：落到空白区 / 区头 → 成为根级，追加到末尾。
 */
export type DropTarget = { kind: 'body'; id: number } | { kind: 'gap'; id: number } | { kind: 'root' };

/** 同级排序：与 `buildTree` 同一条规则。 */
function byPosition(a: Channel, b: Channel): number {
  return (a.position - b.position) || a.name.localeCompare(b.name);
}

/** 某个父级下排好序的兄弟；`exclude` 用来「先拿掉被拖的那个再算下标」。 */
export function siblingsOf(channels: Channel[], parent: number, exclude?: number): Channel[] {
  return channels.filter((c) => c.parent === parent && c.id !== exclude).sort(byPosition);
}

/** 深度：根级 0，根（`id = 0`）自己算 -1。父链断了就当 0，坏数据不该让界面卡死。 */
export function depthOf(channels: Channel[], id: number): number {
  if (id === 0) return -1;
  const byId = new Map(channels.map((c) => [c.id, c]));
  const seen = new Set<number>();
  let depth = 0;
  let current = byId.get(id);
  while (current && current.parent !== 0 && !seen.has(current.id)) {
    seen.add(current.id);
    const parent = byId.get(current.parent);
    if (!parent) break;
    depth += 1;
    current = parent;
  }
  return depth;
}

/** `id` 是不是 `ancestor` 的子孙（**不含** `ancestor` 自己）。 */
export function isDescendant(channels: Channel[], id: number, ancestor: number): boolean {
  const byId = new Map(channels.map((c) => [c.id, c]));
  const seen = new Set<number>();
  let current = byId.get(id);
  while (current && !seen.has(current.id)) {
    if (current.parent === ancestor) return true;
    if (current.parent === 0) return false;
    seen.add(current.id);
    current = byId.get(current.parent);
  }
  return false;
}

/** 子树高度：只有自己这层是 1。层数上限的判定要用它。 */
export function subtreeHeight(channels: Channel[], id: number, guard = new Set<number>()): number {
  if (guard.has(id)) return 1; // 成环的坏数据：当作叶子，别递归到栈溢出
  guard.add(id);
  const children = channels.filter((c) => c.parent === id);
  if (children.length === 0) return 1;
  return 1 + Math.max(...children.map((child) => subtreeHeight(channels, child.id, guard)));
}

/** 子孙个数（不含自己）。删除确认里「连同 N 个子频道」用它。 */
export function descendantCount(channels: Channel[], id: number): number {
  const guard = new Set<number>([id]);
  const stack = channels.filter((c) => c.parent === id).map((c) => c.id);
  let total = 0;
  while (stack.length > 0) {
    const next = stack.pop();
    if (next === undefined || guard.has(next)) continue;
    guard.add(next);
    total += 1;
    stack.push(...channels.filter((c) => c.parent === next).map((c) => c.id));
  }
  return total;
}

/** 子树人数合计（含自己）。删除前判断「里面还有没有人」用它。 */
export function subtreeMembers(channels: Channel[], id: number): number {
  const guard = new Set<number>([id]);
  const stack = channels.filter((c) => c.parent === id);
  let total = channels.find((c) => c.id === id)?.members ?? 0;
  while (stack.length > 0) {
    const next = stack.pop();
    if (!next || guard.has(next.id)) continue;
    guard.add(next.id);
    total += next.members;
    stack.push(...channels.filter((c) => c.parent === next.id));
  }
  return total;
}

/**
 * 判定一次拖拽落点：合法就给出 `moveChannel` 的参数，非法返回 `null`。
 *
 * 非法有三种：拖到自己身上、拖进自己的子树（成环）、落点深度加上被拖子树高度超过
 * 3 层（设计上限，与 `buildTree` 的 `maxDepth` 是同一条线）。另外「落点和现在一模
 * 一样」也当非法——为一次没有变化的操作打扰服务器没有意义。
 */
export function moveTarget(
  channels: Channel[],
  draggedId: number,
  target: DropTarget,
): { parent: number; position: number } | null {
  const dragged = channels.find((c) => c.id === draggedId);
  if (!dragged) return null;

  let parent: number;
  let position: number;
  if (target.kind === 'root') {
    parent = 0;
    position = siblingsOf(channels, 0, draggedId).length;
  } else {
    const row = channels.find((c) => c.id === target.id);
    if (!row) return null;
    if (target.kind === 'body') {
      if (row.id === draggedId) return null; // 拖到自己身上
      parent = row.id;
      position = siblingsOf(channels, row.id, draggedId).length;
    } else {
      parent = row.parent;
      // 下标按「去掉被拖者之后」的兄弟列表算，所以被拖者原本在前面时不用再 -1。
      position = siblingsOf(channels, parent, draggedId).findIndex((c) => c.id === row.id);
      if (position < 0) return null;
    }
  }

  if (parent === draggedId || isDescendant(channels, parent, draggedId)) return null;
  // 被拖者落在 `depthOf(parent) + 1` 层，它的子树再往下 `subtreeHeight - 1` 层。
  if (depthOf(channels, parent) + subtreeHeight(channels, draggedId) > 2) return null;

  const current = siblingsOf(channels, dragged.parent).findIndex((c) => c.id === draggedId);
  if (parent === dragged.parent && position === current) return null;
  return { parent, position };
}

/**
 * 把覆盖层套在镜像上（**不**改 `app.channels`）。
 *
 * 为什么要连兄弟的 `position` 一起重排：落点下标是「去掉被拖者之后插在第几位」，
 * 直接写进被拖者的 `position` 会和原来的兄弟撞号，而 `buildTree` 撞号时按名字排，
 * 于是拖完看到的位置可能不是刚拖到的地方——乐观更新就白做了。
 */
export function applyPending(channels: Channel[], moves: PendingMove[] | null): Channel[] {
  if (!moves || moves.length === 0) return channels;
  const out = channels.map((c) => ({ ...c }));
  for (const move of moves) {
    const node = out.find((c) => c.id === move.channelId);
    if (!node) continue;
    node.parent = move.parent;
    const siblings = siblingsOf(out, move.parent, move.channelId);
    const at = Math.max(0, Math.min(move.position, siblings.length));
    siblings.splice(at, 0, node);
    siblings.forEach((c, index) => {
      c.position = index;
    });
  }
  return out;
}
