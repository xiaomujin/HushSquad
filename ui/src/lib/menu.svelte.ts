// 右键菜单的**唯一**状态。菜单是全局单例：同一时刻只会有一个开着。
//
// 为什么不用一个通用的「弹出层」组件：菜单的定位要跟着鼠标走（`clientX/clientY`），
// 还要在贴边时往回收，这些只对菜单有意义。做成通用组件会多一层没人用的抽象。

export interface MenuItem {
  label: string;
  /** 点下去做什么。`enabled === false` 时不会调用。 */
  run?: () => void;
  /** 灰掉但**仍然显示**：隐藏会让用户既不知道有这个功能，也不知道该找谁开权限。 */
  enabled?: boolean;
  /** 灰掉时的说明（`title`），比如「需要管理员权限」。 */
  hint?: string;
  /** 危险操作（踢人）标红。 */
  danger?: boolean;
  /** 分隔线（此时只有这一项有意义）。 */
  separator?: boolean;
}

class MenuState {
  open = $state(false);
  x = $state(0);
  y = $state(0);
  items = $state<MenuItem[]>([]);

  /** 在鼠标位置打开。贴到右下角时往回收，免得菜单被窗口裁掉。 */
  show(event: MouseEvent, items: MenuItem[]): void {
    const width = 208;
    const height = 26 * items.length + 12;
    this.x = Math.min(event.clientX, window.innerWidth - width - 8);
    this.y = Math.min(event.clientY, window.innerHeight - height - 8);
    this.items = items;
    this.open = true;
  }

  close(): void {
    this.open = false;
  }
}

export const menu = new MenuState();
