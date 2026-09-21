# 边合成边校听（流式增量）· 真机探测与设计

> 2026-09-21。对应线程 `cc-ai-audio-workshop-streaming-preview`；来源 `docs/product-plan.md` 点名
> 的 v0.1 漏项：「边合成边校听的现成增量机制（`audio8-tts-stream` → `/v1/audio/speech/live`）」。
> **本文只写实测到的事实与据此的设计判断**；悬而未决的部分列成上游前置条件，不按已实现宣传。

## 1. 目标

对齐 CHARTER 成功标准第 3 条（5 分钟稿人工投入 < 10 分钟）：**合成与校听并行**，人工时间从
「等完 + 听」变成「边出边听」；挂钟时间口径不变（≥ 合成总时长，见 CHARTER §2 定标备注）。

## 2. 真机探测结论（决定性）

权威对象是**随包引擎 v0.8.2-metalbf16**（`audio.cpp 0.8.2-metalbf16 git: 5356843`），
独立端口 18082 只挂 `audio8-tts-stream` 探测；本机 8080 上另有用户开发版引擎
（`e83e9537 breeze-bf16-metal`）作对照，行为同类。原始产物见 §5。

### 2.1 契约（读 v0.8.2 源码 + 实测）

```text
POST /v1/audio/speech/live?model=<id>&input=<text>&sample_rate=24000
     &stream_format=audio|sse[&voice=..][&reference_text=..][&seed=..]
```

- **请求体必须 chunked**：非 chunked 直接 400 `live speech requires an incrementally delivered body`；
  TTS 可发空 chunked 体（`0\r\n\r\n`）。
- 只接受 `mode=streaming` 的模型（否则 400）；`response_format` 只支持 `pcm`。
- **不支持 `voice_ref` 路径**：query 与 body 都没有这个字段；`voice` 只按服务端预设/voice 库名解析
  （`--voice-dir`）或 cached voice id。**克隆音色走 live 目前无路**。
- `stream_format=audio`：裸 PCM S16LE chunked；`stream_format=sse`：`speech.audio.delta`（base64 PCM）
  × N → `speech.audio.done`（timing）→ `data: [DONE]`。

### 2.2 实测（bundled v0.8.2，模型已热）

| 用例 | 结果 |
|---|---|
| SSE，短句 | 200 → **1 个 delta（整段 6.4s 音频）** → `{"type":"error",…,"message":"live streaming response did not observe input end"}`；**没有 done timing** |
| SSE，长句（6 句） | 200 → 19.4s 后直接 `error: live streaming response did not observe input end`，**0 个 delta** |
| audio，短句 | 3.74s 全部 PCM 一次到达（213,006B ≈ 4.44s 音频）：**first_arrival == total** |
| audio，长句 | 26.33s 全部 PCM 一次到达（1,384,463B ≈ 28.8s 音频）：**first_arrival == total** |

### 2.3 结论

1. **v0.8.2 的 live 输出不是渐进的**：短句/长句都是整段生成完才一次写出，首发延迟 ≈ 离线整句合成时间。
   「首句提前出声」在当前引擎上**不成立**。
2. **SSE 分支在 TTS 场景有 bug**：`input_end` 只在模型读 PCM 输入时被观测（TTS 不消费音频输入），
   末尾必报 `did not observe input end`，且可能吞掉 done 事件。修需上游（见 §4）。
3. `stream_format=audio` 是当前唯一可用形态（不报错），但没有渐进收益。
4. **真正可行的是复用现有逐句离线管线**：`/v1/tasks/run` 已经逐句合成、逐句落盘
   `sentences/NNN.wav`，并逐句回传 `Msg::Sentence`；代码核实 `play_sentence`（`src/main.rs`）
   **没有 running 守卫**、行内「试听」按钮运行中也一直可点——底子已在，缺的是把它变成一等体验。

## 3. Phase 1 设计：逐句边合成边校听（本次实现方向）

**目标体验**：合成进行中，已完成句可随时试听；不必等整篇拼装。

现状链路（已核实）：

```
worker: 逐句 /v1/tasks/run → sentences/NNN.wav 落盘 → 逐句 Msg::Sentence 回传
UI:     行状态滚动更新（已合成）；行内「试听」→ play_sentence（只查「已合成 + wav 在」）
        「重录」enabled: !busy（运行中禁用，保持）；全篇试听 play_all（成品优先，否则按已合成句顺序播）
```

变更点（按最小改动排序）：

1. **显式化提示**：合成进行中，句子区/状态行写明「已完成句可直接试听，不必等跑完」；
   行内「试听」保持可用（**不**跟「重录」一起被 busy 禁）。
2. **运行中的连播**：新增/复用「从这句开始连播已完成句」；播到第一句未完成时**停下并说明**
   （不静默跳过、不排队等它）。
3. **互不干扰**：试听/连播不触发停止、不清任务；停止合成不杀正在播放的音频（反之亦然），
   两种状态在状态行分别可见。
4. **选择与进度解耦**：合成中点击某行只影响选中/试听，不改 worker 的目标句；「重录」运行中仍禁用。

验收（Phase 1）：

- 单测：`play_sentence` 在 `running` 下的两条分支（已完成可播 / 未完成给可执行文案）；
  运行中连播只包含已完成句且到未完成句停下；停止与播放互不改变对方状态。
- 真机：5 分钟稿跑到第 3 句时，第 1 句点「试听」能出声；连播到未完成句停下并有说明；
  合成结束后「全篇试听」仍优先播 `final.wav`。
- 不回归：`重录` 运行中禁用、停止语义、导出/质检判据不变。

## 4. Phase 2 候选（需要上游/更多设计，不在本批）

**live API 接入的前置条件（给上游的最小修复清单）**：

1. TTS 模型不消费 PCM 输入时，不应要求 `input_end`（当前 SSE 必报 `did not observe input end`）；
2. 真正的渐进输出：`speech.audio.delta` 应在生成过程中分次到达（当前 `first_arrival == total`）；
3. live 支持 `voice_ref`（或明确走服务端 voice 库的落库协议），否则克隆音色只能在离线链路用；
4. delta → 句子边界/时长的映射规则（流式输出没有句子切分）。

其它候选：自动顺播（下一句一完成自动接播）、边合成边质检（每句完成即 ASR 回读打分）。

## 5. 复现与产物

```bash
# 独立起随包引擎（不碰用户在跑的 8080 服务）
cd /Volumes/DataExt/tmp/aw-stream && ./run-eng.sh        # 端口 18082，只挂 audio8-tts-stream
python3 probe.py cold|warm|long|audio                    # http.client 版探针
# 原始 socket 版（能看到 SSE 的 error 事件与第一字节时刻）
#   见 FINDINGS.md 里的命令与 raw-sse.txt / probe2.sse
```

产物：`/Volumes/DataExt/tmp/aw-stream/FINDINGS.md`、`raw-sse.txt`、`probe2.sse`、`probe.py`、
`engine.log`（含 `audio8_tts.ar_generate_ms` / `codec_decode_ms` 的真实耗时）。
