# 模型暴露与能力契约对齐（`config/models.schema.yaml` → 界面）

只读审计用真机数据挖出的一类缺陷：**配置层已经写明的产品决策与能力契约，Rust 侧没读**，
于是界面暴露了不该暴露的模型、或说了与后端矛盾的话。

## 修的是什么

| # | 真机事实（改前） | 现在 |
|---|---|---|
| 1 | 配音「高级 → 引擎」下拉列出 `product_excluded` 的模型。选 `audio8-tts-01b` **HTTP 200、0.46 秒、回读 ASR 得空串（完全不可懂）、全程无报错** —— 最坏的失败形态 | 下拉只留可选引擎；被排除/流式专用的一律不出现 |
| 2 | 音色区对所有引擎都写「免参考音，开箱可用」，与 `index-tts2` 的 `requires.voice_ref: true` 矛盾（不接 `voice_ref` 直接报错） | 文案与可点性由引擎能力驱动；没给参考音时**禁用**合成并说明原因 |
| 3 | 默认引擎写死 `audio8-tts`：清单里没有它的机器明明有可用 TTS，却显示「没有可用音色」 | **按能力**挑：优先"不需要参考音"的引擎，全都要参考音时退回第一个 |
| 4 | 歌曲引擎 `_ => Yue2` 静默兜底 + UI 选项与下标映射各写死一份：清单里换个 gen 模型会被静默当 yue2 跑 | 选项来自清单（`task == "gen"`）；未知 id **显式报错**，不换引擎 |

## 数据从哪来

App 运行时只读 `server.json`（`AW_SERVER_CONFIG` 可覆盖路径），**不读** `config/models.schema.yaml`。
schema 是产品决策的**唯一作者**，经两条路径投递到 App：`tools/audio_config.py render`
写进 `server.json`（显式、优先级高），以及随包的 `config/model-capabilities.json`
（兜底；**`render --write` 不再是前提**）—— 见 `docs/model-capabilities.md`。

改前 `to_server()` 只透传 `mode` / `product_excluded`，`role` / `requires` / `known_issues`
到不了界面 —— 已同步透传（只加字段，不改任何产品决策）。

App 侧 `ServerModel` 新增（全部 `#[serde(default)]`，**旧清单缺字段 = 不设限**，
换服务端版本不会让 App 突然什么都选不了）：

- `product_excluded: bool`
- `mode: String` / `role: String`
- `requires: { voice_ref: bool }`
- `known_issues: Vec<String>`

## 一处判据，界面只消费

按 `LESSON_同一语义两处实现必然漂移`：同一语义不能有两份实现。

- `is_selectable_tts_engine(&ServerModel)` —— **配音可选引擎的唯一判据**
  （`task == "tts"` 且没被排除且不是流式专用）。下拉、默认引擎都走它。
- `voice_readiness(engine, ref_path, ref_exists) -> VoiceReadiness` —— **能不能开工的唯一判据**
  （`Ready` / `NoEngine` / `ReferenceMissing` / `EngineNeedsReference`）。
  主按钮可用性（`dub-voice-ready`）、内置音色行可点性（`engine-requires-voice-ref`）、
  块头阻断提示（`voice-hint`）全部由这一次判定投影出来，Slint 里**不再重拼条件**。
  （改前 `voice-ready: root.voice-index >= 0` 只看"有没有引擎"。）
- `song_engine_options(&[ServerModel])` —— **歌曲引擎的唯一清单**；UI 选项与
  "下标 → id"都从它派生。`song_model_for_id` 未知 id 返回 `Err`。
- `known_issues` 在引擎行下方**只读**展示（`docs/product-plan.md` §3.3 第 3 条），不参与自动决策。

## 真机数字（本机实际清单）

`AW_UI_STATE=engines` 把**实际清单算出来的**结果打到 stderr（不是代码推断）。

**A. 现有 `server.json`（App 现在真读的那份）**——下拉已从 5 项收敛到 2 项。
本节的数字是**这一批之后**的：`requires` / `known_issues` 靠随包能力清单兜底，
所以不再需要先 `render --write`（详见 `docs/model-capabilities.md`）：

```
清单里 task==tts 共 5 个 → 过滤后剩 2 个
  [0] audio8-tts  · 要求参考音=false · 默认选中=true  · known_issues="数字/电话/金额读法不稳定，…"
      开工判据：Ready
  [1] index-tts2  · 要求参考音=true  · 默认选中=false · known_issues="不接 voice_ref 会直接报错，…"
      开工判据：EngineNeedsReference
  [不出现在下拉] audio8-tts-01b        · product_excluded=true  · mode=offline  · role=""
  [不出现在下拉] audio8-tts-stream     · product_excluded=false · mode=streaming · role=streaming
  [不出现在下拉] audio8-tts-01b-stream · product_excluded=true  · mode=streaming · role=streaming
音乐制作引擎（清单 task==gen）
  yue2（歌词成歌） → yue2
  ACE-Step（文生音乐） → ace-step
  stable-audio-small-music → stable-audio-small-music
```

`audio8-tts-01b` 就是那个"选了会静默产出听不懂的音频"的模型 —— 现在它连选项都不是。
`stable-audio-small-music` 是本次的诚实边界：清单里有个 App 还没接入的 gen 引擎，
界面**照样列出来**（清单说了算），但选择后会在提交时报
「引擎 stable-audio-small-music 还没有接入音乐制作链路」—— **显式失败，不是静默换引擎**。

**B. 按 schema 重新渲染后的清单（在**临时副本**上跑，用户文件不动）**——与 A **逐行一致**：

```sh
cp ~/.local/opt/audio.cpp/server.json /Volumes/DataExt/tmp/…/server-rendered.json
AW_SERVER_CONFIG=/Volumes/DataExt/tmp/…/server-rendered.json \
  python3 tools/audio_config.py render --write      # 只动临时副本
diff <(兜底清单的 engines dump) <(渲染后清单的 engines dump)
# → 无差异（两条投递路径结论逐行一致）
```

服务端显式值优先，但那份值本来就来自同一份 schema，所以渲染与不渲染应当得到同一份结论。
这条对照是"两条投递路径同源、不会各说各话"的证据（**改前** A 与 B 不一致：A 是"没声明"、
B 才有 `requires`）。

`EngineNeedsReference` 就是界面上的：内置默认音色行置灰、主按钮禁用、
提示「这个引擎必须提供参考音频（不能只用内置音色）：在下面填一段参考 wav 再开始配音」。

两次同清单运行输出逐字节一致（`diff` 为空）；实测跑法：等 6 秒以上再 kill
（并行编译把机器压满时启动会变慢，等太短会拿到空输出）。

## 已知边界 / 前提

1. ~~现有 `server.json` 没有 `role` / `requires` / `known_issues`，要靠渲染才有提示。~~
   **已解决（批次 `cc-ai-audio-workshop-capability-fallback`）**：这些字段改由随包
   `config/model-capabilities.json` 兜底，App **逐字段**回落（服务端显式值优先）。
   `render --write` 从此是可选动作，不是前提 —— 见 `docs/model-capabilities.md`。
   本机实测：**没有改过** `server.json`（sha256 `2e317598…` 前后一致），
   `index-tts2` 已显示"要求参考音=true / `EngineNeedsReference`"。
2. **不改产品决策**：`product_excluded` / `known_issues` 的内容仍由 `config/models.schema.yaml` 说了算。
3. **模型管理里的可见性**：被排除的模型若通过下载清单进入「模型」区，仍会出现在下载列表里
   （属 `docs/model-download.md` 那批的表面），但**不能**被选为配音引擎。
4. **没有做**：按机器推荐量化档；把 `known_issues` 做成富文本/链接；
   App 直接读 `models.schema.yaml`（仍由 server.json 单一入口）。
5. 屏幕锁着 → 没有像素级目视验证；验证方式是 `AW_UI_STATE` 冒烟 + 上面这份真机 dump。

## 回归与阳性对照

每条都实测过"故意改坏 → 转红 → 复原"：

| 用例 | 阳性对照（故意改坏） | 结果 |
|---|---|---|
| `tts_engine_list_drops_product_excluded_and_streaming_only` | 去掉过滤里的 `!product_excluded` / `!is_streaming_only()` | 红：`["audio8-tts","audio8-tts-01b","audio8-tts-stream","audio8-tts-01b-stream","index-tts2"]` |
| `engine_requiring_reference_cannot_start_on_builtin_voice` | 删掉 `voice_readiness` 的 `requires_voice_ref` 分支 | 红：`left: Ready, right: EngineNeedsReference` |
| `default_engine_follows_capability_not_a_hardcoded_id` | 默认引擎改回"找 audio8-tts" | 红：`left: -1, right: 0`（清单只有 index-tts2 时） |
| `unknown_song_engine_is_an_explicit_error_not_a_silent_yue2` | `other => Err(..)` 改回 `_ => Ok(Yue2)` | 红：`未知引擎不能有实现: Yue2` |
| `dub_voice_ready_is_projected_not_recomputed_in_slint` | app.slint 退回 `voice-ready: root.voice-index >= 0;` | 红 |
| `engine_note_surfaces_requires_and_known_issues` | —— | 钉住只读说明的文案契约 |
| `song_engine_options_come_from_the_manifest` | —— | 钉住选项来源 + 兜底清单不为空（空列表会让 Slint 按 0 除） |
| `builtin_voice_row_is_gated_by_engine_capability` | 删掉内置音色行的 `!root.engine-requires-voice-ref` | 红 |
