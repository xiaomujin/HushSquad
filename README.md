# HushSquad（静队）

> 快如闪电，静如深海。

类 TeamSpeak 的语音开黑软件。Rust + Tauri 2，桌面与 Android 优先，Web 端暂缓。
核心追求：低延迟（分档验收）、强降噪（DeepFilterNet3 / RNNoise 双档）、频道/小队语音。

## 当前状态

**P0 完成**：工作区骨架、依赖纪律、门禁脚本已落地（门禁全绿）。
**P1 进行中**：`audio-core` 的接缝（`Stage` / `Frame` / 延迟换算）已定义，Opus 封装与 `testkit` 离线回放待做。

| 阶段 | 状态 |
|---|---|
| 技术设计 | v0.2 完成（含 v0.2.1 勘误） |
| 依赖选型核实 | v0.1 完成（源码级核实） |
| 协议草案 | v0.1 完成 |
| 执行方案 | v0.1.1 完成 |
| P0 地基（代码） | **完成**（7 个 crate 骨架 + deny.toml + 门禁脚本，全绿） |
| P1 骨架 L0（代码） | 进行中（P1-1 接缝 / P1-2 链路描述 / P1-3 Opus 封装 已完成；下一步 P1-4 testkit） |

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/HushSquad-技术设计-v0.2.md`](docs/HushSquad-技术设计-v0.2.md) | 架构、音频链路、传输、服务器、里程碑、验收标准、延迟预算 |
| [`docs/crates-选型验证报告-v0.1.md`](docs/crates-选型验证报告-v0.1.md) | 每个候选依赖的证据等级与结论（含 4 个不存在的包） |
| [`docs/协议草案-v0.1.md`](docs/协议草案-v0.1.md) | 控制面 + 语音面协议、密钥派生、语音包字节布局、错误码 |
| [`docs/proto/hushsquad-control-v1.proto`](docs/proto/hushsquad-control-v1.proto) | 控制面 protobuf 定义 |
| [`docs/执行方案与步骤-v0.1.md`](docs/执行方案与步骤-v0.1.md) | P0–P5 逐步执行计划、闸门数字、失败对策、勾选清单 |

## 阅读顺序

设计文档 → 选型报告 → 协议草案 → 执行方案。四份文档里的关键常数已对齐（10 ms 处理帧、20 B 语音包头、30 ms DFN3 延迟）。

## 目录结构

```text
crates/
  audio-core/    纯 DSP：降噪/AGC/Opus/抖动缓冲/混音。不依赖 I/O 与网络
  audio-io/      cpal 封装：设备枚举、热插拔、Android 权限与路由
  protocol/      控制面消息 + 语音包线格式（20 B 头）
  transport/     加密 UDP：ChaCha20-Poly1305、keepalive、会话重绑定
  client-core/   客户端状态机：频道、成员、PTT、选择性收听
  server-core/   频道树、ACL、SFU 语音转发
  testkit/       离线回放 / 网络模拟 / 延迟测量（CLI）
docs/            设计、选型、协议、执行方案、ADR、measurements.md
proto 定义见 docs/proto/hushsquad-control-v1.proto
```

依赖方向是硬约束：`audio-core` 不认识 `transport`/`protocol`（门禁里有一条断言在守着）。

## 构建与检查

### Windows（需要 MSVC 工具链）

两种方式任选：

1. 在 **Developer PowerShell for VS 2022** 里直接 `cargo build`；
2. 普通 shell 用包装脚本（内部用 vswhere 定位 VS，自动 call vcvars64）：

```bat
scripts\with-msvc.bat cargo build --workspace
```

构建前置：MSVC 工具链 + **cmake**（`opusic-sys` 用它编译 libopus）。本机 cmake 装在
`%LOCALAPPDATA%\Programs\CMake`，`scripts\with-msvc.bat` 会自动补进 PATH；CI 的 ubuntu 镜像自带。

### 一键门禁（5 项，任一失败即非 0）

```powershell
powershell -ExecutionPolicy Bypass -File scripts\check.ps1
```

```bash
./scripts/check.sh
```

门禁内容：`cargo fmt --check` → `cargo clippy -D warnings` → `cargo test` → `cargo deny check`（许可证 / advisory / 来源 / 禁用 crate）→ **audio-core 不得含 tokio / axum / tower** 的依赖纯净断言。

`Cargo.lock` 入库（项目含二进制目标，构建需可复现）。工具链锁定在 `rust-toolchain.toml`。
## 明确不做

浏览器 Web 端、视频、MCU 服务端混音（v1）、P2P/NAT 打洞（v1）、完整 WebRTC ICE/SDP、Mumble 客户端互通。

## 许可证

**待定**。在确定许可证前，本仓库不得引入任何 AGPL 或来源不明的代码（见 `docs/执行方案与步骤-v0.1.md` §12.1）。
