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
