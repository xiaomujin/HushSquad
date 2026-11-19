# apps/app — 桌面外壳（Tauri 2）

> 这层壳只做三件事：开窗口、把 IPC 命令转成 `hushsquad_client` 的调用、把会话状态推回前端。
> **没有任何音频或网络逻辑**——那些都在 `apps/client` 里，所以命令行客户端和界面跑的是同一条链路。
> 决策与理由见 [`ADR-0014`](../../docs/ADR/ADR-0014-桌面外壳与门禁的边界.md)。

## 为什么是独立的 workspace

`apps/app/Cargo.toml` 里有自己的 `[workspace]` 表，根 `Cargo.toml` 用 `exclude = ["apps/app"]`
把它排除。原因只有一个：**Tauri 的依赖树（几百个 crate）和它要的系统库不该绑架音频门禁**。
`scripts/check.ps1` 因此一个字没改，`cargo deny` 的许可证扫描也不会混进 GUI 的几百个 crate。

代价是两份 `target/`，以及「`cargo fmt --all` 覆盖不到这里」——所以要记得跑
`scripts/check-app.ps1`。它**不在 CI 里**（明确欠账，等 Linux 打包真跑通一次再加）。

## 跑起来

```powershell
pwsh -File scripts/check-app.ps1          # 前端 check + build，再编外壳（顺序不能反）
& apps/app/target/debug/hushsquad-app.exe
```

`frontendDist` 指向 `ui/dist` 且是**编译期内嵌**的：改了 `ui/` 必须先 `pnpm build` 再
`cargo build`，否则窗口里跑的还是上一版资源。`tauri.conf.json` 的 `beforeBuildCommand`
只在 `tauri build` 时生效，`cargo build` 不触发。

## 目录

```text
apps/app/
├── src/
│   ├── main.rs        # 薄壳：调 run()
│   ├── lib.rs         # Builder + invoke_handler 列表
│   ├── ipc.rs         # 前端能调的命令（含"哪些还不是真的"的表）
│   └── session.rs     # 会话线程 + Bridge（Channel<Event> 往外推，指令队列往里拉）
├── capabilities/      # 主窗口的最小权限集（core:default）
├── icons/             # 由 scripts/gen-icons.py 生成
├── tauri.conf.json
└── build.rs
```

## 会话线程

```text
ui ──invoke──▶ ipc.rs ──▶ session.rs ──▶ hushsquad_client::run_with(cfg, host)
                   ▲                              │
                   └──── Channel<Event> ◀─────────┘
```

- 会话跑在**自己的线程**上（current_thread 运行时），因为 Tauri 主线程要跑窗口消息循环；
- `Host` 是 `apps/client` 里的普通 trait，**不带 tauri 依赖**（视图类型只 derive `Serialize`），
  换外壳不用动 `apps/client`；
- 三条推送节奏：状态「变了才推」、电平 20 Hz、网络 0.2 Hz；
- 指令每 tick 拉一次，所以界面上按一下最多 10 ms 生效。

## 还不是真的

`ipc.rs` 顶部有一张表。当前最要紧的两条：

1. **`setVoice` 没有后端**：上行还没有 VAD 门控，麦克风一直在发（语音激活 / PTT / 常开
   三种模式在行为上还没区别）；
2. **`selftest` 如实报错**：测量代码在 `audio_io::impulse`，但还没拼成会话里的一次往返。
   在它之前，网络面板的预算分解与设备段只能显示「未测量」。
