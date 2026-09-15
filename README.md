# 音频作坊 · audio-workshop

**本地优先的音频工作台**：自己的声音、自己的机器、自己的素材——不按字收费。

三场景统一设计（配音 / BGM / 歌曲），**配音先行**。立项依据、实测基线、风险与路线图见 [CHARTER.md](CHARTER.md)。

> 当前阶段：**M0** —— 把配音链路做到"自己每天能用"。
> 完成判据：连续 7 天每天产出 ≥1 条真实内容；同类环节不退回剪映/ElevenLabs；5 分钟稿 <10 分钟出片。

## 本仓库（开源层）

| 路径 | 作用 |
|---|---|
| `config/models.schema.yaml` | **模型参数化配置**：每个模型的旋钮、已知缺陷登记、`product_excluded` 标记、文本兜底规则 |
| `tools/audio_config.py` | 配置层：渲染服务配置 / 文本兜底（数字规范化 + 发音词典）/ 按场景端到端执行 |
| `tools/audio_eval.py` | 评估台：可懂度（ASR 回测 + 字符级对齐）、耗时、峰值内存、与上次对比的回归检测 |

设计原则：**技术会进步、模型会换 —— 价值在配置层，不在改模型**。模型不稳的地方（数字读法、不可用变体）由配置声明与兜底，不写死在代码里。

引擎是 [audio.cpp](https://github.com/0xShug0/audio.cpp)（Apache 2.0），以 **submodule 锁定 tag** 引入，适配只写在本仓库的薄层。

## 快速开始

前置：本机已装 audio.cpp 服务（`audiocpp_server`），模型放在配置里的 `runtime.models_root`。

```bash
# 1. 校验配置：模型路径是否都在、哪些模型需要额外参数
./tools/audio_config.py check

# 2. 看文本层会怎么改写（含发音词典与年份逐位读）
./tools/audio_config.py text "报价 1234.56 元，2026 年 3 月交付" --model audio8-tts

# 3. 把配置渲染成服务配置（默认只显示差异，加 --write 才写）
./tools/audio_config.py render

# 4. 按场景跑一次：稿子 → 文本兜底 → 合成
./tools/audio_config.py run dub --text "…" --out out.wav

# 5. 质量评估：可懂度 / 耗时 / 内存，自动与上次对比
./tools/audio_eval.py
```

## 许可红线

**只做模型下载器，不打包权重。** 部分模型（如非商用许可）不能随发行版分发——这是产品形态的硬约束，见 CHARTER 第 5 节。
