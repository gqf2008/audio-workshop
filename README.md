# 音频作坊 · audio-workshop

**本地优先的音频工作台**：自己的声音、自己的机器、自己的素材——不按字收费。

三主线统一设计（配音 / BGM / 音乐制作），另有「人声分离」「音色设计」两个 Tab，**配音先行**。立项依据、实测基线、风险与路线图见 [CHARTER.md](CHARTER.md)。

> 当前阶段：**M2 已稳定；M4 歌曲彩蛋开发中**。配音 + BGM 主链路已可用；歌曲场景已接入
> yue2/ACE-Step 文生歌与 sheetsage2→yue2 翻唱（音乐制作 Tab 提供「文生歌 / 翻唱」模式切换，
> 翻唱固定 yue2 并选源音频），作为质量未达主线产品级的彩蛋能力。

## 本仓库（开源层）

| 路径 | 作用 |
|---|---|
| `src/` + `ui/` | **桌面壳**（Rust + Slint 1.17 + slint-pixel）：配音 + BGM 真实链路 |
| `crates/aw-core/` | **核心库**：切句 / 文本兜底 / 服务客户端 / 逐句合成 / 拼装 / BGM 生成与混音（与 Python 行为由 90 例 parity 夹具固定） |
| `config/models.schema.yaml` | **模型参数化配置**：每个模型的旋钮、已知缺陷登记、文本兜底规则 |
| `config/model-capabilities.json` | 随包**能力清单**（生成物）：`role`/`requires`/`known_issues`/`product_excluded`/`mode`，App 的兜底来源 |
| `tools/audio_config.py` | 配置层（CLI 形态）：渲染服务配置 / 文本兜底 / 按场景端到端执行 |
| `tools/gen_model_capabilities.py` | 从 schema 生成上面那份能力清单（`--check` 校验，纯离线） |
| `tools/audio_dub.py` | 配音链路 CLI（M0）：切句 / 逐句合成 / 拼装 / 单句重录，工程可断点续作 |
| `tools/audio_eval.py` | 评估台：可懂度（ASR 回测 + 字符级对齐）、耗时、峰值内存、回归对比 |
| `tools/model_fetch.py` | **基础模型下载器**（M1 免费）：包装上游 model_manager 拉权重 + 打印手动路径片段 |

设计原则：**技术会进步、模型会换 —— 价值在配置层，不在改模型**。模型不稳的地方（数字读法、不可用变体）由配置声明与兜底，不写死在代码里。

模型的能力/硬要求（哪个引擎必须给参考音频、哪个变体不可用）由 `config/models.schema.yaml` 声明，经**两条路**投递到 App：`server.json`（显式、优先级高）与上面那份随包能力清单（兜底）。所以**不需要**先跑 `audio_config.py render --write` 才有提示；详见 `docs/model-capabilities.md`。

引擎是 [audio.cpp](https://github.com/0xShug0/audio.cpp)（Apache 2.0），当前为**独立仓库 checkout（v0.7.3）**，submodule 化待落；适配只写在本仓库的薄层。

## 快速开始

### 桌面壳（主入口）

前置：`audiocpp_server` 在跑（默认 `http://127.0.0.1:8080`，可用 `AW_SERVER` 覆盖），
音色清单优先从 `AW_SERVER_CONFIG` 读取；否则按平台查找
（macOS 保留 `~/.local/opt/audio.cpp/server.json`，Linux 用 `~/.config/audio.cpp/server.json`，
Windows 用 `%APPDATA%\\audio.cpp\\server.json`）。

**平台要求（系统对话框）**：应用里的「选文件 / 选目录」用各平台**自带**的程序拉起——
macOS `osascript`、Windows PowerShell、Linux **`zenity`**（不随本应用分发，要自己装）：
Debian/Ubuntu `sudo apt install zenity`、Fedora `sudo dnf install zenity`、
Arch `sudo pacman -S zenity`。缺了它**不会**静默失败：状态行会如实说「系统对话框不可用」
并给出这条安装命令；macOS 上被「系统设置 → 隐私与安全性 → 自动化」拒绝时同样会给授权指引
（用户取消与选择器起不来是两回事，判定与文案都在 `src/picker.rs` 一处）。

```bash
cargo run --release        # 打开配音工作台
```

流程：粘稿（或示例稿）→ 点「开始合成」→ 逐句状态流转 → 点句子试听 / ↑↓ 翻句 /
空格 播放停止 → 配音页「高级」里导出 WAV / SRT（右上角抽屉是全局设置：外观 / 工程 / 服务 / 模型） → 切到 BGM 场景写描述，生成并混音 → 导出三轨。

- **断点续作**：工程逐句落盘（`~/Documents/音频作坊/projects/<工程名>/`），
  重开 / 重跑自动跳过已合成句。
- **单句重录**：行内「重录」换 seed 只重跑该句；时间轴点击 = 从那句开始听。
- **音色克隆**：配音页旁白块内「换音色 → 参考音频」填参考 wav 路径，再填**参考音频的文本**
  （服务端要求两者成对，缺文本会 500）；文本可以点「自动转写」让 ASR 听一遍填好，**但要自己核对**。
  也可以走「音色设计」Tab。详见 `docs/voice-clone.md`。
- **BGM 三轨**：BGM 场景按当前配音工程时长生成 30s 分段，按句子时间轴自动 duck，
  导出 `<工程>_voice.wav`、`<工程>_bgm.wav`、`<工程>_mixed.wav` 和 SRT。
- **跨平台配置**：工程/导出目录使用系统 Documents；模型根可用 `AW_MODELS_ROOT` 覆盖；
  推理后端 `auto` 探测（macOS→metal / `nvidia-smi`→cuda / 其它→cpu），也可用 `AW_BACKEND` 强制。
- 校听倍速即时生效（回放层，不动合成产物）；合成语速是模型参数，属配置层。

### 命令行（M0 链路，仍然可用）

```bash
./tools/audio_config.py check                    # 校验配置与模型落点
./tools/audio_config.py text "报价 1234.56 元" --model audio8-tts
./tools/audio_eval.py                            # 质量评估 + 回归对比
./tools/audio_dub.py new --out 工程目录 ...      # 配音工程（见 --help）
# M2 BGM：对已有配音工程跑完整生成/对齐/duck/mix（真服务）
cargo run -p aw-core --example bgm_run -- ~/Documents/音频作坊/projects/工程名 \
  "温暖克制的科技感口播背景音乐，钢琴与轻电子，无人声，循环友好"
# M4 歌曲：文生歌（yue2 / ACE-Step；ACE-Step 本机内存不够）
#   翻唱（sheetsage2 → yue2 cot=melody）桌面入口在「音乐制作」Tab：切到「翻唱」模式，
#   选源音频后走 transcribe_abc → generate_cover（固定 yue2）。命令行示例仍可用：
#   真机整首已跑通（2026-09-17）：本机 audiocpp_server(metal) + 30.08s 源音频 → 产物 51.76s，
#   RMS=3000.6 非静音，墙钟 3m36s。跑的是 aw-core 的 examples/song_cover（与 Cmd::RunCover
#   同一对调用），不是 GUI 点击路径 —— GUI 接线靠单测 + 只读冒烟，像素级布局仍未目视。
# ACE-Step 的门槛是**可用内存** 9.77 GiB（引擎估算 8.77 GiB + 1 GiB 余量）；
# 本机 16GB 实测（2026-09-17 三次采样，可用值是变量不是硬件常量）：
#   7.32 GiB（TTS+ASR 常驻）/ 8.24 GiB（全部卸载后）/ 6.60 GiB（复核时）
#   → 都低于门槛，预检直接拒绝；需要更大内存的机器或 CUDA 主机（不是配置问题）
cargo run -p aw-core --example song_run -- yue2 歌词.txt "Mandarin Chinese R&B slow jam" out-dir
cargo run -p aw-core --example song_cover -- source.wav 歌词.txt "R&B slow jam" out-dir
```

### 模型下载（基础下载器，免费）

```bash
python3 tools/model_fetch.py --list              # 产品登记的模型 + 本机落点状态
python3 tools/model_fetch.py audio8-tts          # 从上游源拉权重（幂等，可续传）
```

下载器**不另造逻辑**：包装上游 `tools/model_manager_v2.py`（`model_specs/` 是下载链接
的 source of truth），下载后校验落点并打印 `server.json` 手动路径配置片段。
上游 checkout 位置：环境变量 `AUDIOCPP_DIR`，默认本仓库同级的 `../audio.cpp`。

## 本地验证

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # 含 mock 服务的策略测试与 parity 夹具
cargo test -p aw-core -- --ignored  # 需要本机服务在线的真实 e2e
```

发布版无演示后门：`strings target/release/audio-workshop | grep -c AW_UI_STATE` → 0。

## 许可红线

**只做模型下载器，不打包权重。** 部分模型（如非商用许可）不能随发行版分发——这是产品形态的硬约束，见 CHARTER 第 5 节。
