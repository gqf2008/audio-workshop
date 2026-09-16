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

## 2.7 真机复跑：真实歌曲的 RTF 与产物采样率（2026-09-17）

§2.6 那组数据用的是 4.27s 短输入，模型加载（约 6~9s）占比极大，容易让人以为分离很慢。
这次拿**一首真实的 50.92s 双声道 48kHz yue2 生成歌曲**复跑（chunk_seconds=30，本地缓存模型
HTDemucs-ORT，M4/16GB）：

| 构建 | 计算 wall（含模型加载） | RTF（wall / 输入 50.92s） |
|---|---|---|
| `cargo run`（debug） | **51.2s** | ≈ **1.01** |
| `cargo run --release` | **27.2s** / **25.4s**（两次采样） | ≈ **0.53 / 0.50** |

两轨都非静音（release 两次采样的 RMS）：`vocals` ≈ **0.0769~0.0802**、`accompaniment` ≈
**0.0807~0.0841**；人声轨在前 5s 与 45s 后近静音（那两段本来就是纯伴奏），符合歌曲结构。

**发现并修掉的一处偏差：产物采样率被写死成 44.1kHz**（上一版我诊断错了，这里保留记录）

第一版我把它当成"上游为补齐最后一块生成的尾巴"，做的是**裁回输入时长**——**那是错的**：
它会把最后 8.8% 的**真实内容**裁掉。复核之后做了一次对齐测量（同一首歌，输入 50.919s /
48kHz / 2ch / 2444096 帧），三种比法：

测量方法（可复算）：取左声道，**20ms 窗平均绝对值**做包络，逐窗算 Pearson 相关系数。

| 读法 | 与输入包络的相关系数 |
|---|---|
| 按各自声明的采样率读（= 按墙钟位置对齐） | 0.268 |
| **把产物按输入采样率（48k）重读**（= relabel 之后） | **vocals 0.800 / accompaniment 0.822** |

（第一版文档里还有一行"把时间轴按 48/44.1 压缩后再比 = 0.792"，那是**按包络下标插值**的口径，
复核按常见包络/重采样口径复原不出来 —— 已删掉，只留两行可复现的。两行已经足够区分
"内容变多"与"标签错"：如果只是尾巴变长，relabel 不会让相关系数从 0.27 跳到 0.82。）

而且两轨的**帧数与输入完全一致**（2444096）。结论：上游**不做重采样、保留输入的帧数/时间轴**，
但把输出标签写成**模型自己的采样率 44100**（`splitter.rs` 里输出 `sample_rate` 取自模型 manifest；
`audio.rs` 只是按上层传入的值写头）—— 48kHz 输入因此得到"同帧数、44.1kHz"的产物：
**时长 +8.84%、播放被拉慢 1.0884 倍**。它不是多了尾巴，是**标签错了**。

`separate_tracks` 现在的做法是 **relabel**（把两轨标签改回输入的采样率，样本一个不动）：

- 这是**无损往返**：上游喂给模型的是"输入样本按 44.1kHz 播"的慢放版本，产物的**帧索引与输入
  帧索引对齐**（复核逐样本比对过：旧产物与 relabel 后产物 `array_equal`，最大绝对差 0），
  改回标签即可 1:1 还原（比真做一次重采样更保真——不引入抗混叠滤波损失）；
- 真机复验（release）：两轨 **48000Hz / 2ch / 2444096 帧 / 50.919s**，与输入逐项一致；
  与输入的包络相关系数 **0.800 / 0.822**（按 44.1kHz 读只有 0.268）；
- 只在**拿得到输入采样率**时做（wav）；mp3/flac 的采样率在上游解码器里，这两种输入沿用上游
  标签，并在界面/CLI 里说明「时长与速度可能与源不一致」（`SeparatedTracks.note`）；
- wav 却读不出采样率（损坏/权限）也算一类说明报出去，不静默跳过；
- 只处理 16-bit PCM（该模型产物就是 16-bit），其它位深原样返回；
- 临时文件用 `*.rate` 显式命名，创建/读/写/finalize/rename 任一步失败都会清掉它；
- **上游该修的地方**：`separate_stems_internal` 输出时把 `sample_rate` 传成**模型**的 44100，
  而输入并没有被重采样到模型采样率。正确的修法二选一：① 输出时保留输入的采样率标签；
  ② 在输入阶段重采样到模型采样率（那样输出 44.1kHz 也是对的）。这属于上游的 bug，值得提
  issue/PR（本仓库只做 relabel 兜底，不改上游）。

**两轨 `.part` 的失败清理**（这一条与上面的诊断无关，是复核另外抓到的既有洞）：上游 `write_audio`
是先建目标文件再逐样本写，中途 ENOSPC/权限变化会留下半截文件。复核用 8MB 受限卷复现过
"人声轨失败后留下 8.1MB `.part`"，现在 `write_stems_with_cleanup` 保证**任一轨失败都不留 `.part`**
（含"人声成功、伴奏失败"这种半套）。

## 4. 接入方案（**已实现**；文件名/消息名以代码为准）

> 实现落地在两处：`crates/aw-core/src/separate.rs`（后端）与 `ui/separation_workbench.slint`
> + `src/main.rs::wire_separation`（页面与接线）。本节早期版本写的是"改 `ui/extra_tabs.slint`、
> 消息名 `SeparationProgress|Done|Failed`"，实际改成独立页面文件 + 单一 `Msg::Separation*`
> 系列；另外停止用的是**分离自己的** `sep_stop` 标志（与配音共用会把两边的停止请求互相吃掉，
> 审查抓到过）。以下原文保留作设计意图记录。

1. `crates/aw-core` 新增 `separate.rs`，依赖 `stem-splitter-core`：
   - `Separator` 复用（模型只加载一次，避免每次分离都吃一遍模型加载）；
   - `set_split_progress_callback` / `set_download_progress_callback` → 转发成我们自己的进度事件；
   - 支持**停止**：上游没有 stop 参数。**2026-09-17 已核实**：`stem-splitter-core` rev `9120251`
     的 `src/core/splitter.rs` 里没有任何 `cancel` / `AtomicBool` / `should_stop`（grep 无命中），
     所以"chunk 粒度取消"做不到——实现是把它放进独立线程，`should_stop` 只在整轮
     `Separator::separate` 返回后查一次（`separate_tracks`）：停止 = **跑完整轮再丢弃结果、不落盘**，
     耗时照算。UI 文案必须照这个写，不能说成"当前分块跑完就停"。
2. 模型来源两条路，都接到全局设置：
   - 首次使用：上游自动下载到用户目录（带进度，需联网一次）；
   - 离线/内网：用全局设置的**模型目录**放 `*.onnx`，走 `SplitOptions.model_path` 跳过下载。
   这条正好复用 v1.4 已有的「模型目录」设置，不需要新概念。
3. `src/main.rs`：新增 `Cmd::RunSeparation` / `Msg::SeparationProgress|SeparationDone|SeparationFailed`，
   接入统一任务队列（见下）。
4. `ui/extra_tabs.slint`：把现在的 `GatedTab` 占位换成真正的分离页（拖入区 / 主操作 / 两轨结果 / 高级）。

## 4.5 CoreML 加速 A/B 实测：**更慢且输出静音 → 已否决**（2026-09-17）

上游 `stem-splitter-core` 有 `coreml` feature，而且即使编译进去也**默认关闭**
（`core/src/engine.rs` 里要 `ENABLE_COREML` 环境变量才启用，注释原文写着
"CoreML can sometimes produce silent/zero outputs on certain models"）。
为了不拍脑袋，本机做了 A/B（同一段 4.27s 单声道 44.1k 输入、模型已缓存）：

下表是**本机单次实测**（数字会随推理线程调度小幅波动，第二次独立复现的结果列在括号里）：

| 路径 | 计算耗时 | 人声轨 RMS | 伴奏轨 RMS |
|---|---|---|---|
| CPU（默认 `onednn`） | **13.7s**（复现 13.1s） | 0.13802（复现一致） | ≈0.0009（两次 0.00092 / 0.00095） |
| CoreML（`features = ["coreml"]` + `ENABLE_COREML=1`） | **34.1s（慢 2.5×）**（复现 33.7s，慢 2.6×） | **0.00000** | **0.00000** |

两条结论是硬结论（独立复现一致）：

1. **更慢**（2.5~2.6×）。至于原因——推测是 htdemucs 的算子图以大量小算子为主、
   逐 op 派发开销吃掉了加速收益——**这只是推测**，A/B 只能证明"更慢"，不能证明机制。
2. **输出静音**：全零产物，且**进程 exit 0、没有任何报错** —— 正是上游注释警告的失败模式。
   CPU 跑到 0.138 的人声轨，CoreML 得到 0.000。也就是说这条路不只是"没收益"，
   而是**会静默产出废品**（比慢更危险：用户拿到的是空文件）。

因此**不启用 coreml feature**，保持 CPU + oneDNN。参考量级（同样是**本机单次实测**，
不是恒定值）：RTF ≈ 3.2（4.27s 音频 ≈ 13.1~13.7s 计算），外加约 6~9s 的进程/模型加载。

若将来要再试，先按上表复现：**必须同时比对耗时与产物 RMS**，只看耗时会被"更快"的假象骗到
（这次恰好相反，但静音输出才是更危险的形态）。

## 5. 代价与风险（都是真实的，别在 UI 里藏）

| 风险 | 具体 | 处置 |
|---|---|---|
| 依赖体积与构建时间 | ONNX Runtime + 音频解码栈会显著拉长首次编译（分钟级） | 单独批次做，先量一次 `cargo build` 增量再决定是否默认开启 |
| 首次联网下载 ~200MB | 与"本地优先"口径需要一句话说明 | UI 明写"首次使用需下载模型（约 200MB），之后离线"；给下载进度 |
| 内存/显存 | 官方建议 4GB+ RAM；本机是共享显存的 Apple Silicon | 用 `chunk_seconds` 控制；失败时给可执行原因（内存不足） |
| ~~无内置"伴奏"轨~~ | **已澄清：上游有 `mix_except`**，不需要自己求和 | 直接用 `save_mix_except(&[Stem::Vocals])` |
| 停止不彻底 | 上游无 stop API（已核实：splitter.rs 无任何取消入口） | 实现为"停止 = 整轮跑完再丢弃结果、不落盘"；UI 文案写"本轮分离跑完才会丢弃结果（上游没有取消接口）" |
| 模型许可 | 上游代码 MIT/Apache；**模型权重许可是另一件事**，需单独核对后再对外分发 | 接入前核对模型 manifest 里的许可字段；不确定就不随包分发、只走用户侧下载 |

## 6. 复现本结论的命令

§2.7 那两行相关系数可以用这段脚本复算（输入 = 源音频，产物 = 分离结果）：

```python
import wave, struct, math
def mono(path):
    with wave.open(path) as w:
        sr, ch, n = w.getframerate(), w.getnchannels(), w.getnframes()
        v = struct.unpack(f"<{n*ch}h", w.readframes(n)[:n*ch*2])
    return sr, [v[i] for i in range(0, len(v), ch)]          # 左声道
def env(sig, win):                                            # win = 20ms 的样本数
    return [sum(abs(x) for x in sig[i:i+win])/win for i in range(0, len(sig)-win+1, win)]
def corr(a, b):
    m = min(len(a), len(b)); ma = sum(a[:m])/m; mb = sum(b[:m])/m
    num = sum((x-ma)*(y-mb) for x, y in zip(a[:m], b[:m]))
    da = math.sqrt(sum((x-ma)**2 for x in a[:m])); db = math.sqrt(sum((y-mb)**2 for y in b[:m]))
    return num/(da*db) if da and db else 0
isr, isig = mono("源.wav"); ssr, ssig = mono("产物.wav")
print("按各自声明采样率读 :", round(corr(env(isig, int(isr*0.02)), env(ssig, int(ssr*0.02))), 3))
print("按输入采样率重读   :", round(corr(env(isig, int(isr*0.02)), env(ssig, int(isr*0.02))), 3))
```


```console
curl -sS https://api.github.com/repos/gqf2008/Xmusic-splitter \
  | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["language"],d["license"]["spdx_id"])'
curl -sS https://raw.githubusercontent.com/gqf2008/Xmusic-splitter/master/src-tauri/Cargo.toml
curl -sS https://api.github.com/repos/gqf2008/stem-splitter-core/contents/src
curl -sS https://raw.githubusercontent.com/gqf2008/stem-splitter-core/master/src/types.rs
curl -sS https://crates.io/api/v1/crates/stem-splitter-core
```
