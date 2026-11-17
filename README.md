# HushSquad（静队）

> 快如闪电，静如深海。

类 TeamSpeak 的语音开黑软件。Rust + Tauri 2，桌面与 Android 优先，Web 端暂缓。
核心追求：低延迟（分档验收）、强降噪（DeepFilterNet3 / RNNoise 双档）、频道/小队语音。

## 当前状态

**P0 完成**：工作区骨架、依赖纪律、门禁脚本已落地（门禁全绿）。
**P1 完成**：离线直通链路 + 测量工具已落地——`testkit run` 能出 encode/decode 的 p50/p95/max，热路径分配数由机器验证为 0。

**P2 进行中**：`audio-io` 与 `loopback` 已落地，物理环回延迟测出真值——**本机默认设备 198.3 ms，D1 预算 ≤15 ms，不达标**，根因在输出设备的音效链（APO）而非我们的代码。下一步做 WASAPI 独占模式绕过它。

| 阶段 | 状态 |
|---|---|
| 技术设计 | v0.2 完成（含 v0.2.1 勘误） |
| 依赖选型核实 | v0.1 完成（源码级核实） |
| 协议草案 | v0.1 完成 |
| 执行方案 | v0.1.1 完成 |
| P0 地基（代码） | **完成**（7 个 crate 骨架 + deny.toml + 门禁脚本，全绿） |
| P1 骨架 L0（代码） | **完成**（接缝 / 链路描述 / Opus 封装 / testkit 离线回放 / 素材生成 / 延迟模型，tag `p1-gate`） |

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

### 离线测量（P1 起）

```powershell
# 1. 生成合成素材（固定种子，不入库）
cargo run -p testkit --release -- gen --out testdata/synthetic

# 2. 跑一遍链路，出报告（耗时数字只在 --release 下有意义）
cargo run -p testkit --release -- run --in testdata/synthetic/clean_48k_mono.wav \
    --out out/proc.wav --chain opus --codec opus:10ms:32k --report out/report.json

# 3. 把技术设计附录 A 的延迟预算用代码复算一遍
cargo run -p testkit --release -- latency-model --chain dfn3,agc2,opus
```

`run` 报告里的 `allocations_in_hot_path` 必须为 0，否则命令直接失败——这是「热路径不许分配」的机器门禁。

### 桌面环回（P2）

```powershell
# 看设备
cargo run -p loopback -- --list-devices

# 直通：采集 → rtrb → 播放（会出声，注意回授）
cargo run -p loopback --release -- --seconds 10 --ring-ms 10 --report out/loopback.json

# 环回延迟（D1）：播放一个 1 ms 脉冲 + 麦克风录回，匹配滤波定位 + 回调时间戳换算
cargo run -p loopback --release -- --impulse --single --seconds 3 --repeat 5 \
    --report out/impulse.json
```

`--buffer <帧>` 是设备缓冲的请求值（如 128/256/480），调小降延迟、更容易 xrun；不给就用设备默认。
**共享模式下它不改变实测延迟**（本机 128 与 480 差 0.16 ms），别指望靠调小它救延迟。

`--single` 只发一个脉冲（没有周期歧义）；`--repeat N` 连测 N 次并给出中位数与极差；
`--dump out/rec.wav` 把录音写成 WAV（排查「两个峰」这类问题的唯一手段）。

`--impulse` 的主值是**时间戳法**：把「发出去的那个样本」和「录到的那个样本」各自换算成同一根回调墙钟再相减，
因此不含我们自己的环形缓冲占用，也不含两个流的启动错位。
`delay_ms_correlation` 是相关法的位移，含流启动错位（本机 0.4–1.4 s），**只能当诊断**。

> 本机实测：默认设备环回延迟 **198.3 ms**（n=5，极差 0.39 ms），D1 预算 ≤15 ms → **不达标**；
> 瓶颈在输出设备的音效链（WASAPI 自己只承认 43 ms），见 `docs/measurements.md` §1.1 与结论 C-05/C-06。
## 明确不做

浏览器 Web 端、视频、MCU 服务端混音（v1）、P2P/NAT 打洞（v1）、完整 WebRTC ICE/SDP、Mumble 客户端互通。

## 许可证

**待定**。在确定许可证前，本仓库不得引入任何 AGPL 或来源不明的代码（见 `docs/执行方案与步骤-v0.1.md` §12.1）。
