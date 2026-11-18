# ui/ — 静队的 Web 前端

> 这份前端**只是渲染器**：不碰 PCM、不碰网络包、不算延迟账。状态的唯一真相在 Rust 侧。
> 决策与理由见 [`ADR-0013`](../docs/ADR/ADR-0013-前端框架与渲染器边界.md)；视觉与交互见
> [`docs/UI设计-v0.1.md`](../docs/UI设计-v0.1.md)。

## 跑起来

```bash
cd ui
pnpm install
pnpm dev        # http://localhost:5173 —— 浏览器里就能点，用 Mock 后端
pnpm build      # 产物在 ui/dist，给 Tauri 的 frontendDist 用
pnpm check      # svelte-check（类型 + a11y），应为 0 errors 0 warnings
```

**新机器上如果 `pnpm install` 报 `ERR_PNPM_IGNORED_BUILDS: esbuild`**：pnpm 11 默认不跑依赖的
构建脚本，而 esbuild 需要 postinstall 装平台二进制。跑一次 `pnpm approve-builds --all` 即可
（白名单同时写在 `pnpm-workspace.yaml` 的 `onlyBuiltDependencies` 里，供 CI 参考）。

## 目录

```text
ui/
├── index.html
├── src/
│   ├── main.ts               # 挂载
│   ├── App.svelte            # 选屏：连接 / 房间 / 设置
│   ├── app.css               # 视觉 token + 组件样式（与 mockup.html 共用同一份变量）
│   ├── lib/
│   │   ├── ipc.ts            # ★ 命令与事件的唯一定义处 + MockBackend
│   │   ├── backend.ts        # 后端实例（现在固定 Mock，接 Tauri 时只改这里）
│   │   ├── state.svelte.ts   # 镜像（runes），只有快照，没有本地真相
│   │   ├── tree.ts           # 扁平频道表 → 可渲染的行（折叠、深度、子树人数）
│   │   └── format.ts         # 头像字、电平百分比、dBFS 显示
│   └── components/
│       ├── ConnectScreen.svelte
│       ├── RoomScreen.svelte     # 顶栏 + 左栏 + 主区 + 面板 + 底栏
│       ├── ChannelTree.svelte
│       ├── MemberArea.svelte     # 网格 / 列表两种视图
│       ├── NetworkPanel.svelte   # 软件预算 + 实时 + 自检
│       ├── BottomBar.svelte      # 麦克风电平 + 语音激活状态灯
│       └── SettingsScreen.svelte
├── pnpm-workspace.yaml       # esbuild 构建白名单
└── dist/                     # 构建产物（不入库）
```

## Mock 与真实后端

`src/lib/ipc.ts` 定义了 `Backend` 接口，`MockBackend` 是它的浏览器实现（假数据 + 定时器，
20 Hz 电平、1 Hz 网络数字）。Tauri 壳接进来之后，写一个 `TauriBackend` 实现同一接口，
把 `src/lib/backend.ts` 里的一行换掉即可——组件一行都不用改。

约定（ADR-0013）：

- 低频「状态变了」（连接状态、频道树、成员增删）→ Tauri `emit`；
- 高频「数字在动」（电平、VAD、抖动缓冲、RTT）→ Tauri 2 的 `Channel<T>`，20 Hz 合并推送；
- 前端不做定时轮询，不在本地算延迟，不做乐观更新（音量滑杆例外：拖动时本地跟随）。

## 评审用的 URL 参数（只影响 Mock）

```text
?autoconnect=1        直接进房间
&view=list            列表模式
&panel=1              展开网络面板
&collapse=12          预折叠指定分组（逗号分隔多个 id）
```

例：`http://localhost:5173/?autoconnect=1&view=list&panel=1`

## 与 `docs/ui/mockup.html` 的关系

稿子是**视觉参考**，这里是**实现**。两者共用 `app.css` 里的同一套 token；
改视觉的顺序是「先改 token，再改组件」，否则两边会漂。
