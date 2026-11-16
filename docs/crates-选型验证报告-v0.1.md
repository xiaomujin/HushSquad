# HushSquad crates 选型验证报告

> 版本 v0.1　日期 2026-11-16　配套文档：`HushSquad-技术设计-v0.2.md`
> 用途：给每个候选依赖一个**可复核的证据结论**，避免把「候选」当「已确定」，也避免把不存在的包写进计划。

## 0. 结论速览

| 领域 | 采用 | 版本 | 证据等级 | 主要风险 | 备选 |
|---|---|---|---|---|---|
| 音频 I/O | `cpal` | 0.18.2 | **E1** | 安卓 AAudio burst 大小未实测 | `oboe` 0.6.1（已停更） |
| 无锁缓冲 | `rtrb` | 0.4.1 | E2 | 无 | — |
| 重采样 | `rubato` | 5.1.0 | E2 | 需自己做漂移补偿环 | — |
| 降噪（质量档） | `deep-filter-rt` | 0.2.0 | **E1** | tract 在 ARM 的实时性、+8.4 MB 产物、MSRV 1.96 | 自行封装 libDF |
| 降噪（低延迟档） | `nnnoiseless` | 0.5.2 | **E1** | 非平稳噪声效果一般 | — |
| AGC | `sonora-agc2` | 0.3.0 | **E1** | 需与平台 AGC 二选一 | 自研简单增益 |
| AEC（可选） | `webrtc-audio-processing` | 2.1.0 | E1/E3 | 需 C++ 交叉编译 | `sonora` 0.3（纯 Rust） |
| Opus | `opus` | 0.4.0 | **E1** | 绑定 `opusic-sys`，需 C 构建 | `opus-pure` 0.2.3（比预期成熟） |
| 抖动缓冲 | `neteq` | 0.9.1 | **E1** | **默认 feature 会拉进 axum/tokio**；解码器绑 `ropus` | `jittr` 0.2（无时间伸缩） |
| 语音加密 | `chacha20poly1305` | 0.11.0 | E2 | 无 | AES-GCM（M0-D9 对比） |
| 控制面 | `rustls` 0.23 稳定线 + `tokio` 1.54 | — | E2 | 勿误用 0.24-dev | `quinn` 0.11（QUIC，待决） |
| 工具 | `cargo-deny` 0.20 | — | E2 | 必须进 CI，否则 AGPL 会悄悄进来 | — |

**禁止进 `Cargo.toml`**：`buzz-relay`、`voicemcu`、`svod`、`dtln-rs`（crates.io 上不存在）、`mumcord-codec`（AGPL-3.0-only）、任何 OCB2 实现、`sew-noise` 与 `punch-hole`（近零使用，不得承重）。

## 1. 验证方法与证据等级

| 等级 | 含义 | 本次如何取得 |
|---|---|---|
| **E1** | 解包 `.crate` 源码逐行核实（依赖树、常量、API 签名、许可证文件、NOTICE） | `static.crates.io` 下载 + gunzip/untar + 检索 |
| E2 | crates.io API 元数据（版本、下载量、许可证字段、更新时间、仓库地址） | `crates.io/api/v1/crates/{name}` |
| E3 | 仓库 README / CHANGELOG 自述 | 随包文档 |
| E4 | 未验证（推断或假设） | 标注为「待 M0 实测」 |

核实日期：**2026-11-16**。下载的源码包为临时材料，已从仓库删除，可按本文方法复现。

---

## 2. 逐项验证

### 2.1 音频 I/O：`cpal` 0.18.2 ✅ 采用

| 事实 | 证据 |
|---|---|
| 29 159 831 次下载，MIT/Apache，2026-08-16 更新 | E2 |
| **Android 后端是 AAudio**：包内存在 `src/host/aaudio/mod.rs`、`convert.rs`，以及 `java_interface/{audio_manager,devices_info,audio_features,utils}.rs` | E1 |
| Android 目标依赖 `jni`、`ndk`、`ndk-context`、`libc`、`audio_thread_priority` 0.35（optional）、`num-traits` | E1（Cargo.toml 里 cfg(target_os = android) 的依赖块） |
| 桌面后端齐全，`realtime` feature 走 `audio_thread_priority` | E1 |

**影响**：v0.2 把「Android 用 AudioRecord + JNI 桥接」改成「复用 cpal」是有依据的——AAudio 正是 Android 的低延迟路径（相对 Java `AudioRecord` 的常规路径）。

**风险 / 待验证**：

- `java_interface` 需要 Android `Context`（经 `ndk-context` 注入），Tauri 集成时初始化顺序要小心；
- AAudio 的实际 burst 大小与 `performanceMode` 未在 cpal 层暴露成显式选项 → **M0-G2** 必须实测环回延迟；
- 若实测 > 40 ms，出路是 `oboe` 0.6.1 直连（该包 2024-03 后停更）或自己 JNI 设低延迟模式 + 独占。

### 2.2 无锁缓冲与重采样：`rtrb` 0.4.1 / `rubato` 5.1.0 ✅ 采用

- `rtrb`：15 628 638 次下载，MIT/Apache，2026-10-25（E2）。SPSC、无锁、可预分配，正是回调线程与处理线程之间需要的东西。
- `rubato`：15 691 122 次下载，2026-11-12（E2）。异步重采样，适合 44.1↔48 kHz 边界转换。
- 注意：`rubato` 只解决「采样率不一致」，**不解决时钟漂移**。漂移补偿要在抖动缓冲层做（见 2.6 与 D7）。

### 2.3 降噪

#### `deep-filter-rt` 0.2.0 ⚠️ 采用（质量档），但必须带降级路径

| 事实 | 证据 |
|---|---|
| MIT OR Apache-2.0；2 450 次下载；0.1.0 首发于 2026-11-01，0.2.0 于 2026-11-07 | E2 |
| **NOTICE.md 声明**：这是 DeepFilterNet 的 `libDF` 在 commit `d375b2d` 的 vendored 副本 → 来源可追溯，非野生重写 | E1 |
| **模型以 ONNX 随包内置**：`enc.onnx` 1.9 MB、`erb_dec.onnx` 3.2 MB、`df_dec.onnx` 3.3 MB（合计约 8.4 MB 编进二进制） | E1 |
| 推理依赖 **`tract-core` + `tract-onnx` + `tract-pulse`**，另有 `ndarray`、`realfft`、`itertools`、`rust-ini`、`log`、`anyhow` | E1（Cargo.toml） |
| `edition = 2024`，**`rust-version = 1.96`** | E1 |
| 帧长 480 样本（10 ms @48 kHz）；`delay()` = **1440 样本 = 30 ms**（10 ms STFT 重叠 + 2 帧前瞻） | E1（`src/tract.rs:314`、`src/lib.rs:5`） |
| CHANGELOG 0.2.0 修复：数字静音 78 秒后输出 NaN、post-filter 最高频点、0 dB 时静音；并声称处理代码零分配 | E1/E3 |

**影响**：算法延迟 30 ms 是硬性的，与附录 A 预算一致；模型内置意味着**无需运行时下载**（对离线与隐私有利）。

**风险**：

1. **tract 是纯 Rust ONNX 运行时**：编译时间显著，ARM 上推理性能**未知**，是 D5 的核心验证点；
2. 产物体积 +8.4 MB（桌面可接受，APK 需评估）；
3. MSRV 1.96 / edition 2024 → CI 与 Android NDK 工具链都要跟上；
4. 版本极新（两周内多次发版）→ 锁小版本，升级前跑离线回归。

**建议**：做成**非默认** feature `denoise-dfn3`，只在桌面与高端 Android 打开；若 tract 不达标，改为自行封装 libDF 或退 RNNoise。

#### `nnnoiseless` 0.5.2 ✅ 采用（低延迟档，兼作 VAD）

| 事实 | 证据 |
|---|---|
| BSD-3-Clause；1 161 424 次下载；2025-12-18 | E2 |
| **`FRAME_SIZE = 120 << FRAME_SIZE_SHIFT(2) = 480`** → 恰好 48 kHz 的 10 ms 帧 | E1（`src/lib.rs:38`） |
| **`pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32`** → 返回值即 VAD 概率 | E1（`src/denoise.rs:95`） |
| 权重内嵌（`src/weights.rnn` 85 KB），纯 Rust，无外部模型 | E1 |

**影响**：不只提供降噪，还免费给一个**语音活动检测**源（说话人高亮、DTX、静音压缩都需要）。v0.2 里「VAD 未提」的缺口可由它填。

#### `sew-noise` 0.10.2 ❌ 不采用

MIT/Apache、2026-11-16 更新，但**仅 12 次下载**，仓库为个人新建（`sew-crates`）。v0.1 打算用它做「统一 Stage 接口 + 档位切换」，等于把架构支点压在陌生新包上。**改为自研 10 行 trait。**

### 2.4 AEC / AGC

| 候选 | 事实 | 结论 |
|---|---|---|
| `sonora-agc2` 0.3.0 | BSD-3；1 338 118 次下载；模块化（`adaptive_digital_gain_controller`、`limiter`、`clipping_predictor`、`fixed_digital_level_estimator`）；注释明确 applies the current gain to **10 ms frames**；edition 2024 / MSRV 1.91 | ✅ **采用**（E1） |
| `webrtc-audio-processing` 2.1.0 | 198 152 次下载；主 crate **无 build.rs**，C++ 在 `webrtc-audio-processing-sys`（+ `-config`）里；`portaudio`、`serde` 为可选 | ✅ **建议加入**（AEC 需要时首选，成熟度最高）；代价是 Android 交叉编译要 C++ 工具链（E1） |
| `sonora` 0.3.0（AEC3） | BSD-3；1 329 992 次下载；纯 Rust 移植 | ⚪ 备选，需实测双讲与收敛 |
| `aec3` 0.4.0 | 63 259 次下载，许可证未逐一取回 | ⚪ 备选 |

**提醒**：AGC 必须与平台 AGC 互斥（Android 的 VOICE_COMMUNICATION 音源常自带 AGC/NS），否则双重处理会出现泵动与呼吸感。

### 2.5 Opus

#### `opus` 0.4.0 ✅ 默认绑定

| 事实 | 证据 |
|---|---|
| 4 115 916 次下载，2026-08-23；绑定的是 **`opusic-sys` 0.7.3**（不是常见的 `libopus-sys`） | E2 + E1 |
| `pub enum Application { Voip, Audio, LowDelay }`，其中 `LowDelay = OPUS_APPLICATION_RESTRICTED_LOWDELAY` | E1（`src/lib.rs`） |
| 我们需要的能力（Voip 模式、10 ms 帧、in-band FEC、码率/复杂度设置）都能表达 | E1 |

风险：C 库交叉编译（Android 需 NDK 工具链）；`opusic-sys` 是否跟随 libopus 上游主线需在 M0 确认。

#### `opus-pure` 0.2.3 ⚪ 比原先估计的成熟，值得 A/B

| 事实 | 证据 |
|---|---|
| BSD-3-Clause；93 277 次下载；2026-11-07；110 个 `.rs`、2 MB 源码 | E2 + E1 |
| **`Application::RestrictedLowDelay = 2051`**；README 明确给出「追最低延迟：RestrictedLowDelay + 2.5/5/10 ms 帧」的用法 | E1（`src/config.rs:32`、`src/encoder.rs:386`、README:307） |
| 编码器实现含 lookahead / pre-skip 处理 | E1（`examples/encode.rs:63`） |
| 仓库自带 `docs/interop-validation.md`（51 KB）做与参考实现的逐项对齐验证 | E1 |

**结论**：可作为纯 Rust 备选，消除 C 工具链依赖；但仍需 M0 对比语音质量、ARM CPU 与包兼容性。不支持把它当默认。

**共同事实**：两个实现都逃不掉 48 kHz 下 6.5 ms 编码前瞻（312 样本），预算必须计入（设计文档附录 A 第 5 行）。

### 2.6 抖动缓冲：`neteq` 0.9.1 ⚠️ 采用，但有两个必须先处理的事实

| 事实 | 证据 |
|---|---|
| MIT OR Apache-2.0；125 433 次下载；2026-08-16；源自 `security-union/videocall-rs` | E2 |
| **具备完整 NetEQ 时间伸缩三件套**：Accelerate / Pre-emptive Expand / Classical Expand；每 10 ms tick 在 Normal/Accelerate/Pre-emptive Expand/Expand 间决策 | E1（README:13/67/95、`src/time_stretch.rs` 22 KB） |
| 有 `buffer_level_filter`、`delay_manager`、`statistics` → 缓冲深度自适应与统计齐备 | E1 |
| **`default = [native, audio_files]`，`native` 含 `cpal`、`clap`、`axum`、`tokio`、`tower`、`tower-http`、`env_logger`、`rand`、`ropus`** | E1（Cargo.toml 的 features 段） |
| 解码器实现是 `ropus`（纯 Rust Opus 解码器，14 674 次下载） | E1/E2 |

**必须做**：`neteq = { version = "0.9", default-features = false }`，否则一个 HTTP 服务器全家桶会进实时音频 crate。

**待验证（D7）**：

1. 关掉 `native` 后是否仍可用？还是它只提供「包→PCM」一体接口、必须接受 `ropus` 解码？若必须接受，就要决策「用 ropus 解码」还是「neteq 只当缓冲、自己解码」；
2. 时间伸缩能消化 ppm 级慢漂移，但**没有显式重采样**——长时漂移仍需外层补偿器，除非 4 小时长跑证明够用；
3. `buffer_level_filter.rs:106` 有 `sample_rate_hz == 0` 的除零保护分支，说明接口允许未设采样率 → 初始化必须显式传 48 000。

`jittr` 0.2.0（MIT，4 334 次下载，2023-01 后停更，二叉堆、无时间伸缩）→ 只适合当最小缓冲参考实现。

### 2.7 语音加密：`chacha20poly1305` 0.11.0 ✅ 采用

- RustCrypto AEADs，116 298 201 次下载，2026-08-05（E2）。key 32 B / nonce 12 B / tag 16 B。
- **禁止**：任何 OCB2 实现（含从 Mumble 代码移植）。OCB2 已于 2018 年被证明可选择性伪造，Mumble 用它属历史包袱。
- 备选 AES-GCM：ARMv8 有 AES 指令，吞吐可能更高 → 与 D9 一起测（语音包很小，ChaCha20 软实现通常足够）。

### 2.8 控制面与网络栈

| 候选 | 事实 | 结论 |
|---|---|---|
| `rustls` | 1 128 449 999 次下载；**crates.io 当前最大版本是 `0.24.0-dev.3`（预发布）** | ✅ 用 0.23 稳定线；依赖里不要写 0.24-dev（E2） |
| `tokio` | 1 198 969 537 次下载，1.54.1 | ✅ 控制面（E2） |
| `quinn` | 423 813 089 次下载，0.11.12 | ⚪ 仅在 Q4 选 QUIC 时引入（E2） |
| `cargo-deny` | 7 580 292 次下载，0.20.2 | ✅ **必须进 CI**，防 AGPL 与重复许可证混入（E2） |

### 2.9 明确不可用清单

| 名称 | 实际情况 | 处置 |
|---|---|---|
| `buzz-relay` | crates.io 上**不存在** | 若指 GitHub 项目需注明仓库；v1 不需要（服务器即中继） |
| `voicemcu` | **不存在** | v1 不做 MCU |
| `svod`（gtcrn 推理） | **不存在** | 若需超轻量降噪，需重新找候选 |
| `dtln-rs` | **不存在** | 仅作思路参考 |
| `mumcord-codec` | 0.9.0，**AGPL-3.0-only 已确认** | ❌ 不得链接 |
| `sew-noise` | 0.10.2，**12 次下载** | ❌ 不承重 |
| `punch-hole` | 0.1.0，**13 次下载**（实现其实完整：NAT 分类 + 策略协商 + 返回 tokio UdpSocket，零信令零身份） | ❌ v1 不引入；未来做 P2P 时是最佳起点 |

---

## 3. 跨 crate 的组合风险

| 风险 | 具体表现 | 对策 |
|---|---|---|
| **MSRV 被抬高** | `deep-filter-rt` 1.96、`sonora-agc2` 1.91、`punch-hole` 1.88，多包已用 edition 2024 | 统一最新 stable + `rust-toolchain.toml` 锁定；Android NDK 同步升级 |
| **编译时间与体积** | tract 全家 + 8.4 MB ONNX + `opusic-sys` C 构建 | feature 拆分，`denoise-dfn3` 仅桌面/高端安卓；CI 缓存 target |
| **feature 污染** | `neteq` 默认拉 axum/tokio/cpal | 一律 `default-features = false`；CI 用 `cargo tree` 断言实时 crate 无 tokio |
| **C 工具链** | `opusic-sys`、`webrtc-audio-processing-sys` 都要 C/C++ 交叉编译 | 先上 `opus`，AEC 按需再引入；提前跑 Android target 构建 |
| **包龄/成熟度** | `deep-filter-rt` 0.2.0、`neteq` 0.9.x 都在快速迭代 | 锁小版本 + WAV 离线回归 + 网络模拟回归作为升级闸门 |
| **帧长契约** | DFN3 480、nnnoiseless 480、sonora-agc2 10 ms、neteq 10 ms tick | 天然一致 → **反向支持「10 ms 为统一处理帧」的决定**；2.5/5 ms Opus 帧反而破坏一致性 |

---

## 4. 建议的 workspace 依赖骨架

```toml
[workspace.dependencies]
# 音频 I/O 与基础
cpal = "0.18"
rtrb = "0.4"
rubato = "5"

# 降噪
nnnoiseless = "0.5"                                  # 低延迟档 + VAD，默认启用
deep-filter-rt = { version = "0.2", optional = true } # 质量档，feature = denoise-dfn3

# AEC / AGC
sonora-agc2 = "0.3"
# webrtc-audio-processing = { version = "2.1", optional = true }  # feature = aec，需 C++ 工具链

# 编解码
opus = "0.4"                                         # 备选 opus-pure = "0.2"（feature = opus-pure）

# 抖动缓冲（必须关默认 feature）
neteq = { version = "0.9", default-features = false }

# 加密与传输
chacha20poly1305 = "0.11"
rustls = "0.23"                                      # 勿用 0.24-dev
tokio = { version = "1.54", features = ["rt-multi-thread", "net", "time", "macros"] }
```

`crates/audio-core/Cargo.toml` 的 feature 建议：

```toml
[features]
default = []
denoise-rnn = ["dep:nnnoiseless"]        # 各平台默认
denoise-dfn3 = ["dep:deep-filter-rt"]    # 桌面/高端安卓
aec = ["dep:webrtc-audio-processing"]
```

---

## 5. 待验证清单（映射设计文档附录 D）

| # | 待验证 | 方法 | 通过标准 | 实验 |
|---:|---|---|---|---|
| 1 | cpal AAudio 真实 burst / 环回延迟 | 两台安卓机实测 | ≤ 40 ms p95 | D2 |
| 2 | `deep-filter-rt` 在 ARM 的推理耗时 | 低端机跑 1 分钟音频 | ≤ 4 ms / 10 ms 帧 | D5 |
| 3 | tract 首次编译时间与产物体积 | 冷构建计时 + du | 记录基线；APK 增量 < 15 MB | D11（新增） |
| 4 | `neteq` 关默认 feature 后的可用 API | 读导出 + 试编译 | 能只用缓冲、不用其解码 | D7 |
| 5 | `neteq` 长时漂移消化能力 | 4 小时注入 ±100 ppm 漂移 | 缓冲深度不单调漂移 | D7 |
| 6 | `opus` vs `opus-pure` 质量与 CPU | 同段语音 ABX + CPU | 差异可接受才可换纯 Rust | D8 |
| 7 | `webrtc-audio-processing` 的 Android 交叉编译 | CI 跑 aarch64-linux-android | 构建通过、体积可接受 | D12（新增） |
| 8 | ChaCha20-Poly1305 vs AES-GCM 吞吐 | 单核 pps 基准 | ≥ 20 万 pps | D9 |

---

> **使用纪律**：E1 项可当作稳定事实；E2 项依赖 crates.io 元数据（会变，重核日期见文首）；E4 与「待验证」项**一律不得在代码里当作已确定结论**。新增任何依赖前，在本报告补一行并写明证据等级。
