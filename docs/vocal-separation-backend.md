# 人声分离后端：接入可行性（来源：gqf2008/Xmusic-splitter）

> 状态：**可行性已核实 + 已端到端跑通**（`crates/aw-core/src/separate.rs` + `examples/separate_run.rs`）。
> 本文记录"能不能用、怎么用、代价是什么"，不含臆造的模型名或阈值。
> 核实日期 2026-09-16；被核实的上游仓库当时为最新 master。

## 1. 上游是什么

| 项 | 事实 | 来源 |
|---|---|---|
| `gqf2008/Xmusic-splitter` | Tauri v2 + React 的桌面人声分离工具，本地推理、无遥测、输入输出目录由用户选 | 仓库 README |
| 推理核心 | 不是 Python Demucs，而是 Rust crate **`stem-splitter-core`**（`Cargo.toml` 里以 git rev 固定） | `src-tauri/Cargo.toml` |
| 依赖声明 | `stem-splitter-core = { git = "https://github.com/gqf2008/stem-splitter-core", rev = "9120251a..." }` | 同上 |

## 2. `stem-splitter-core`（可复用的那一层）

- **纯 Rust + ONNX Runtime**，模型是 **htdemucs**（Hybrid Transformer Demucs），**4 轨**输出：vocals / drums / bass / other。
- **已发布 crates.io**：`stem-splitter-core`，最新 `1.2.0`，许可 **MIT OR Apache-2.0**，累计下载 2285（2026-04-13 发布）。
- 自动下载并缓存模型（~200MB，SHA-256 校验），**也支持完全离线的本地模型**：
  `SplitOptions.model_path: Option<String>` 指向本地 ONNX 即可跳过下载。
- 带**进度回调**：`set_split_progress_callback` / `set_download_progress_callback`（`SplitProgress`），
  正好能喂给我们的任务进度条。
- 支持 GPU 加速（CUDA / CoreML / DirectML / oneDNN，自动探测）；macOS 走 CoreML。
- 长音频有分块：`SplitOptions.chunk_seconds`（默认 60s，官方注释写明是"降低内存占用"，内存不足时可调小）。

关键 API（`src/lib.rs`）：

```rust
pub use crate::core::splitter::{split_file, remove_vocals, VocalRemovalResult, Separator, SeparatedStems, Stem};
pub use crate::io::progress::{set_download_progress_callback, set_split_progress_callback, SplitProgress};
pub use crate::model::model_manager::{ensure_model, load_model_from_path, ModelHandle};
pub use crate::types::{AudioData, ModelManifest, SplitOptions, SplitResult};

// SplitOptions { output_dir, model_name, manifest_url_override, model_path, chunk_seconds }
// SplitResult  { vocals_path, drums_path, bass_path, other_path }
pub fn prepare_model(model_name: &str, manifest_url_override: Option<&str>) -> Result<()>;
```

## 3. 与设计稿的对应

设计稿 `docs/ui/redesign-v1/screens.md` §4 要求：拖入区 → `分离人声 / 伴奏` → **两轨结果**（人声 + 伴奏），
每轨可试听 / 导出；模型、阈值、输出进「高级」；能力未就绪时明确写"未接入"且不给假选项。

**伴奏轨不用自己求和**（此处修正早先版本的说法）：上游 `SeparatedStems` 自带
`mix_except(&[Stem::Vocals])` / `save_mix_except(...)`，正是为"去人声"准备的。
我们只需要两次保存：

```rust
stems.save(Stem::Vocals, ".../xxx_vocals.wav")?;
stems.save_mix_except(&[Stem::Vocals], ".../xxx_accompaniment.wav")?;
```

## 2.5 接入时踩到的两个硬坑（都已解决，别重复踩）

| 坑 | 现象 | 处置 |
|---|---|---|
| **crates.io 的 1.2.0 API 不完整** | 只有 `split_file`，没有 `Separator` / `mix_except` / `model_path`（离线模型）/ `chunk_seconds` | 与上游 App 一样改用 **git rev**：`rev = "9120251a64283ff7101662fc4509186c4f6cf274"` |
| **master + ort 默认解析版本编译不过** | `ort = "2.0.0-rc.10"` 被解析成 rc.11 → 报 `no method named map_err found for type bool`、`field inputs of Session is private` | 把 ort 钉到 **`=2.0.0-rc.10`**（`cargo update -p ort --precise 2.0.0-rc.10`，并在 Cargo.lock 里固定） |

## 2.6 实测数据（本机 Apple Silicon，2026-09-16）

| 项 | 实测 |
|---|---|
| 依赖构建 | `cargo check -p aw-core` 冷启 **30.5s**（含 ort 2.0.0-rc.11 解析）；切 git rev + 钉 rc.10 后总量级相当 |
| ONNX Runtime 获取方式 | `ort` 用 `download-binaries` + `copy-dylibs`：**构建期下载**并拷贝 dylib 到 target 旁（打包要带上这个库） |
| 首次模型下载 | **209,884,896 字节**，有逐字节进度回调 |
| 推理后端 | 自动探测 → `Trying execution providers: ["oneDNN"] (with CPU fallback)` → `Successfully initialized session with GPU providers!` |
| 端到端 | 4.27s 单声道 44.1k 输入：**131s**（含 200MB 下载 + 首次加载模型）；两轨写出成功 |
| 分离质量抽样 | 输入是人声 TTS：`vocals` RMS **0.13802**（≈ 输入 0.13820），`accompaniment` RMS **0.00095**（≈ 静音），帧数三者一致（188416）—— 符合"纯语音应全进人声轨"的预期，说明链路真的在分离而不是复制 |

## 4. 接入方案（建议）

1. `crates/aw-core` 新增 `separate.rs`，依赖 `stem-splitter-core`：
   - `Separator` 复用（模型只加载一次，避免每次分离都吃一遍模型加载）；
   - `set_split_progress_callback` / `set_download_progress_callback` → 转发成我们自己的进度事件；
   - 支持**停止**：上游没有 stop 参数，需在 chunk 粒度或线程粒度做取消（待验证其回调能否中断），
     最坏情况是把分离放进独立线程、停止=丢弃结果 + 不再排队后续 chunk（止损不彻底，需在 UI 文案上如实写）。
2. 模型来源两条路，都接到全局设置：
   - 首次使用：上游自动下载到用户目录（带进度，需联网一次）；
   - 离线/内网：用全局设置的**模型目录**放 `*.onnx`，走 `SplitOptions.model_path` 跳过下载。
   这条正好复用 v1.4 已有的「模型目录」设置，不需要新概念。
3. `src/main.rs`：新增 `Cmd::RunSeparation` / `Msg::SeparationProgress|SeparationDone|SeparationFailed`，
   接入统一任务队列（见下）。
4. `ui/extra_tabs.slint`：把现在的 `GatedTab` 占位换成真正的分离页（拖入区 / 主操作 / 两轨结果 / 高级）。

## 5. 代价与风险（都是真实的，别在 UI 里藏）

| 风险 | 具体 | 处置 |
|---|---|---|
| 依赖体积与构建时间 | ONNX Runtime + 音频解码栈会显著拉长首次编译（分钟级） | 单独批次做，先量一次 `cargo build` 增量再决定是否默认开启 |
| 首次联网下载 ~200MB | 与"本地优先"口径需要一句话说明 | UI 明写"首次使用需下载模型（约 200MB），之后离线"；给下载进度 |
| 内存/显存 | 官方建议 4GB+ RAM；本机是共享显存的 Apple Silicon | 用 `chunk_seconds` 控制；失败时给可执行原因（内存不足） |
| ~~无内置"伴奏"轨~~ | **已澄清：上游有 `mix_except`**，不需要自己求和 | 直接用 `save_mix_except(&[Stem::Vocals])` |
| 停止不彻底 | 上游无 stop API | 先按"停止=不再排队 + 丢弃结果"实现，UI 文案写清"正在处理的分块会跑完" |
| 模型许可 | 上游代码 MIT/Apache；**模型权重许可是另一件事**，需单独核对后再对外分发 | 接入前核对模型 manifest 里的许可字段；不确定就不随包分发、只走用户侧下载 |

## 6. 复现本结论的命令

```console
curl -sS https://api.github.com/repos/gqf2008/Xmusic-splitter \
  | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["language"],d["license"]["spdx_id"])'
curl -sS https://raw.githubusercontent.com/gqf2008/Xmusic-splitter/master/src-tauri/Cargo.toml
curl -sS https://api.github.com/repos/gqf2008/stem-splitter-core/contents/src
curl -sS https://raw.githubusercontent.com/gqf2008/stem-splitter-core/master/src/types.rs
curl -sS https://crates.io/api/v1/crates/stem-splitter-core
```
