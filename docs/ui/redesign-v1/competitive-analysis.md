# 音频作坊 · UI Redesign v1.4 竞品模式分析

> 分析范围：公开产品页与用户提供的本地参考截图。  
> 原则：只记录产品模式与设计结论，不把第三方位图、截图或品牌素材复制进仓库。  
> 参考链接：
> - ElevenLabs Studio 3.0: https://elevenlabs.io/studio
> - ElevenLabs Voiceover Studio: https://elevenlabs.io/voiceover-studio
> - ElevenLabs Dubbing Studio: https://elevenlabs.io/dubbing-studio
> - ElevenLabs Voice Design: https://elevenlabs.io/voice-design
> - ElevenLabs Voice Cloning: https://elevenlabs.io/voice-cloning
> - Murf Studio: https://murf.ai/murf-studio

## 0. 证据与效度

- **视觉证据（已逐张目视核对）**：本机参考截图 `ElevenLabs Voiceover Studio 首屏`（旁白块列表 + 右侧内容预览 + 底部多轨时间轴）、`旁白块近景`、`时间轴片段近景`、`Voice Design 单卡片`、`Voice Cloning 向导`、`Murf Studio Find Your Perfect Voice 面板`。
- **未取得交互细节的对照对象**：讯飞智作、魔音工坊。仅能抓到营销首页与导航结构（讯飞智作 IA：首页 / 讯飞配音 / 数字人视频 / AIGC工具箱 / 素材 / 资产中心），**不足以支撑交互层结论，故本表不下断言**。
- **口径**：表中"看到的模式"只写截图/页面可直接证实的现象；"采用 / 不采用"和"原因"才是我们的设计判断，两者分开陈述，避免把推断伪装成竞品事实。

## 1. 逐条对照

| 参考对象 | 看到的模式 | 采用 / 不采用 | 原因 |
|---|---|---|---|
| ElevenLabs Voiceover Studio | 左侧是“旁白块”列表：块头为彩色 dot + 说话人/轨道名（如 `Primary Narrator`），文本直接挂在块下，块内提供 `Generate Audio`；**音色不显示在块头，而是在底部时间轴的轨道上（如 `Narration Track - Original`）配合右侧参数面板选择**；右侧是内容预览。 | **部分采用：** 保留“旁白块”的对象结构（彩色 dot + 名称 + 文本 + 单一生成动作）。<br>**偏离：** 把音色提升到块头（dot + 当前音色名/来源 + 试听/换音色），而不是像 ElevenLabs 那样只放在时间轴轨道里。<br>**不采用：** 不在 v1.4 首屏加入右侧视频预览和可编辑多轨时间轴。 | ElevenLabs 的块头是**说话人/轨道**身份，音色归属在轨道上——这依赖“多轨 + 侧栏参数面板”才能被发现。我们是单说话人优先、无多轨时间轴的首屏，若照搬会把音色再次藏进轨道，正是用户反馈的“没有选择音色”。因此把“说话人 + 音色”合并到块头，用一个可见控件同时承担身份与换音色，是适配我们约束的必要偏离。 |
| ElevenLabs Voiceover Studio / Studio 3.0 | 底部 timeline 把 voice、SFX、music 等轨道统一呈现，选中 clip 后在右侧显示局部参数。 | **采用：** BGM、人声分离、音乐制作的结果统一使用轨道行；需要时再展开或进入高级。 | 减少结果页的高级参数堆叠，让“结果对象”先可见、可试听、可导出；为未来统一 timeline 留出组件接口。 |
| ElevenLabs Studio 3.0 | 一个编辑器统一 voice / music / SFX / captions，强调可组合、可导出。 | **采用：** 共享工程资产、统一任务中心、统一导出的方向。<br>**不在 v1.4 采用：** 单屏多轨编辑器和完整 captions 时间轴。 | 我们的核心目标是每屏一个主任务；统一模型应该是“工程资产 + 任务”，不是立刻复制完整 DAW 工作流。 |
| ElevenLabs Dubbing Studio | 以源内容为中心，配音、语言和导出是围绕源素材的连续链路。 | **采用：** 分离 / BGM / 配音结果都回写当前工程，导出围绕工程产物组织。<br>**不采用：** 视频转译、源视频预览和语言版本管理。 | 当前产品是音频工作台，未验证视频与多语言 Dubbing 能力，不能把竞品范围误当成已实现后端能力。 |
| ElevenLabs Voice Design | 单卡片，三个核心动作：Prompt -> Text to preview -> Generate voice。 | **采用：** 音色设计首屏严格收敛为 Prompt、试听文本、Generate Voice；模型 / 风格 / seed 放高级或能力门控。 | 这三件事构成完整、低认知负担的 voice design 闭环，也符合当前单条 voice_ref / 试听能力边界。 |
| ElevenLabs Voice Cloning | Instant Voice Clone 与 Professional Voice Clone 分开；添加样本、填写 Voice Info、Finish up 分步向导；显示样本列表和时长进度。 | **采用：** 克隆作为音色设计的二级向导，独立于 Voice Design 首屏；步骤建议为 Add Voice -> Voice Info -> Finish up。<br>**不采用：** 直接照搬专业克隆的时长和样本数量要求。 | 单条 voice_ref 已有基础，但多音色库、长样本和专业克隆后端未完成；向导可以设计，能力必须门控。 |
| Murf Studio | 独立 Find Your Perfect Voice 面板：搜索、语言、性别、年龄筛选，音色卡片、波形和试听。 | **先前不采用，现已修正：** v1.4 曾照搬到我们这边，但本机 `GET /v1/audio/voices?model=<id>` 对所有 tts 模型只返回 `["default"]`，**我们根本没有多音色可选**——照搬的结果是把“模型列表”伪装成“音色列表”（`audio8-tts / index-tts2 / audio8-tts-01b / -stream` 全是模型）。改为页内两来源音色区（内置默认 / 参考音频克隆）。<br>**保留：** 试听与“使用”分离、当前项有选中态。 | 竞品模式成立的前提是它有真实的音色市场（120+ voices）；我们只有“每引擎 1 个内置音色 + 克隆”。**照搬交互而不核对后端能力，会把模型当成音色卖**。多音色库属 P4，接入后再谈搜索/筛选。 |
| Murf Studio / ElevenLabs Voiceover Studio | 试听与选择分离：先浏览和试听，再明确使用；当前音色有选中态。 | **采用：** 选择器行第一次点击选中、第二次点击或 `试听` 播放；底部 `使用此音色` 明确应用。 | 避免“点击即使用”导致试听时误改工程，也为键盘和读屏提供可预测的操作顺序。 |
| ElevenLabs / Murf 的彩色音色标识 | 为每个旁白或音色使用稳定的彩色 dot / 色条。 | **采用：** 当前音色和轨道行使用稳定的颜色标识，但颜色不作为唯一信息来源，同时显示名称和来源。 | 颜色能快速建立对象识别，但必须配合文字和形状，满足无障碍与色弱用户。 |
| 主流 studio 的 timeline 播放 | 多轨时间轴、片段选择、局部参数、播放头。 | **部分采用：** 只保留轨道行、试听、选中态；不在配音首屏加入可编辑时间轴。 | 我们当前的关键路径是“生成 -> 校听 -> 导出”，完整 timeline 编辑会显著增加实现和交互复杂度。 |

## 2. v1.4 设计结论

### 2.1 配音 Tab

采用“旁白块”而不是普通参数行：

```text
● 当前音色  audio8-tts  [内置]
  [试听] [换音色] [新建音色 →]
┌──────────────────────────────┐
│ 欢迎来到今天的开箱视频。      │
│ 这次我把三个版本都带来了。    │
└──────────────────────────────┘
```

- 彩色 dot + 音色名 + 来源是块头（这是对 ElevenLabs 的刻意偏离：其块头只放说话人/轨道名，音色在时间轴轨道上）。
- 文本 / 句子是块体。
- `试听`、`换音色`、`新建音色`是块头操作，不增加第二个主操作。
- 没有音色时块体不可开始生成；主按钮 disabled，原因就在块头下方。
- 运行中块头锁定；完成后更换音色使块和结果 stale。

### 2.2 音色 ≠ 模型（本机事实）

`GET /v1/audio/voices?model=<id>` 对本机所有 tts 模型都只返回 `["default"]`：

- **音色**（发声身份）只有两种来源：`内置默认音色` / `参考音频克隆`。
- **模型**（`audio8-tts` / `index-tts2` / `0.1b` / `stream`…）是**引擎参数**，放本页「高级」的 `引擎` 下拉。
- 因此 v1.4 的「换音色」是**页内展开的两来源音色区**（与「高级」同款盒子），
  不是 Murf 式搜索/筛选音色市场——**我们没有可搜的音色市场**，照着做只能把模型当音色列。
- 多音色库（音色库存档 / 导入导出）属 P4，接入后再引入搜索与筛选。

### 2.3 结果轨道行

BGM、人声分离、音乐制作的结果采用统一轨道行：

```text
[●] 音轨名 / 产物名             [试听] [导出]
[●] 伴奏 / 混音 / 版本名        [试听] [导出]
```

- 高级参数不铺在结果区。
- 需要在轨道行上“展开”或进入高级抽屉。
- 未来可以扩展为统一多轨 timeline，但 v1.4 不做 DAW 级编辑。

### 2.4 音色设计

Voice Design 首屏固定三件事：

1. `Prompt`
2. `Text to preview`
3. `Generate voice`

克隆向导独立为二级流程：

```text
Add Voice → Voice Info → Finish up
```

只有后端真实支持样本持久化、音色库和专业克隆时才开放对应步骤；单条 `voice_ref` 能力不能被包装成完整 Professional Voice Clone。

## 3. 不采用清单

- 不把 ElevenLabs / Murf 截图或品牌资产放进仓库。
- 不在配音首屏复制完整视频预览、内容编辑器和多轨编辑器。
- 不把 Murf 的完整 Find Your Perfect Voice 市场常驻在首屏。
- 不为匹配竞品而伪造语言、性别、年龄、模型、专业克隆或音色库能力。
- 不把 BGM / 分离 / 音乐的高级参数重新铺回结果区。
- 不将“Studio”做成无边界 DAW；音频作坊仍保持每屏一个主任务。
