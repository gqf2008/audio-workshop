# 音频作坊 · UI Redesign v1.2 实现说明

> 本文只给出从设计到 Rust / Slint 的映射、callback/属性增删建议、迁移步骤与验收标准，不修改实现、不提交代码。
> v1.2 的逐 Tab 交互契约见 `interaction-spec.md`；可点击状态演示见 `interactive-prototype.html`。
> 实施基线：当前 `feat/ui-simplify` 实际是“配音 + BGM”两 Tab；`feat/m4-song` 另有 `SongWorkbench`，但主界面模型与命名不符合本轮设计。v1.2 目标导航为“配音 / BGM / 人声分离 / 音乐制作 / 音色设计”五 Tab。实施时应先把两条线对齐，再做壳层重构。

## 1. 现状事实

| 项目 | 当前事实 | 对本设计的影响 |
|---|---|---|
| 场景导航 | `ui/app.slint` 当前 `scenes = ["配音", "BGM"]`；顶部有 segmented tab | 保留顶部导航，扩展为五个场景 |
| 抽屉 | `WorkbenchDrawer` 同时承载工程、主题、音色、任务、导出 | 拆为“全局设置 + 当前 Tab 高级抽屉 + 任务中心 + 统一导出” |
| 配音 | `DubWorkbench` 已做到句子行简化、选中展开、首屏主按钮 | 作为低密度基线保留，补状态化和高级分组 |
| BGM | `BgmWorkbench` 只有 prompt + 3 个并列按钮 + progress + status | 改成一个主操作和状态化结果区 |
| 音乐制作 | `feat/m4-song/ui/song_workbench.slint` 存在但叫“歌曲 · 彩蛋”，模型选择在首屏 | 合并到主线后改名“音乐制作”，模型/步数/时长移入高级 |
| 人声分离 | 当前 `ui/`、`src/main.rs`、`config/models.schema.yaml`、`aw-core` 均未发现分离任务/模型接口 | 必须在实施前补后端能力；UI 先做能力门控，不造模型名 |
| 音色设计 | 当前仅有单条 `voice_ref` 克隆基础；配音高级中的参考音路径是入口；M4 P4 多音色/音色库/预设未完整实现 | 首版复用试听与单条 voice_ref；保存为音色、音色库和预设导入导出必须能力门控 |
| 任务恢复 | 配音有 `project.json` 断点；BGM 产物主要存在运行时状态；无统一任务记录 | 增加统一任务记录或统一 ProjectView；不依赖内存恢复 |
| 导出 | 配音 `export-requested`、BGM `bgm-export-tracks` 分开 | 保留适配器，逐步收口到统一导出请求 |

## 2. 组件映射

### 2.1 建议新增的 Slint 组件

| 组件 | 位置建议 | 职责 |
|---|---|---|
| `TopTabBar` | `ui/shell.slint` | 五个一级 Tab；选中态、运行/失败 badge、键盘切换 |
| `ProjectStrip` | `ui/shell.slint` | 工程名、保存状态、恢复状态、切工程入口 |
| `TaskStatusLine` | `ui/shell.slint` | 当前 Tab 的一条状态；不承担全局状态栏 |
| `AdvancedButton` | `ui/shell.slint` | 打开当前 Tab 的高级抽屉；标题随 Tab 变化 |
| `TaskCenterButton` | `ui/shell.slint` | 跨 Tab 任务摘要与入口 |
| `TaskCenterDrawer` | `ui/task_center.slint` | 运行中、等待中、失败、完成任务列表；重试/取消/回来源 |
| `ExportDrawer` | `ui/export_drawer.slint` | 按当前结果类型展示格式、目录、分轨选择 |
| `AudioDropZone` | `ui/separation_workbench.slint` | 点击/拖入音频、文件摘要、能力未就绪态 |
| `StemResultRow` | `ui/separation_workbench.slint` | 人声/伴奏两轨试听与下载 |
| `MusicWorkbench` | `ui/music_workbench.slint` | 由 `SongWorkbench` 迁移；歌词/风格/主操作/结果 |
| `VoiceDesignWorkbench` | `ui/voice_design_workbench.slint` | 试听文本、基础音色/参考音频、试听/创建音色、结果条 |
| `VoiceAdvancedDrawer` | `ui/voice_design_workbench.slint` | 基础模型、参考音频/文本、风格/情绪/seed 三组高级参数 |
| `VoicePresetMenu` | `ui/voice_design_workbench.slint` | 导出/导入音色预设的受限入口；P4 未完成时显示能力未就绪 |
| `ResultBar` | `ui/shell.slint` | 结果摘要、试听、导出次操作；结果就绪才出现 |

### 2.2 现有组件迁移

| 现有 | 处理 |
|---|---|
| `MainWindow` | 保留为壳层；接入 `TopTabBar`、`ProjectStrip`、`TaskCenterButton`；移除全局状态栏 |
| `DubWorkbench` | 保留；主画布做“稿件/句子”状态切换；抽出 `DubAdvancedDrawer` |
| `SentenceRow` | 保留默认三元素；展开区改为试听、重录、文本修正、错误详情 |
| `WorkbenchDrawer` | 拆解，不再作为万能抽屉；代码可保留为兼容层，逐步删除跨场景区块 |
| `BgmWorkbench` | 保留 prompt 主画布；删除常驻标题/长说明/三按钮；抽取 `BgmAdvancedDrawer`、`BgmResultBar` |
| `SongWorkbench` | 重命名为 `MusicWorkbench`；模型 segmented control 移入高级；标题改为“音乐制作” |
| `PixelDrawer` | 保留；同一时间只显示一个上下文抽屉 |
| `PixelToast` | 保留用于瞬时反馈；失败和恢复状态不能只靠 toast |
| `PixelSegmentedControl` | 顶部五 Tab 可作为初始实现；若选中态或等分宽度在窄屏不稳定，另做 `TopTabBar` 包装 |

`build.rs` 当前只监听 `ui/app.slint`、`ui/dub_workbench.slint`、`ui/model.slint`；新增 `shell.slint`、`task_center.slint`、`export_drawer.slint`、`separation_workbench.slint`、`music_workbench.slint`、`voice_design_workbench.slint` 后必须同步增加 rerun 监听。此项属于实施步骤，不在本轮设计改动中执行。

## 3. 全局壳层：属性与 callback

### 3.1 建议保留

- `scene: int`：仍作为当前 Tab 索引。
- `scene-changed(int)`：切 Tab 只更新当前视图，不取消任务。
- `project-name`、`project-edited()`：工程上下文继续复用。
- `theme-scheme`、`theme-picked(string)`：主题是全局设置，不再在业务抽屉里与音色混排。
- 窗口控制相关 callback：`drag-start`、`minimize`、`toggle-maximize`、`close-window`、`begin-resize`。

### 3.2 建议修改

```text
scenes: ["配音", "BGM"]                    → ["配音", "BGM", "人声分离", "音乐制作", "音色设计"]
scene 索引                                  → 0..4，并在 UI state 持久化
drawer-open: bool                           → advanced-open + drawer-scope
status-text: string                         → project-status-text + task-status-text
busy/running: bool                          → active-task-id + tasks + 派生的 tab-busy
```

### 3.3 建议新增属性

| 属性 | 类型方向 | 作用 |
|---|---|---|
| `project-revision` | string/int | 防止 UI 与 worker 使用不同工程版本 |
| `project-saved-at` / `project-dirty` | string/bool | 项目条保存状态 |
| `tasks` | `[TaskSummary]` | 任务中心数据 |
| `active-task-id` | string/int | 当前 Tab 的任务状态 |
| `drawer-scope` | string | `dub` / `bgm` / `separation` / `music` / `voice_design` / `export` |
| `capability-separation` | string/bool | 分离后端未就绪时做能力门控 |
| `capability-music` | string/bool | 音乐制作实验能力门控 |
| `capability-voice-library` | string/bool | 多音色保存、音色库和预设能力门控 |
| `music-result`、`separation-result`、`voice-preview-result` | struct/compatible model | 结果条和导出读取 |

### 3.4 建议新增 callback

```text
task-center-toggled()
task-retry(string task_id)
task-cancel(string task_id)
task-resume(string task_id)
task-open-source(string task_id)
export-open(string scene)
settings-open()
project-open()
```

### 3.5 建议删除或降级

| 当前属性/回调 | 处理 |
|---|---|
| `char-count`、`est-duration`、`total-duration`、`total-label` | 只做内部计算，不进入首屏 |
| `bgm-progress`、`bgm-status-text` | 由任务记录派生；不要在主窗口散落第二套状态 |
| `bgm-preview` | 改成结果轨选择后的试听，或保留为兼容适配器 |
| `bgm-export-tracks` | 迁移到统一导出请求；迁移期保留适配器 |
| `export-wav-on`、`export-srt-on`、`export-dir` | 收到 `ExportDrawer`，不挂在主窗口常驻 |
| `voice-ref-path`、`speed`、`speed-label`、`auto-normalize` | 收到 `DubAdvancedDrawer` |
| `seek` | 配音校听播放器内部能力；首屏没有时间轴时刻度 |

## 4. 各 Tab 的组件与事件映射

### 4.1 配音 Tab

保留：

```text
DubWorkbench
SentenceRow
script-edited / use-sample / clear-script
select-sentence / preview-one / redo-one
start-run / stop-run / stop-preview
```

建议新增：

```text
view-changed("script" | "sentences")
retry-failed()
advanced-open("dub")
```

建议移动：

- `resplit()`：从首屏/万能抽屉移到项目设置或配音高级的“文本处理”组。
- `voice-index`、`voice-ref-path`、`speed`、`auto-normalize`：移入 `DubAdvancedDrawer`。
- 导出选择与 `export-requested()`：通过统一 `ExportDrawer` 触发。
- `total-duration`、`total-label`：只作为结果条的可读时长，不在句子行常驻。

状态映射建议：

```text
空稿       → primary = "导入 / 粘贴稿件"
有稿未合成 → primary = "开始配音"
运行中     → primary = "停止"
完成       → primary = "再生成一版"，ResultBar 显示导出
失败       → TaskStatusLine 显示 retry-failed
恢复       → primary = "继续配音"
```

### 4.2 BGM Tab

保留：

```text
BgmWorkbench
generate / stop
prompt
has-result
```

建议新增：

```text
result-index / select-result(int)
preview-bgm()
preview-mix()
advanced-open("bgm")
next-version()
```

建议移动：

- 时长、分段、模型、循环选项 → `BgmAdvancedDrawer`。
- duck gain、混音来源、三轨保留 → `BgmAdvancedDrawer`。
- `export-tracks` → 统一导出请求。

必须删除：

- 首屏标题“BGM · 当前配音工程”和长描述。
- 首屏同时出现 `生成并混音`、`试听混音`、`导出三轨`。
- 单独 progress bar 与状态文本堆叠；运行中只保留状态行中的轻量进度。
- 无结果时可见的试听和导出按钮。

BGM 的需要额外区分两个状态：

```text
bgm-only-ready      → 仅 BGM 可试听/导出
mix-ready           → BGM + voice + mixed 可试听/导出
```

### 4.3 人声分离 Tab

新增 `SeparationWorkbench`，建议属性：

```text
input-path: string
input-name: string
input-duration-label: string
stage: string
progress: float
has-result: bool
vocals-path: string
accompaniment-path: string
capability: "ready" | "unsupported" | "error"
status-text: string
```

建议 callback：

```text
pick-input()
input-dropped(string path)
start-separation()
cancel-separation()
preview-stem("vocals" | "accompaniment")
export-stem("vocals" | "accompaniment")
```

建议新增 Rust 任务消息：

```text
Cmd::Separate { revision, input_path, options }
Msg::StemProgress { task_id, stage, fraction }
Msg::StemDone { task_id, vocals, accompaniment, duration }
Msg::StemFailed { task_id, code, message }
```

以上为接口建议，不是已存在 API。没有真实模型注册、服务端端点或本地推理实现前，UI 的 `capability` 必须为 `unsupported`，主操作不可发起任务。

### 4.4 音乐制作 Tab

从 `SongWorkbench` 迁移：

```text
SongWorkbench       → MusicWorkbench
"歌曲 · 彩蛋"       → "音乐制作" + 轻量“实验性”标签
model-index         → MusicAdvancedDrawer.model-index
song-generate       → music-generate
song-preview        → music-preview
song-export-track   → 统一导出适配器，或保留为兼容 callback
```

建议属性：

```text
lyrics: string
style: string
busy: bool
has-result: bool
status-text: string
result-duration-label: string
```

建议高级属性：

```text
model-index
steps
duration-seconds
language
seed（只在确有必要时显示，默认不在首屏）
```

首屏只保留歌词、风格、生成主操作、状态、高级入口、结果条；模型选择、质量长文、导出格式全部移出。

### 4.5 音色设计 Tab

新增 `VoiceDesignWorkbench`，与配音高级音色选择共享同一份音色资产。

建议属性：

```text
audition-text: string                 // 默认预填，可编辑
base-voice-index: int                 // 基础音色；-1 表示未选
reference-path: string                // 参考音频路径
reference-name: string                // 文件名/资产摘要
reference-text: string                // 仅在后端真实支持时启用
voice-name: string                    // 保存音色时使用
stage: string
progress: float
has-preview: bool
is-saved: bool
status-text: string
capability-voice-library: "ready" | "unsupported" | "error"
```

建议 callback：

```text
audition-text-edited()
base-voice-changed(int)
pick-reference()
reference-dropped(string path)
start-preview()
stop-preview()
create-voice()
preview-result()
save-voice()
use-in-dub()
export-preset()
import-preset()
advanced-open("voice_design")
```

建议新增任务消息/命令：

```text
Cmd::VoicePreview { revision, text, model, voice_ref, reference_text? }
Cmd::SaveVoiceAsset { revision, preview_id, name, metadata }
Msg::VoicePreviewProgress { task_id, stage, fraction }
Msg::VoicePreviewDone { task_id, audio_path, duration }
Msg::VoicePreviewFailed { task_id, code, message }
Msg::VoiceAssetSaved { task_id, voice_id, name }
```

这些消息只有在前端现有 TTS 调用和后端字段契约确认后才能实现。当前已有基础：

- 单条 `voice_ref` 请求可作为试听生成基础；
- 配音工程已记录 `model`、`voice_ref`、`voice_ref_hash`；
- 配音高级的参考音频入口已经存在，但不等同于多音色库。

当前未完成：

- 多音色库、音色列表管理、跨工程复用；
- 音色预设的稳定序列化格式、导入导出；
- 风格/情绪/参考文本是否被当前 TTS 后端接受；
- 音色资产的版本、许可、引用计数和迁移策略。

首版状态映射：

```text
未选音色/参考音 → primary = "生成试听"（禁用）
输入就绪         → primary = "生成试听"
试听运行         → primary = "停止"
试听完成         → primary = "创建音色"
已保存音色       → primary = "再生成一版"，结果条出现“用于配音”
P4 能力未就绪    → “创建音色/预设导入导出”置灰并解释
```

高级抽屉分三组：

1. **基础音色 / 模型**：TTS 模型、许可说明、默认音色。
2. **参考音频与参考文本**：参考音频、参考文本；只有后端真实接受参考文本时才显示可编辑状态。
3. **风格 / 情绪 / seed**：风格、情绪、seed、采样参数；能力未就绪时整组显示未就绪，不造滑杆。

与配音共享：`VoiceAsset` 只保存一份；配音高级只读取和选择，不复制或重写音色设计中的参考信息。保存成功后从结果条“用于配音”直达配音 Tab，并预选该音色。

## 5. 统一任务模型

现有 worker 已是串行命令处理，可作为任务队列的基础，但 UI 需要额外一层任务记录。

建议 `TaskSummary` 至少包含：

```text
id
scene             配音 / bgm / separation / music / voice_design
kind              synthesize / bgm_generate / mix / separate / music_generate / voice_preview / voice_save / export
status            queued / running / reviewable / done / failed / cancelled
stage             给用户看的阶段名称
progress          0.0..1.0
input_summary     稿件摘要 / prompt 摘要 / 文件名 / 歌名
source_revision   关联工程版本
artifact_ids      结果资产
error_code        可执行错误类型
error_message     面向用户的错误说明
retry_count
resume_token      可恢复任务的服务端/本地断点
created_at / updated_at
```

持久化建议：

- 优先把任务摘要挂到工程 manifest；若担心与现有 `project.json` 兼容，可新增同目录 `tasks.json`。
- 不把 `RefCell<Option<BgmArtifacts>>` 当恢复来源。
- 重启时读取任务记录；`running` 的任务根据 `resume_token` 回退为 `queued` / `reviewable`，不要伪装成仍在运行。
- 切换 Tab 只改变视图，不改变 worker 的当前任务。

## 6. 后端能力缺口与接口要求

### 6.1 人声分离：需要新增，不要假设已存在

当前可读代码中没有：

- `config/models.schema.yaml` 的分离任务模型；
- `aw-core` 的分离模块；
- `audiocpp_server` 的分离端点契约；
- `src/main.rs` 的分离命令/消息；
- Slint 分离工作台。

实施前必须先做产品/技术决策：

1. 使用哪个本地分离模型、是否可分发、权重的下载与许可如何处理。
2. 分离接口是复用 `/v1/tasks/run` 还是新增专用端点。
3. 返回格式是否固定为 vocals / accompaniment 两个 wav，以及采样率、声道和位深。
4. 错误是否能区分解码失败、模型缺失、显存/内存不足、磁盘不足、取消。

建议接口契约（示意，不是现有 API）：

```text
request:
  input_path, model_id, options { threshold? denoise? output_format? }

progress:
  { task_id, stage, fraction }

result:
  { task_id, vocals_path, accompaniment_path, duration_seconds,
    sample_rate, channels, format }

errors:
  decode_failed | model_missing | out_of_memory | disk_full | cancelled | internal
```

UI 在模型和端点未就绪时显示能力未就绪；不得在高级抽屉里写一个假模型名，也不得把“阈值”滑杆接到不存在的参数。

### 6.2 音乐制作：先迁移 M4 分支

- `feat/m4-song` 已有歌曲 UI 和服务接入方向，但当前主线缺少。
- 实施时先合并/摘取音乐链路，再改名为 `MusicWorkbench`。
- 保留现有 Rust 音乐调用，不要为了 UI 重写后端。
- 模型选择必须从主画布移到高级抽屉；如果模型未下载，高级组显示真实下载/配置状态。

### 6.3 音色设计：复用单条 voice_ref，不假设音色库已完成

当前已存在的真实基础：

- 配音请求支持 `voice_ref`，`Project` 会保存 `voice_ref` 与 `voice_ref_hash`；
- 配音高级已经有参考音频路径输入；
- 当前 `aw-core::Client` 的请求只写入 `voice_ref`，没有在代码中确认参考文本、风格或情绪字段的传输；
- 当前没有可复用的多音色库、音色预设格式、导入导出和跨工程引用实现。

因此首版音色设计只能承诺：

- 用当前真实 TTS 模型生成一条试听；
- 选择参考音频作为 `voice_ref`；
- 在当前工程内复用该试听/参考配置；
- 将“保存为音色”“音色库”“预设导入导出”显示为 P4 能力门控状态，直到后端和存储契约落地。

不得臆造：

- 未在服务端注册的风格/情绪模型；
- 未确认的参考文本字段；
- 未定义的预设文件格式；
- “已保存到音色库”而实际只有内存状态。

音色资产至少要等产品确定以下契约后再实现持久化：

```text
voice_asset:
  id, name, created_at, updated_at
  base_model
  reference_audio_path / content_hash
  reference_text?          # 只有服务端支持时
  style / emotion?         # 只有服务端支持时
  default_seed?
  license / usage_scope
  preview_audio_path
```

### 6.4 导出：保留现有能力，先代理后统一

- 配音：`export-requested` 继续作为适配入口。
- BGM：`bgm-export-tracks` 继续作为适配入口。
- 分离/音乐：新增导出适配。
- UI 统一调用 `ExportDrawer`，内部按场景路由；迁移期不要一次性删除旧 callback，先并存，再收口。

## 7. 迁移步骤

### Step 1：统一分支与基线

1. 把 `feat/m4-song` 的音乐能力合并/摘取到 UI 重构分支。
2. 保留当前“顶部 Tab”修复，但删除“素材库”和“场景在抽屉”的历史代码。
3. 明确当前主线 `ui/app.slint` 的顶部 Tab 是唯一导航基线。

### Step 2：重做全局壳层

1. 将 `scenes` 改为五个名称。
2. 抽出 `TopTabBar`、`ProjectStrip`、`TaskCenterButton`、`TaskStatusLine`。
3. 删除全局状态栏，把状态迁到当前 Tab 状态行、项目条和任务中心。
4. 让所有 Tab 的切换先只切视图，不取消任务。

### Step 3：拆抽屉

1. 把 `WorkbenchDrawer` 的工程/主题/任务/音色/导出拆开。
2. 保留 `PixelDrawer`，新增 `drawer-scope`。
3. 配音高级、BGM 高级、分离高级、音乐高级、音色设计高级各自只渲染自己的组。
4. 全局设置只保留主题、存储、服务、关于。

### Step 4：重构配音和 BGM 主屏

1. 配音主画布做“稿件 / 句子”状态切换，清空和切句变为条件性/高级动作。
2. 配音只保留一个状态行和一个主操作位。
3. BGM 删除长标题、长说明、三按钮，改成 prompt 主画布 + 结果条。
4. 两者都接入任务状态行和恢复状态。

### Step 5：迁移音乐制作

1. `SongWorkbench` 改名、去掉“彩蛋”长文。
2. 模型选择移入高级。
3. 歌词、风格、生成、结果条保持首屏。
4. 接入统一任务队列和导出。

### Step 6：接入音色设计

1. 先复用现有 `voice_ref` 试听链路，不新增未知模型字段。
2. 实现 `VoiceDesignWorkbench`：预填试听文本、基础音色/参考音频、生成试听主操作。
3. 高级抽屉只放当前后端真实支持的模型、参考音频与参考文本、seed 等参数。
4. `创建音色`、音色库和预设导入导出先做能力门控；P4 存储/序列化契约确认后再开启。
5. 保存成功后让配音高级读取同一 `VoiceAsset`，并提供“用于配音”跳转。

### Step 7：接入人声分离

1. 先设计/实现后端模型和任务接口。
2. 再实现 `SeparationWorkbench`、拖入区、两轨结果。
3. 首版只放已存在的模型和阈值；没有就显示能力未就绪。
4. 接入统一任务记录、重试和导出。

### Step 8：统一任务与恢复

1. 添加任务摘要/data model。
2. 在 `UiState` 和 worker 消息间建立 task id 映射。
3. 将 `busy/running` 改为按任务来源判断；不要一个全局 `busy` 阻止所有 Tab。
4. 重启后恢复工程、任务可见状态、上次 Tab/选中对象。
5. 导出成功/失败保留可重试记录。

## 8. 验收标准

### 8.1 导航

- [ ] 960×640 和 1200×880 下，顶部五个 Tab 都可见且不换行；必要时仅允许标签缩写。
- [ ] 五个 Tab 均可在一次点击内到达；任何 Tab 都不藏在抽屉或二级页。
- [ ] 切 Tab 不取消正在运行的配音、BGM、分离、音乐或音色试听任务。
- [ ] 当前 Tab、运行 badge、失败 badge 在切换和重启后正确恢复。

### 8.2 密度

- [ ] 每个 Tab 默认首屏语义元素 ≤ 8，完成态也 ≤ 8。
- [ ] 每个稳定状态只有 1 个主操作。
- [ ] 首屏可见参数输入数量：配音 0、BGM 1、分离 1、音乐 2、音色设计 2。
- [ ] 首屏没有 seed、毫秒、时间轴刻度、模型名、步数、阈值、duck dB。
- [ ] 任意一行并列操作 ≤ 3 个。
- [ ] 高级参数默认 0 个，打开抽屉后才出现。
- [ ] 空 / 运行 / 完成 / 失败 / 恢复五态均可通过状态 fixture 截图复核。

### 8.3 任务与恢复

- [ ] 任务中心能看到五个来源的 queued / running / failed / done 状态。
- [ ] 失败任务可重试并回到来源 Tab。
- [ ] 应用重启后，本地已保存的工程可继续；任务不会显示为运行中却没有 worker。
- [ ] BGM 分段失败只重试未完成段，不覆盖已完成产物。
- [ ] 配音断点续作继续跳过已完成句，失败句可单独重试。
- [ ] 导出只在结果有效时启用；导出失败不修改源结果。

### 8.4 能力真实性

- [ ] 人声分离没有真实模型/端点时显示能力未就绪，不出现假模型、假阈值或可发起的伪任务。
- [ ] 音乐制作模型未下载时给出真实下载/配置路径。
- [ ] 音色设计只把单条 `voice_ref` 试听和当前工程复用写成可用能力；音色库、保存为音色、预设导入导出在 P4 未完成时显示能力未就绪。
- [ ] 音色设计中的参考文本、风格、情绪、seed 只有在后端真实支持时才可编辑；否则显示未就绪。
- [ ] 模型许可、参考音用途和实验性提示只在需要时显示，不占主画布。

### 8.5 实现验证

代码实施后至少执行：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

另外增加 UI 状态截图基线和五个 Tab 的尺寸快照；每个状态至少覆盖 960×640 窄窗。若新增 capability gate 或任务恢复路径，按仓库规则补阳性/阴性测试，确保未就绪态和失败态真实可达。

- [ ] 逐条执行 `interaction-spec.md` §8 的 `INT-*` 验收项；每项都必须在 Slint 中可操作或可断言状态，而不是只检查静态截图。

## 9. 交互到 Slint 的实现映射

本节把 `interaction-spec.md` 中的交互行为映射到 Slint / Rust 状态，不引入新的后端假设。

| 交互 | Slint / Rust 状态 | 实现要求 |
|---|---|---|
| 顶部 Tab 点击 | `scene`、`scene-changed(int)`、`tab-busy` 派生状态 | 切 Tab 只更新视图；不得调用 stop/cancel |
| 主按钮点击 / 二次点击 | `active-task-id`、`active-task-scene`、任务状态 | 第一次启动 / 继续，第二次停止；文案由状态派生 |
| 高级抽屉 | `drawer-open`、`drawer-scope` | 启动主任务时自动关闭；`Esc` / 遮罩关闭后焦点回到触发按钮 |
| 任务中心 | `[TaskSummary] tasks`、`active-task-id` | 显示来源 Tab、状态、进度、结果、取消 / 重试 / 打开来源 |
| 取消任务 | `task-cancel-requested(task_id)` | 任务二次点击确认；取消后状态为 recoverable，不删除已完成结果 |
| 失败重试 | `task-retry-requested(task_id)`、单句 / 分段 retry callback | 配音支持单句，BGM 支持失败段，分离使用整任务，音乐支持单版本 |
| stale | `input-revision`、`result-revision`、`result-stale: bool` | 输入变化后比较 revision；不自动删除旧结果；导出按钮按 stale 禁用或进入旧版本确认 |
| 焦点 | `FocusScope` / Rust 侧 focus 请求 | `用于配音` 后切到配音、打开高级抽屉、聚焦音色选择器；关闭抽屉后回到触发点。具体聚焦调用以 Slint 1.17 API 验证为准 |
| 确认 / 撤销 | `confirm-requested(kind)`、`undo-token` | 清空、覆盖音色、删除版本、替换源文件需确认；软删除支持 5 秒撤销 |
| 文件校验 | `file-pick-requested()`、`file-dropped(path)`、`file-validation-state` | 分离音频和音色参考音频共用校验状态；后端限制由服务返回，不在 UI 写死 |
| “用于配音” | `voice-asset-id`、`preselected-voice-id` | 保存成功后写入工程资产；点击后只预选，不自动触发合成 |
| 能力门控 | `capability-separation`、`capability-voice-library` | unsupported 时主按钮 disabled、状态行解释、任务中心不新增伪任务 |

交互状态与视觉状态的绑定必须单源：

```text
task.status       → 主按钮文案 / disabled / tab badge / 任务中心
input-revision    ≠ result-revision → stale
task.error_code   → 状态行文案 / 重试粒度 / 错误类型
selected-object   → 行选中态 / 二次点击试听目标
drawer-scope      → 抽屉标题 / 内容 / 允许的 callback
```

未来 Slint 验收除了静态截图，还要增加状态回调测试：点击 / 二次点击 / 切 Tab / 取消 / 重试 / stale / 恢复 / 用于配音 / 能力门控都必须能独立触发并断言状态变化。

## 10. 实施风险

| 风险 | 说明 | 处理 |
|---|---|---|
| 现有 `busy` 是全局单例 | 一个任务运行会阻塞其他 Tab 的主操作 | 改成任务来源 + 任务中心判断 |
| BGM 产物只在内存 | 重启无法恢复结果与导出 | 任务记录持久化，结果写入工程资产 |
| 分离后端不存在 | UI 一旦开放就是空壳 | 先做能力门控和后端契约 |
| 音色设计能力边界混淆 | 单条 voice_ref、试听、音色库、预设互操作不是同一能力 | 在 UI 上区分试听/当前工程复用与 P4 音色库，分别门控 |
| 音乐分支与主线 UI 冲突 | `SongWorkbench`、scenes、app 组合方式不同 | 先合并/摘取，再重构壳层 |
| 万能抽屉改动大 | 现有 callback/property 绑定较多 | 保留旧 callback 适配器，逐 Tab 迁移后再删 |
| 计数口径再次漂移 | 只看 grep 数量会掩盖并列操作和状态堆叠 | 以本文件的语义元素、主操作、参数输入数一起验收 |
