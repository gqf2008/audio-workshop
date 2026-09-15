# 音频作坊 · audio-workshop

**本地优先的音频工作台**：自己的声音、自己的机器、自己的素材——不按字收费。

三场景统一设计（配音 / BGM / 歌曲），**配音先行**。立项依据、实测基线、风险与路线图见 [CHARTER.md](CHARTER.md)。

> 当前阶段：**M1** —— 最小桌面壳：完整配音工作台（句子列表 + 校听器 + 键盘翻句）
> + 基础模型下载器（免费）+ 本地音色克隆（voice_ref）。完成判据：脱离命令行可自用。

## 本仓库（开源层）

| 路径 | 作用 |
|---|---|
| `src/` + `ui/` | **桌面壳**（Rust + Slint 1.17 + slint-pixel）：配音工作台真实链路版 |
| `crates/aw-core/` | **核心库**：切句 / 文本兜底 / 服务客户端 / 逐句合成 / 拼装（与 Python 行为由 90 例 parity 夹具固定） |
| `config/models.schema.yaml` | **模型参数化配置**：每个模型的旋钮、已知缺陷登记、文本兜底规则 |
| `tools/audio_config.py` | 配置层（CLI 形态）：渲染服务配置 / 文本兜底 / 按场景端到端执行 |
| `tools/audio_dub.py` | 配音链路 CLI（M0）：切句 / 逐句合成 / 拼装 / 单句重录，工程可断点续作 |
| `tools/audio_eval.py` | 评估台：可懂度（ASR 回测 + 字符级对齐）、耗时、峰值内存、回归对比 |
| `tools/model_fetch.py` | **基础模型下载器**（M1 免费）：包装上游 model_manager 拉权重 + 打印手动路径片段 |

设计原则：**技术会进步、模型会换 —— 价值在配置层，不在改模型**。模型不稳的地方（数字读法、不可用变体）由配置声明与兜底，不写死在代码里。

引擎是 [audio.cpp](https://github.com/0xShug0/audio.cpp)（Apache 2.0），当前为**独立仓库 checkout（v0.7.3）**，submodule 化待落；适配只写在本仓库的薄层。

## 快速开始

### 桌面壳（M1 主入口）

前置：`audiocpp_server` 在跑（默认 `http://127.0.0.1:8080`，可用 `AW_SERVER` 覆盖），
音色清单从 `~/.local/opt/audio.cpp/server.json` 发现（可用 `AW_SERVER_CONFIG` 覆盖）。

```bash
cargo run --release        # 打开配音工作台
```

流程：粘稿（或示例稿）→ 点「开始合成」→ 逐句状态流转 → 点句子试听 / ↑↓ 翻句 /
空格 播放停止 → 抽屉里导出 WAV / SRT。

- **断点续作**：工程逐句落盘（`~/Documents/音频作坊/projects/<工程名>/`），
  重开 / 重跑自动跳过已合成句。
- **单句重录**：行内「重录」换 seed 只重跑该句；时间轴点击 = 从那句开始听。
- **音色克隆**：抽屉「音色」填参考 wav 路径（index-tts2 的 voice_ref）。
- 校听倍速即时生效（回放层，不动合成产物）；合成语速是模型参数，属配置层。

### 命令行（M0 链路，仍然可用）

```bash
./tools/audio_config.py check                    # 校验配置与模型落点
./tools/audio_config.py text "报价 1234.56 元" --model audio8-tts
./tools/audio_eval.py                            # 质量评估 + 回归对比
./tools/audio_dub.py new --out 工程目录 ...      # 配音工程（见 --help）
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
