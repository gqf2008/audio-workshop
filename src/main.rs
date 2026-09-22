// Windows 上以 GUI 子系统运行：不加这一条，双击/开始菜单启动的 exe 会**多弹一个控制台
// 黑窗**（2026-09-19 真机反馈），窗口里还会滚出 UTF-8 的诊断行 —— 中文 Windows 的控制台
// 代码页是 GBK，看着就是乱码。
//
// 只在 release 生效（装给用户的那个）：debug 保留控制台，否则 `cargo test` 在 Windows 上
// 的输出会被"无控制台"吞掉（测试二进制同样吃到这个属性）。release 下 stderr 写不出去不会
// 崩：std 把 Windows 的 ERROR_INVALID_HANDLE 当"丢弃"处理（`io::stdio::handle_ebadf`），
// 只是诊断看不到 —— 用户可见的状态都在界面状态栏与 /health 里。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 音频作坊 · 配音工作台（M1 桌面壳——真实链路版）
//!
//! 链路：UI（本文件）↔ 工作线程（合成/拼装）↔ audiocpp_server
//! （aw-core 的 `Client`，POST /v1/tasks/run）。核心逻辑全在 aw-core
//! （切句/文本兜底/逐句合成/拼装），与 Python tools/ 行为由 parity 夹具固定；
//! 本文件只做：界面状态、线程间消息、校听播放。
//!
//! 落盘布局（`~/Documents/音频作坊/`）：
//!   projects/<工程名>/project.json      工程（逐句落盘 → 断点续作）
//!   projects/<工程名>/sentences/NNN.wav  逐句音频
//!   projects/<工程名>/out/final.wav|srt 成品
//!   voice-trimmed/<slug>-<hash8>-15s.wav 超长参考音频的裁剪副本（原文件不动，克隆用它）
//!   <工程名>.wav / <工程名>.srt          导出（复制自 out/）

mod backup;
mod batch;
mod cancel;
mod dictionaries;
mod download;
mod download_mirror;
mod engine_supervisor;

/// 随包引擎的进程级句柄槽。放全局是因为壳的退出路径只有 `main` 末尾一处 ——
/// 句柄走局部变量会在 `ui.run()` 阻塞期间被借用打结，走全局反而只有一条回收路径。
static ENGINE_SUPERVISOR: std::sync::Mutex<Option<engine_supervisor::EngineSupervisor>> =
    std::sync::Mutex::new(None);
mod export;
/// 随包分发的模型下载清单（M4-P7 第二段：下载源）。映射规则与诚实边界都在那里。
mod model_capabilities;
mod model_sources;
/// 路径比较的唯一入口（`..` / 软链都按真实路径消解）。
mod paths;
mod picker;
mod player;
mod sep_history;
mod tasks;
mod templates;
/// 检查更新（M4-P8 的另一半）：版本比对 + 发布页链接。逻辑都在那里，这里只接线。
mod update;
mod versions;
mod voices;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, Timer, TimerMode, VecModel};

use aw_core::{
    assemble_bgm, bgm_only_artifacts, generate_cover, generate_segments_stoppable, generate_song,
    mix_project, BgmArtifacts, BgmOptions, BgmRun, Client, ClientError, Project, SongModel,
    SongOptions, VoiceSource, DEFAULT_PUNCTUATION,
};
use sha2::{Digest, Sha256};

slint::include_modules!();

slint_pixel::impl_title_bar_ui!(MainWindow);
slint_pixel::impl_resize_ui!(MainWindow);

/// 主循环节拍：40ms（消息泵 + 播放头推进）。
const TICK_MS: u64 = 40;
/// 音色试听用的固定短句（只播不落工程；改文案不用动链路）。
const VOICE_PREVIEW_TEXT: &str = "你好，这是当前音色的试听。";

/// 与 Python `tools/audio_dub.py` 同款链路参数。
const GAP_MS: u64 = 250;
const MAX_CHARS: usize = 80;
const BASE_SEED: u64 = 831001;
/// 工程目录与导出目录的根目录名（`~/Documents/音频作坊/`）。
const WORKSHOP_DIR: &str = "音频作坊";
/// 独立生成 BGM（没有配音成品）时的默认时长。
const DEFAULT_BGM_STANDALONE_SECONDS: f64 = 60.0;
const DEFAULT_PROJECT: &str = "示例工程 · 频道口播";
/// 中文口播时长估算：秒/字（未合成句的展示用估值；合成后由真实时长覆盖）。
const SECS_PER_CHAR: f32 = 0.18;

// ── 工作线程消息 ──

/// 批量里的一条（`Cmd::RunBatch` 用）。
///
/// 稿子在这里是**已读进内存的正文**：导入时的各种失败（读不到 / 非 UTF-8 / 空稿 /
/// 重名）在 UI 侧已经变成"跳过 + 原因"，worker 不该再去碰文件系统读稿——
/// 那会把导入期的错误推迟到执行期，用户看到的是"跑到一半才说文件有问题"。
struct BatchCmdItem {
    task_id: u32,
    /// 工程名（已过 `file_stem` 归一）
    name: String,
    script: String,
}

enum Cmd {
    /// 开始/继续合成。script/model/voice_ref/project_name 取自界面当前值。
    ///
    /// `task_id` 是任务台账里的 id：worker 真正开始执行时用它把条目从"排队中"提升
    /// 为"运行中"，并在执行前检查它是否已被取消（排队中点停止的情况）。
    Run {
        revision: u64,
        task_id: u32,
        script: String,
        model: String,
        voice_ref: Option<String>,
        /// 参考音频里实际念的内容。是否必填按引擎（audio8-tts 要、index-tts2 不要），
        /// 成对时与 `voice_ref` 一起下发（见 `aw_core::VoiceClone`）。
        voice_ref_text: Option<String>,
        project_name: String,
        /// 句间静音（毫秒）：只影响拼装出来的时间轴，不影响逐句音频
        gap_ms: u64,
        /// 文本兜底（数字/年份规范化）开关：影响 spoken 文本 → 变了要重录
        auto_normalize: bool,
        /// 当前启用的发音词典（空 map = 没启用）：与兜底开关同类，改了要重录
        dict: std::collections::BTreeMap<String, String>,
        /// true = 这次只重跑工程里 `error:` 的句子（用户释放内存后点「继续合成」）。
        /// false = 正常续作：done 跳过，pending/error 继续。
        retry_failed: bool,
    },
    /// 单句重录（换 seed 重合成该句）
    Redo { revision: u64, index: usize },
    /// 用户点「释放模型内存」：调用服务端 /v1/tasks/unload_all_models。
    UnloadModels,
    /// 批量配音（M4-P1）：按顺序把 N 篇稿子跑成 N 个工程。
    ///
    /// 与单篇共用同一条链路（load_resumable → synthesize → assemble）与同一份
    /// `projects_root`；每条自带 task_id，所以任务中心里是 N 条独立的配音任务、
    /// 排队中的那几条也能单独取消（取消登记表按 task_id 定位）。
    RunBatch {
        revision: u64,
        model: String,
        voice_ref: Option<String>,
        /// 与单篇同一份语义：批量里每篇都用同一个克隆音色
        voice_ref_text: Option<String>,
        /// 与单篇同一套：句间静音 + 文本兜底开关 + 发音词典（批量产物也受它们影响）
        gap_ms: u64,
        auto_normalize: bool,
        dict: std::collections::BTreeMap<String, String>,
        items: Vec<BatchCmdItem>,
    },
    /// 拼装成品 + SRT。`gap_ms` 是当前的句间静音：停顿改了只要重新导出就能应用，
    /// 不必把已合成的句子再跑一遍。
    Assemble { revision: u64, gap_ms: u64 },
    /// 启动时把已恢复工程交给 worker，保证重开后 Redo/Assemble 仍作用于同一工程。
    OpenProject {
        revision: u64,
        dir: PathBuf,
        project: Project,
    },
    /// UI 的稿件/工程名/模型/参考音已变；旧 current 立即作废，直到下一轮 Run。
    InvalidateProject,
    /// 基于当前配音工程生成并混合 BGM。
    /// `duck_gain` 来自「高级」里的 duck 强度（语义档位在 UI 侧映射成系数）。
    RunBgm {
        revision: u64,
        task_id: u32,
        prompt: String,
        duck_gain: f32,
        /// 没有配音成品时用的目标时长（独立生成 BGM）；有配音成品时忽略，按配音时长对齐。
        standalone_seconds: Option<f64>,
        /// 产物落点：全新机器上 worker 里还没有 current（没载入过工程），
        /// 独立生成 BGM 也要有地方写 bgm/segments 与 bgm/bgm.wav。
        dir: PathBuf,
    },
    /// 音色试听：用指定音色合成一句固定短句，只播不落工程、不改 current。
    PreviewVoice {
        revision: u64,
        model: String,
        voice_ref: Option<String>,
        /// 试听走与成品同一次请求组装；缺文本时按引擎拦（audio8-tts 要、index-tts2 不要）
        voice_ref_text: Option<String>,
        text: String,
    },
    /// 文本描述生成音色（vdes）：用一段描述合成一句试听文本，只播不落工程、
    /// 不改 current。设计形态是 `VoiceSource::Design(description)` —— 不经过克隆字段。
    DesignVoice {
        revision: u64,
        model: String,
        text: String,
        description: String,
    },
    /// 人声分离（本地 htdemucs）：两轨写到 out_dir。
    ///
    /// `task_id` 是任务台账里的 id：分离与"配音工程 revision"无关，但**必须与自身运行对齐**，
    /// 所以所有分离消息都带回 task_id，UI 侧用"是不是当前这条任务"过滤，
    /// 不依赖 revision（否则期间改稿就会把终态消息丢掉、任务永远停在运行中）。
    RunSeparation {
        revision: u64,
        task_id: u32,
        input: PathBuf,
        out_dir: PathBuf,
        stem: String,
        model_dir: Option<PathBuf>,
        chunk_seconds: Option<u32>,
    },
    /// 质检：把已合成句逐句交给 ASR 回读，算可懂度并定位最差句。
    ///
    /// `dir` 是工程目录（worker 自己读 project.json 拿参考文本，避免 UI 再传一份、
    /// 两份不一致）；工程损坏会被 `Project::load_if_present` 拦住并如实报错。
    RunEval {
        revision: u64,
        task_id: u32,
        dir: PathBuf,
        /// 回读用的 ASR 模型 id。由 UI 侧用 `effective_asr_model()` 算好传进来——
        /// worker 自己再读一次设置就有两个真相源，报告与实际请求会漂移。
        model: String,
    },
    /// 歌曲彩蛋生成（独立于配音工程内容，只复用工程目录）。
    RunSong {
        revision: u64,
        task_id: u32,
        project_name: String,
        model: String,
        lyrics: String,
        style: String,
    },
    /// 翻唱（S2）：源音频 → sheetsage2 转 ABC 谱 → yue2 cot=melody 唱新词。
    ///
    /// 与 `RunSong` 共用同一套任务身份（`task_id`）与 `SongDone / SongStopped /
    /// SongFailed` 终态——不新造并行的终态通道。引擎**固定 yue2**，这里不接
    /// `model` 字段，避免 UI 传错引擎（翻唱换引擎不是换模型，是另一条链路）。
    RunCover {
        revision: u64,
        task_id: u32,
        project_name: String,
        lyrics: String,
        style: String,
        source_audio: PathBuf,
    },
}

enum Msg {
    /// worker **真正开始执行**某条排队任务时回报；UI 据此把台账里的 Pending 提升为
    /// Running。与工程版本无关（排队顺序与改稿无关），所以带 revision: 0 且不过滤。
    TaskStarted {
        task_id: u32,
    },
    /// worker 报某条任务"走到哪一步了"。与 TaskStarted 同理：只认 task_id，不认 revision。
    TaskStage {
        task_id: u32,
        stage: String,
    },
    /// 工程已从磁盘载入（含断点状态与句级复用结果），供 UI 在合成前对齐。
    ProjectLoaded {
        project: Project,
        reused: usize,
    },
    /// 句子状态推进（running / done / error）
    Sentence {
        index: usize,
        status: String,
        duration: Option<f64>,
    },
    /// 一轮合成结束：失败句数、是否被停止；`error` 是第一条失败句的完整状态文案，
    /// 用来让状态栏/任务中心看到 OOM 的可执行下一步，而不是只剩一个失败计数。
    RunDone {
        failed: usize,
        stopped: bool,
        reused: usize,
        error: Option<String>,
    },
    /// 手动释放模型内存的终态（成功/失败都带回服务原文）。
    ModelsUnloaded {
        ok: bool,
        note: String,
    },
    /// 批量：某一条开始跑了（UI 把这一行切成"合成本"，并显示第 i/N 条）
    BatchItemStarted {
        task_id: u32,
        index: usize,
        total: usize,
        name: String,
    },
    /// 批量：某一条的句级进度（done = 已完成句数）
    BatchItemProgress {
        task_id: u32,
        index: usize,
        done: usize,
        total: usize,
    },
    /// 批量：某一条收尾。三种终态各有对应字段——
    /// 跑完（wav/srt 都在）、失败（error 有值）、排队中被取消（skipped=true）。
    BatchItemDone {
        task_id: u32,
        index: usize,
        name: String,
        wav: Option<PathBuf>,
        srt: Option<PathBuf>,
        failed: usize,
        /// 这一篇复用了多少句已合成的音频（断点续作/重跑批量时 > 0）
        reused: usize,
        skipped: bool,
        /// 真正的失败原因（服务不可用、拼装失败…）。跳过不带它。
        error: Option<String>,
        /// 跳过的原因（"排队中被取消" / "整批已停止，这篇没跑"）。失败不带它。
        note: Option<String>,
    },
    /// 整批收尾：完成 / 失败 / 跳过各几篇，以及是否被用户停掉
    BatchDone {
        done: usize,
        failed: usize,
        skipped: usize,
        stopped: bool,
    },
    /// 拼装完成（路径给导出用）
    Assembled {
        wav: PathBuf,
        srt: PathBuf,
        duration: f64,
        done: usize,
        skipped: usize,
    },
    /// 单句重录终态；`error=Some` 时保留失败文案。
    RedoDone {
        index: usize,
        error: Option<String>,
    },
    /// 拼装未产出成品（版本不符或 IO 失败）。
    AssembleFailed(String),
    BgmProgress {
        done: usize,
        total: usize,
    },
    BgmDone {
        artifacts: BgmArtifacts,
        segments: usize,
        /// true = 跟配音同框并混音（有 voice/mixed 轨）；false = 独立生成，只有 BGM 轨
        mixed: bool,
    },
    BgmFailed(String),
    /// BGM 在段间被用户停止（已完成 `done` 段）；与失败区分开，不说成"完成"。
    BgmStopped {
        done: usize,
    },
    /// 音色试听合成完成（wav 字节 + 展示用音色名）
    VoicePreview {
        wav: Vec<u8>,
        label: String,
    },
    /// 全局设置里「测试连接」的结果
    ServerHealth {
        ok: bool,
        detail: String,
    },
    /// 托管引擎周期自愈的结果（后台线程发回；与工程版本无关）。
    /// `note` = 有可报告结论时的状态行文案（None = 稳定态/无事发生）。
    EngineSelfHeal {
        note: Option<String>,
    },
    /// 全局设置里「选择模型目录」的结果（三态：选到 / 用户取消 / 选择器不可用）
    ModelDirPicked {
        pick: picker::Outcome<String>,
    },
    /// 一键备份：目标目录选择结果（三态——**用户取消与"选择器不可用"不是一回事**）
    BackupDirPicked {
        pick: picker::Outcome<String>,
    },
    /// 一键备份终态：`Ok(note)` 是给状态行的那句话，`Err` 是**可执行**的失败原因。
    /// 后台线程发回、与工程版本无关（用户点按钮那一刻和稿件版本没关系）。
    BackupDone {
        result: Result<String, String>,
    },
    /// 检查更新的终态。后台线程发回、与工程版本无关（检查只读网络，和稿件没关系），
    /// 所以必须进 `message_ignores_revision`：否则改一次稿就会把结果静默丢掉，
    /// 界面永远停在「检查中…」。
    UpdateCheckDone {
        result: Result<update::UpdateCheck, String>,
    },
    /// 人声分离进度 / 终态（都带 task_id 以便与当前任务对齐）
    SeparationProgress {
        task_id: u32,
        percent: f32,
        note: String,
    },
    SeparationDone {
        task_id: u32,
        /// 本次任务的输入音频（只写入历史元数据；回看两轨不依赖它）
        input: PathBuf,
        /// 本次任务的工程内输出目录（成功记录必须落在它下面的 history.json）
        out_dir: PathBuf,
        vocals: PathBuf,
        accompaniment: PathBuf,
    },
    SeparationStopped {
        task_id: u32,
    },
    SeparationFailed {
        task_id: u32,
        error: String,
    },
    /// 模型下载：排队/下载/校验/终态都走这一条（带任务 id 自证身份，
    /// 与工程版本无关——改稿不该把下载进度静默丢掉）
    DownloadUpdate(download::Snapshot),
    /// 批量导出跑完了（后台线程回报；不是 worker 任务，不进任务台账）
    BatchExportDone {
        /// 导出到哪个目录（消息自带，别在用的时候再读一次界面值——那个值可能已经变了）
        dir: PathBuf,
        outcome: export::BatchExportOutcome,
    },
    /// 选完要导入的词条文件（.tsv/.csv/.txt）
    DictFilePicked {
        pick: picker::Outcome<String>,
    },
    /// 选完要导入的音色目录（里面有 voice.json + 音频）
    VoiceImportDirPicked {
        pick: picker::Outcome<String>,
    },
    /// 「添加音频…」选完的音频文件（多选，按文件名直接入库；三态）
    VoiceFilesPicked {
        pick: picker::Outcome<Vec<PathBuf>>,
    },
    /// 参考音频行「选择…」选完的文件（单选；结果与手填路径同一套作废语义）
    ReferenceAudioPicked {
        pick: picker::Outcome<String>,
    },
    /// 批量：系统文件框选完的多篇稿件（三态；取消与"选择器不可用"分开）
    BatchScriptsPicked {
        pick: picker::Outcome<Vec<PathBuf>>,
    },
    /// 选择待分离音频的结果（三态）
    SeparationInputPicked {
        pick: picker::Outcome<String>,
    },
    /// 翻唱源音频选择结果（三态；与分离同一套 picker 语义）
    SongSourcePicked {
        pick: picker::Outcome<String>,
    },
    /// 音色试听失败（保留音色名，便于在状态栏说清是哪个音色挂了）
    VoicePreviewFailed {
        label: String,
        error: String,
    },
    /// 文本描述生成音色完成（wav 字节 + 生成时用的试听文本 + 设计模型）。
    ///
    /// `text` 必须随消息回来：点「用作配音音色」时它就是 reference_text，
    /// 不能再去读界面输入框（那个值可能已经被用户改过了）。
    DesignVoiceDone {
        wav: Vec<u8>,
        text: String,
        model: String,
    },
    /// 文本描述生成音色失败。
    DesignVoiceFailed {
        error: String,
        model: String,
    },
    /// 参考音频的**自动转写**结果（后台线程发回，与工程版本无关）。
    ///
    /// 只回结果、不直接写界面：转写是"帮用户填"，不是"替用户决定"——
    /// 调用方要把文本回显出来让人核对（ASR 错一个字，克隆出的音色就跑偏）。
    ReferenceTranscribed {
        /// 转写用的 ASR 模型（成功时一并回显，失败原因里也要带）
        model: String,
        result: Result<String, String>,
    },
    /// 歌曲终态：带 task_id 与分离同理——歌曲可以排在别的任务后面，跨改稿时
    /// 用 revision 过滤会把终态丢掉、任务永远停在"运行中"。
    SongDone {
        task_id: u32,
        path: PathBuf,
        duration: f64,
    },
    SongStopped {
        task_id: u32,
    },
    SongFailed {
        task_id: u32,
        error: String,
    },
    /// 质检进度：已回读 done / 共 total 句
    EvalProgress {
        task_id: u32,
        done: usize,
        total: usize,
        /// 本次实际在用的回读模型：进度文案念它，UI 侧不再自己算一份
        model: String,
    },
    EvalDone {
        task_id: u32,
        summary: EvalSummary,
    },
    EvalStopped {
        task_id: u32,
    },
    EvalFailed {
        task_id: u32,
        error: String,
    },
    /// 工作线程无法继续的错误
    Fatal(String),
}

/// 质检汇总：平均可懂度 + 最差几句（够用户直接去重录那几句）。
#[derive(Clone, Debug)]
struct EvalSummary {
    /// **本次实际用的**回读模型（与发给服务的 model 同一个值，报告/状态行都念它）
    model: String,
    /// 平均可懂度百分比（只算转写成功的句子）
    percent: f64,
    scored: usize,
    /// ASR 转写失败的句数（服务端错误等）——不混进平均分，但要报出来
    asr_failed: usize,
    /// 第一条 ASR 失败的服务端原文（OOM 等），用于质检摘要里的可执行下一步。
    asr_error: Option<String>,
    /// 最差 N 句（按可懂度升序；只含有差异的句子）
    worst: Vec<EvalIssue>,
    /// 跑完之后**工程里的完整分数集**：句 index → （可懂度% + 这份分的来源模型）。
    ///
    /// 来源模型挂在同一个元组里，不另开一个 `Vec` —— 两处分别维护必然漂移，
    /// 而这里一漂移，界面就会把"分是谁测的"说错（本批要修的就是这个）。
    /// 元组第二项是 `Option`：`None` = 这份分**来源未知**（本字段引入前的旧记录）。
    scores: Vec<(usize, f64, Option<String>)>,
    /// 分数/报告没能落盘时的说明（都成功为 None）——分数仍然有效，但要如实告诉你它没落盘
    persist_warning: Option<String>,
    /// 质检报告的落盘路径（人可读的逐句对照表；写失败为 None）
    report_path: Option<PathBuf>,
}

/// 质检报告里的一行：一句的参考文本、ASR 回读、得分与首个差异。
#[derive(Clone, Debug)]
struct EvalRow {
    index: usize,
    reference: String,
    hypothesis: String,
    percent: f64,
    snippet: String,
}

#[derive(Clone, Debug)]
struct EvalIssue {
    /// 句子序号（0 基，与界面"N 句"一致）
    index: usize,
    percent: f64,
    snippet: String,
}

struct WorkerMsg {
    revision: u64,
    msg: Msg,
}

fn worker_message_is_current(worker_msg: &WorkerMsg, revision: u64) -> bool {
    worker_msg.revision == revision
}

// ===========================================================================
// 服务发现：server.json → 音色清单 + 服务地址
// ===========================================================================

/// 全局设置（跨 Tab 的基础设施）：本应用连哪个服务、从哪份清单读模型。
///
/// 只覆盖**本应用的行为**，不改写 audio.cpp 自己的配置：
///   · host/port 覆盖清单里的服务地址（各自独立回落；服务自身的监听地址由
///     audio-service 启动参数决定）
///   · model_dir 是本机模型文件存放位置，用于核对模型盘；**不**决定引擎 id
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct AppSettings {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    /// 模型目录：本机模型文件的存放位置。缺省 = `default_model_dir()`
    /// （跟着服务清单里模型 path 的公共父目录走，推不出来才回落 <应用工作目录>/models）。
    #[serde(default)]
    model_dir: Option<String>,
    /// 当前启用的发音词典（库内文件名）；缺省 = 不启用。
    #[serde(default)]
    dictionary: Option<String>,
    /// BGM 生成的输入（描述 / 压低档位 / 独立生成时长档位）。
    ///
    /// 以前这些只活在 UI 内存里：重启回默认值，用户写的描述白写；跨会话也没法判断
    /// "磁盘上那套 BGM 还算不算当前结果"，只能一律当作过期。落进 settings.json 之后
    /// 两件事一起解决（判据见 src/export.rs 的产物清单）。
    #[serde(default)]
    bgm: BgmSettings,
    /// 「检查更新」的发布清单地址覆盖（给内网/镜像留的口子）。
    /// 缺省 = `src/update.rs::DEFAULT_MANIFEST_URL`（GitHub 最新 Release API）。
    #[serde(default)]
    update_url: Option<String>,
    /// 质检回读（ASR）用哪个模型：在服务清单 `task == "asr"` 的模型里选。
    ///
    /// 缺省 = `aw_core::DEFAULT_ASR_MODEL`（qwen3-asr，M0 定标同款）。
    /// **唯一入口是 `effective_asr_model()`**：worker 真的拿它去请求、界面回显、
    /// 质检报告落盘，三处必须是同一份推导（两份实现必然漂移）。
    #[serde(default)]
    asr_model: Option<String>,
    /// 音色设计用哪个模型：在服务清单 `task == "vdes"` 的模型里选。
    ///
    /// 缺省 = 清单里第一个可用设计模型。与 ASR 一样，用户选的 id 不在当前清单时
    /// **照用不改**，由服务拒绝并如实报错，避免静默换模型。
    #[serde(default)]
    design_model: Option<String>,
    /// 模型下载源镜像前缀（M4-P7）：缺省/空 = 用清单里的官方地址。
    ///
    /// 只有 HF 官方 URL 会被改写（`src/download_mirror.rs` 是唯一实现）；
    /// 填了镜像就**只走镜像**——地址不通就如实报错，不静默回退官方
    /// （"以为在用镜像、其实偷偷走官方"是本仓反复抓的失败形态）。
    #[serde(default)]
    download_mirror: Option<String>,
    /// 同时下载几个模型（默认 2，夹到 1..=4）。归一化只有
    /// `download::effective_concurrency()` 一处，界面回显也从它算。
    #[serde(default)]
    download_concurrency: Option<u32>,
}

/// BGM 的默认描述：**与 ui/app.slint 里 `bgm-prompt` 的默认值必须一致**
/// （有单测用 include_str! 钉住，两边不一致就会红）。
const DEFAULT_BGM_PROMPT: &str = "温暖克制的科技感口播背景音乐，钢琴与轻电子，无人声，循环友好";

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct BgmSettings {
    #[serde(default = "default_bgm_prompt")]
    prompt: String,
    /// duck 强度档位（0 弱 / 1 中 / 2 强；换算见 `duck_gain_for`）
    #[serde(default = "default_bgm_index")]
    duck_index: i32,
    /// 独立生成时的目标时长档位
    #[serde(default = "default_bgm_index")]
    standalone_index: i32,
}

fn default_bgm_prompt() -> String {
    DEFAULT_BGM_PROMPT.to_string()
}

fn default_bgm_index() -> i32 {
    1
}

impl Default for BgmSettings {
    fn default() -> Self {
        Self {
            prompt: default_bgm_prompt(),
            duck_index: default_bgm_index(),
            standalone_index: default_bgm_index(),
        }
    }
}

/// 旧默认值：`<应用工作目录>/models`（打包后即应用目录下的 models/）。
/// **清单里推不出模型根时的回落。**
///
/// cwd 是 `/`（Finder 双击启动的常见情况）时退回可执行文件所在目录：
/// 否则默认值成了 `/models`，界面只说"目录不存在"，看不出根因。
fn fallback_model_dir() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let base = match cwd {
        Some(d) if d != Path::new("/") => d,
        _ => exe_dir.unwrap_or_else(|| PathBuf::from(".")),
    };
    base.join("models")
}

/// 从清单里的模型 `path` 反推「模型根目录」。**纯函数**（落盘探测走 `volume` 注入）。
///
/// 语义：根 = 所有非空 `path` 的**父目录**的最长公共祖先。文件与目录两种 `path` 一视同仁
/// 取 `parent()`——`path` 可能指向文件（`…/Qwen3-ASR-0.6B-GGUF/x.gguf`），也可能指向目录
/// （gen 类 `…/Yue2-3B-GGUF`），我们要的是"服务会去哪一层找"的那层，不是某个模型自己的子目录。
///
/// 返回 `None` 的几种情形都是**推不出可信答案**，调用方必须回落，不许瞎猜一个根：
/// 1. 没有可用 path（清单缺、字段全空）；
/// 2. 有相对路径——服务按它自己的 cwd 解析，我们猜不到；让它参与比较会算出错误的公共根；
/// 3. 跨根（不同卷 / 不同盘）——硬算出的是 `/Volumes` 这种"挂载点容器"，当模型目录用只会
///    把权重放到服务永远不看的地方；
/// 4. 公共祖先只剩文件系统根（Unix `/`、Windows `C:\`）——等于什么都没推出来。
fn model_root_from_paths(paths: &[&str]) -> Option<PathBuf> {
    model_root_with(paths, &volume_of)
}

/// `volume` = 「这条路径所在的那个根」的标识（Unix: `st_dev`；Windows: 盘符 / UNC 前缀）。
///
/// 参数化是为了让「跨根必须回落」这条**可测**：一台机器上造不出第二个文件系统，
/// 不注入就只能写一条换个平台就恒真的断言。
fn model_root_with(paths: &[&str], volume: &dyn Fn(&Path) -> Option<String>) -> Option<PathBuf> {
    let mut common: Option<PathBuf> = None;
    let mut vol: Option<String> = None;
    for raw in paths {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let p = Path::new(raw);
        if !p.is_absolute() {
            return None;
        }
        let parent = p.parent()?.to_path_buf();
        let v = volume(&parent)?;
        match &vol {
            None => vol = Some(v),
            Some(seen) if *seen == v => {}
            Some(_) => return None,
        }
        common = Some(match common {
            None => parent,
            Some(acc) => common_ancestor(&acc, &parent)?,
        });
    }
    let root = common?;
    // 收敛到文件系统根不算"公共父目录"：`/` 或 `C:\` 当模型目录用毫无意义，回落。
    root.parent()?;
    Some(root)
}

/// 两条绝对路径的最长公共祖先目录。按**路径组件**比，不是字符串前缀：
/// `/models-2` 不会被当成 `/models` 里面（见 `LESSON_路径包含判定必须按真实路径而非字面前缀.md`）。
fn common_ancestor(a: &Path, b: &Path) -> Option<PathBuf> {
    a.ancestors()
        .find(|c| b.starts_with(c))
        .map(Path::to_path_buf)
}

/// 路径所在文件系统的标识。路径可能还不存在（模型还没下过），所以先向上找最近的**存在**祖先。
/// 找不到（相对路径、整条链都不可达）→ `None`，调用方整批回落。
#[cfg(unix)]
fn volume_of(p: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let existing = nearest_existing(p)?;
    std::fs::metadata(existing)
        .ok()
        .map(|m| m.dev().to_string())
}

/// Windows：卷的标识就是路径前缀（`C:` / `\\server\share`）。
/// 不用 `volume_serial_number()`：那个要求路径存在，而模型目录常常还没建出来。
#[cfg(not(unix))]
fn volume_of(p: &Path) -> Option<String> {
    match p.components().next()? {
        std::path::Component::Prefix(pre) => Some(pre.as_os_str().to_string_lossy().to_uppercase()),
        _ => None,
    }
}

/// 最近的、真实存在的祖先；一个都没有就是 `None`。
///
/// 只有 Unix 的 `volume_of` 用它（Windows 用路径前缀当卷标识，不需要碰盘）。
/// 不加 cfg 会让 Windows 侧的 `-D warnings` 挂在 dead_code 上。
#[cfg(unix)]
fn nearest_existing(p: &Path) -> Option<PathBuf> {
    let mut cur = Some(p);
    while let Some(c) = cur {
        if c.exists() {
            return Some(c.to_path_buf());
        }
        cur = c.parent();
    }
    None
}

/// 默认模型目录：**优先跟着服务清单的模型根走**（`server.json` 里那些 path 的公共父目录），
/// 推不出来才回落到 `<应用工作目录>/models`。
///
/// 动机（2026-09-17 真机）：默认值是 `<cwd>/models`，本机根本不存在，而清单里的模型 path 全在
/// `/Volumes/DataExt/models/...` —— 抽屉里「模型目录」长期显示“目录不存在（清单里有 N 个模型）”，
/// 下载面板的入口**每条**都挂着「⚠ 落点与 server.json 对不上」（照默认值下完，服务确实找不到）。
fn default_model_dir() -> PathBuf {
    default_model_dir_from(read_server_config().as_ref())
}

/// `default_model_dir()` 的可注入版本（清单由调用方给；单测不碰进程环境变量，见
/// `LESSON_多线程测试中set_var修改进程环境是UB`）。
fn default_model_dir_from(cfg: Option<&ServerConfig>) -> PathBuf {
    cfg.and_then(|c| {
        let paths: Vec<&str> = c.models.iter().map(|m| m.path.as_str()).collect();
        model_root_from_paths(&paths)
    })
    .unwrap_or_else(fallback_model_dir)
}

/// 当前生效的模型目录（显式设置 > 默认）。**这是唯一入口**，别再写第二份"哪个目录生效"的判据。
fn model_dir() -> PathBuf {
    effective_model_dir(&settings_snapshot(), read_server_config().as_ref())
}

/// `model_dir()` 的纯投影（两个输入都由调用方给，便于单测）。
///
/// 只有设置了 `model_dir` 才走设置：推导出来的默认值**永远不许盖掉用户选过的目录**
/// （界面回显、扫描、下载落点、设置回存都消费 `model_dir()`，所以它们不会各说各话）。
fn effective_model_dir(s: &AppSettings, cfg: Option<&ServerConfig>) -> PathBuf {
    s.model_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_model_dir_from(cfg))
}

/// 目录扫描的条目上限：目录是用户自选的，选到 $HOME 或 / 时不至于把 UI 线程扫死。
const SCAN_ENTRY_LIMIT: usize = 20_000;

/// 扫模型目录（深度 ≤2，覆盖 `models/<模型名>/*.gguf` 这种常见摆放）：
/// 返回 (目录是否存在, .gguf 文件数)。
fn scan_model_dir(dir: &Path) -> (bool, usize) {
    scan_model_dir_with_limit(dir, SCAN_ENTRY_LIMIT)
}

/// 带上限的扫描（上限可注入，便于测试）。`limit` 是**整次扫描**的总条目预算，
/// 不是每个目录各自的上限——否则 20000 个子目录 × 每层 20000 条照样能扫死 UI 线程
/// （审查指出过这点）。预算耗尽即停，结果是"至少这么多"。
fn scan_model_dir_with_limit(dir: &Path, limit: usize) -> (bool, usize) {
    if !dir.is_dir() {
        return (false, 0);
    }
    fn count_gguf(dir: &Path, depth: usize, budget: &mut usize) -> usize {
        if *budget == 0 {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut n = 0;
        for e in entries.flatten() {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let path = e.path();
            if path.is_dir() {
                if depth > 0 {
                    n += count_gguf(&path, depth - 1, budget);
                }
            } else if path
                .extension()
                .map(|x| x.eq_ignore_ascii_case("gguf"))
                .unwrap_or(false)
            {
                n += 1;
            }
        }
        n
    }
    let mut budget = limit;
    (true, count_gguf(dir, 2, &mut budget))
}

/// 模型目录里有多少个清单模型的权重文件（用来判断模型盘挂上没）。
///
/// 包含判定走 `crate::paths::resolve_for_compare`（**真实路径**，会消解 `..` 与软链）：
/// 清单里写 `/models/x/../in/y.gguf`、或模型目录本身是软链时，词法的 `Path::starts_with`
/// 会漏报/误报——这正是本仓踩过两次的坑（见
/// `LESSON_路径包含判定必须按真实路径而非字面前缀.md`）。既然已有唯一入口就别再各来一份。
///
/// 单条路径解析不了（理论上不会）就当作"不在里面"，不把整次统计打断。
fn models_under_dir(cfg: &Option<ServerConfig>, dir: &Path) -> usize {
    let Some(cfg) = cfg else { return 0 };
    let Ok(base) = crate::paths::resolve_for_compare(dir, dir) else {
        return 0;
    };
    cfg.models
        .iter()
        .filter(|m| !m.path.is_empty())
        .filter(|m| {
            crate::paths::resolve_for_compare(Path::new(&m.path), &base)
                .map(|p| p.starts_with(&base))
                .unwrap_or(false)
        })
        .count()
}

/// 设置文件：与工程产物同目录，便于用户找到与备份。
fn settings_path() -> PathBuf {
    // 与 projects_root()/export_dir() 同源：走系统 Documents（Windows 上是
    // %USERPROFILE%\Documents），不再写死 HOME —— 否则 Windows 上会落到相对路径，
    // 换个目录启动就相当于"设置丢失"。
    documents_dir().join(WORKSHOP_DIR).join("settings.json")
}

/// 读 settings.json。
///
/// **逐字段解析**：某一段坏掉（手改错、版本不兼容）只丢那一段，不要连带把 host/port
/// 也清掉——整份 `from_str::<AppSettings>` 失败会让用户"设置全没了"。
/// 词典库根目录（每套词典一个 JSON）。
fn dictionaries_root() -> PathBuf {
    documents_dir().join(WORKSHOP_DIR)
}

/// 词典列表 → 界面（名字 + 词条数；坏文件计数）。
fn refresh_dictionaries(ui: &MainWindow, state: &Rc<UiState>) {
    let (rows, broken) = dictionaries::list(&dictionaries_root());
    // 下拉的**第 0 项**固定是"不使用词典"（与 `dict_index` 的映射一致）：
    // 只放库里的名字会让 index 0 显示成第一套词典、而 Rust 按"不使用"处理。
    let mut names: Vec<SharedString> = vec![SharedString::from("不使用词典")];
    names.extend(
        rows.iter()
            .map(|r| SharedString::from(format!("{}（{} 条）", r.name, r.count))),
    );
    ui.set_dict_names(ModelRc::from(Rc::new(VecModel::from(names))));
    // 下拉第 0 项固定是「不使用词典」，所以库里的排 1..n
    let active = state.active_dict_file.borrow().clone();
    let position = active
        .as_deref()
        .and_then(|f| rows.iter().position(|r| r.file == f));
    if active.is_some() && position.is_none() {
        // 启用的那套被删了/读不出来了：必须把内存里的词条也清掉，
        // 否则下一次合成还会用着"界面上已经没有"的那套读法
        *state.active_dict_file.borrow_mut() = None;
        state.active_dict.borrow_mut().clear();
        persist_active_dictionary(ui, None);
        ui.set_status_text("启用的词典已不在库里，已退回「不使用词典」".into());
    }
    ui.set_dict_index(position.map(|i| i as i32 + 1).unwrap_or(0));
    let mut status = if rows.is_empty() {
        "还没有词典".to_string()
    } else {
        format!("{} 套词典", rows.len())
    };
    if broken > 0 {
        status.push_str(&format!("（{broken} 个文件读不出来，已跳过）"));
    }
    ui.set_dict_status(status.into());
}

/// 把"启用某套词典"落进 settings.json（失败也不阻断，只提示）。
fn persist_active_dictionary(ui: &MainWindow, file: Option<&str>) {
    let snapshot = {
        let mut guard = match settings().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if guard.dictionary.as_deref() == file {
            return;
        }
        guard.dictionary = file.map(str::to_string);
        guard.clone()
    };
    if let Err(e) = save_settings(&snapshot) {
        ui.set_status_text(format!("词典选择没能保存（{e}）：重启后会回到上次的选择").into());
    }
}

/// 启用一套词典（或取消启用）：换词典 = 改 spoken 文本 → 与换模型同类，需要重录。
fn activate_dictionary(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    state: &Rc<UiState>,
    cmd_tx: &Sender<Cmd>,
    file: Option<String>,
) {
    let entries = match file.as_deref() {
        None => std::collections::BTreeMap::new(),
        Some(f) => match dictionaries::load_file(&dictionaries_root(), f) {
            Ok(d) => d.entries,
            Err(e) => {
                ui.set_status_text(e.into());
                return;
            }
        },
    };
    *state.active_dict_file.borrow_mut() = file.clone();
    *state.active_dict.borrow_mut() = entries;
    persist_active_dictionary(ui, file.as_deref());
    refresh_dictionaries(ui, state);
    // 词典改的是"这个词该怎么念"→ 旧音频不能复用（与兜底开关同类）
    invalidate_worker_project(cmd_tx, state);
    reset_bgm(ui, state);
    state.assembled.borrow_mut().take();
    clear_eval_scores(ui, rows, state);
    ui.set_has_result(false);
    let what = file
        .as_deref()
        .and_then(|f| dictionaries::load_file(&dictionaries_root(), f).ok())
        .map(|d| d.name)
        .unwrap_or_else(|| "不使用词典".to_string());
    ui.set_status_text(format!("词典已切到「{what}」：需要重新合成（旧读法不再复用）").into());
}

/// 音色库根目录（库目录 + 索引都在它下面）：与应用设置、导出目录同级。
fn voices_root() -> PathBuf {
    documents_dir().join(WORKSHOP_DIR)
}

/// 音色库列表 → 界面（当前工程正在用的那条打上 ✓）。
fn refresh_voice_library(ui: &MainWindow) {
    let in_use = non_empty(ui.get_voice_ref_path().to_string());
    match voices::load(&voices_root()) {
        Ok(index) => {
            let rows: Vec<VoiceLibRow> = index
                .voices
                .iter()
                .map(|v| {
                    let path = voices::audio_path(&voices_root(), v);
                    VoiceLibRow {
                        name: v.name.clone().into(),
                        note: v.note.clone().into(),
                        created: relative_time(now_ms(), v.created_at).into(),
                        current: in_use
                            .as_deref()
                            .map(|p| Path::new(p) == path)
                            .unwrap_or(false),
                    }
                })
                .collect();
            ui.set_library_status(
                if rows.is_empty() {
                    "音色库还是空的".to_string()
                } else {
                    format!("{} 个音色", rows.len())
                }
                .into(),
            );
            ui.set_library_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
        }
        Err(e) => {
            // 坏索引不静默变空列表：用户得知道"我的音色还在不在"
            ui.set_library_rows(ModelRc::from(Rc::new(VecModel::from(Vec::new()))));
            ui.set_library_status(format!("音色库读不出来：{e}").into());
        }
    }
}

/// 音色库（P4）：保存当前参考音 / 应用 / 导出 / 导入。
fn wire_voice_library(ui: &MainWindow, ctx: &VoiceLibraryCtx, state: &Rc<UiState>) {
    // 存入音色库（把当前参考音频复制进库，自包含）
    let weak = ui.as_weak();
    let st_save = state.clone();
    ui.on_library_save(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &st_save) {
            ui.set_status_text("任务进行中：音色库等这轮跑完再改".into());
            return;
        }
        let Some(src) = non_empty(ui.get_voice_ref_path().to_string()) else {
            ui.set_status_text("先在上面选一段参考音频（或填路径），再存入音色库".into());
            return;
        };
        match voices::add_from_file(
            &voices_root(),
            &ui.get_library_name_text(),
            Path::new(&src),
            &ui.get_library_note(),
            now_ms(),
            file_stem,
        ) {
            Ok(entry) => {
                ui.set_library_name_text("".into());
                ui.set_library_note("".into());
                refresh_voice_library(&ui);
                ui.set_status_text(
                    format!("已存入音色库：「{}」（原文件删了也能用）", entry.name).into(),
                );
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    // 备注改了：只有存/导出时才用到，这里只刷新一句提示（不需要落盘）
    let weak = ui.as_weak();
    ui.on_library_note_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_status_text("备注会跟着音色一起保存/导出".into());
    });

    // 应用库里某条：把参考音频换成库里的副本（走既有的"换音色需重录"作废语义）
    let weak = ui.as_weak();
    let st = state.clone();
    let tx = ctx.cmd_tx.clone();
    ui.on_library_apply(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &st) || batch_in_flight(&st) {
            ui.set_status_text("任务进行中：音色等这轮跑完再换".into());
            return;
        }
        let index = match voices::load(&voices_root()) {
            Ok(i) => i,
            Err(e) => {
                ui.set_status_text(e.into());
                return;
            }
        };
        let Some(entry) = index.get(name.as_str()).cloned() else {
            ui.set_status_text(format!("音色库里没有「{name}」").into());
            return;
        };
        // 文件名校验 + 库内路径 + 文件存在，三件事都在这里（手改索引也挡得住）
        let path = match voices::usable_audio_path(&voices_root(), &entry) {
            Ok(p) => p,
            Err(e) => {
                ui.set_status_text(e.into());
                return;
            }
        };
        if non_empty(ui.get_voice_ref_path().to_string()).as_deref()
            == Some(path.to_string_lossy().as_ref())
        {
            // UI 上这条已经禁用了，回调里再兜一下：避免"重复应用"白白作废一次工程
            ui.set_status_text(format!("已经在用「{}」", entry.name).into());
            return;
        }
        // 走唯一入口：换到另一段音频 ⇒ 上一段名下的转写一起清（否则会拿它的文本
        // 当条件去克隆新音频，服务端不报错但声音已经不是用户要的那个）
        set_voice_ref(&ui, path.to_string_lossy().as_ref());
        // 与手工改参考音频同一条路：作废工程与成品，提示需要重新合成
        invalidate_worker_project(&tx, &st);
        reset_bgm(&ui, &st);
        st.assembled.borrow_mut().take();
        ui.set_has_result(false);
        refresh_voice_labels(&ui);
        refresh_voice_library(&ui);
        ui.set_status_text(format!("已切到音色库的「{}」：需要重新合成", entry.name).into());
    });

    // 导出：写到导出目录下的 voice-lib/<名字>/（目录里是 voice.json + 音频）
    let weak = ui.as_weak();
    ui.on_library_export(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        let dest = PathBuf::from(ui.get_export_dir().to_string()).join("voice-lib");
        match voices::export_to(&voices_root(), name.as_str(), &dest, file_stem) {
            Ok(dir) => {
                toast(&ui, &format!("已导出 {}", file_label(&dir)));
                ui.set_status_text(
                    format!(
                        "已导出「{name}」到 {}（拷走这个目录就能在别的机器导入）",
                        dir.display()
                    )
                    .into(),
                );
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    // 导入：选一个"音色目录"（里面有 voice.json + 音频）
    let weak = ui.as_weak();
    let msg = ctx.msg_tx.clone();
    ui.on_library_import(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_status_text("正在打开目录选择框（选导出的那个音色目录）…".into());
        let msg = msg.clone();
        std::thread::spawn(move || {
            let pick = picker::pick_folder("选择要导入的音色目录");
            let _ = msg.send(WorkerMsg {
                revision: 0,
                msg: Msg::VoiceImportDirPicked { pick },
            });
        });
    });

    // 添加音频…：选一个/多个音频文件按文件名直接入库（不动当前工程音色）
    let weak = ui.as_weak();
    let msg = ctx.msg_tx.clone();
    ui.on_library_add_audio(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_status_text("正在打开文件选择框（选要加入音色库的音频，可多选）…".into());
        let msg = msg.clone();
        std::thread::spawn(move || {
            let pick = picker::pick_files(
                "选择要加入音色库的音频",
                "音频",
                &["*.wav", "*.mp3", "*.flac", "*.m4a", "*.ogg"],
            );
            let _ = msg.send(WorkerMsg {
                revision: 0,
                msg: Msg::VoiceFilesPicked { pick },
            });
        });
    });
}

/// 发音词典（P5）：切换 / 导入词条 / 导出词条。
fn wire_dictionary(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    cmd_tx: &Sender<Cmd>,
    msg_tx: &Sender<WorkerMsg>,
    state: &Rc<UiState>,
) {
    // 切换启用的词典（第 0 项 = 不使用）
    let weak = ui.as_weak();
    let rows_for_pick = rows.clone();
    let st = state.clone();
    let tx = cmd_tx.clone();
    ui.on_dict_picked(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &st) || batch_in_flight(&st) {
            ui.set_status_text("任务进行中：词典等这轮跑完再换".into());
            refresh_dictionaries(&ui, &st);
            return;
        }
        let file = if i <= 0 {
            None
        } else {
            let (rows, _) = dictionaries::list(&dictionaries_root());
            rows.get((i - 1) as usize).map(|r| r.file.clone())
        };
        activate_dictionary(&ui, &rows_for_pick, &st, &tx, file);
    });

    // 导入词条
    let weak = ui.as_weak();
    let msg = msg_tx.clone();
    let st_import = state.clone();
    ui.on_dict_import(move || {
        let Some(ui) = weak.upgrade() else { return };
        if dictionary_controls_busy(&ui, &st_import) {
            ui.set_status_text("任务进行中：词典等这轮跑完再导入".into());
            return;
        }
        ui.set_status_text("正在打开文件选择框（词条 .tsv/.csv/.txt）…".into());
        let msg = msg.clone();
        std::thread::spawn(move || {
            let pick = pick_text_file_blocking();
            let _ = msg.send(WorkerMsg {
                revision: 0,
                msg: Msg::DictFilePicked { pick },
            });
        });
    });

    // 导出当前启用的词典
    let weak = ui.as_weak();
    let st_export = state.clone();
    ui.on_dict_export(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(file) = st_export.active_dict_file.borrow().clone() else {
            ui.set_status_text("当前没有启用词典：先选一套".into());
            return;
        };
        let dest = PathBuf::from(ui.get_export_dir().to_string());
        match dictionaries::export_tsv(&dictionaries_root(), &file, &dest, file_stem) {
            Ok(path) => {
                toast(&ui, &format!("已导出 {}", file_label(&path)));
                ui.set_status_text(
                    format!("词条已导出到 {}（可直接编辑后再导入）", path.display()).into(),
                );
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });
}

/// 词典区现在能不能动（任务在飞/批量在飞都不行）。
fn state_dictionary_idle(state: &Rc<UiState>) -> bool {
    !project_editing_blocked_dummy(state) && !batch_in_flight(state)
}

/// 词典控件的统一忙碌判据：UI 在飞（配音/BGM/试听）+ **台账里任何任务在飞**
/// （含歌曲/分离/质检这类不设全局 `running`/`busy` 的）+ 批量在飞。
///
/// 点击时与**异步回调到达时**必须用同一个：文件框打开后用户可能已经起了别的任务。
fn dictionary_controls_busy(ui: &MainWindow, state: &Rc<UiState>) -> bool {
    ui.get_running() || ui.get_busy() || !state_dictionary_idle(state)
}

/// 词条导入的应用结果（供状态行报告）。
#[derive(Debug)]
struct DictImportApplied {
    entry: dictionaries::Entry,
    imported: usize,
    skipped: Vec<String>,
}

/// 把词条文件导入词典库（**不含文件框**）。
///
/// `busy` 由调用方用 `dictionary_controls_busy` 算好传进来——这样"文件框打开期间起了任务"
/// 这条竞态就能被单测直接覆盖（复核要求：回归要真的覆盖 handler 用的那条判据，而不只是判据本身）。
fn import_entries_into_library(
    root: &Path,
    path: &Path,
    active_file: Option<&str>,
    busy: bool,
    now_ms: u64,
) -> Result<DictImportApplied, String> {
    if busy {
        return Err("任务进行中：这次导入没有应用（等这轮跑完再导入）".into());
    }
    let outcome = dictionaries::import_file(path)?;
    if outcome.entries.is_empty() {
        return Err(if outcome.skipped.is_empty() {
            "这个文件里没有词条".to_string()
        } else {
            format!("没有可导入的词条（{} 条被跳过）", outcome.skipped.len())
        });
    }
    // 目标词典：当前启用的那套；没启用就按文件名找/建一套。
    // **库里已有同名（未启用）的那套时，先把它的词条读出来再合并**——
    // 直接 save 会按"同名覆盖"把原词条丢掉。
    let active_loaded = active_file.and_then(|f| dictionaries::load_file(root, f).ok());
    let fallback_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("导入的词典")
        .to_string();
    let current_name = active_loaded
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or(fallback_name);
    let mut merged = match active_loaded {
        Some(d) => d.entries,
        None => {
            let (rows, _) = dictionaries::list(root);
            rows.iter()
                .filter(|r| r.name.eq_ignore_ascii_case(&current_name))
                .find_map(|r| dictionaries::load_file(root, &r.file).ok())
                .map(|d| d.entries)
                .unwrap_or_default()
        }
    };
    let imported = outcome.entries.len();
    for e in &outcome.entries {
        merged.insert(e.from.clone(), e.to.clone());
    }
    let entry = dictionaries::save(root, &current_name, merged, now_ms, file_stem)?;
    Ok(DictImportApplied {
        entry,
        imported,
        skipped: outcome.skipped,
    })
}

/// `project_editing_blocked` 需要 `&MainWindow`，这里只用状态判"有没有任务在飞"。
fn project_editing_blocked_dummy(state: &Rc<UiState>) -> bool {
    state
        .tasks
        .borrow()
        .counts()
        .pending
        .saturating_add(state.tasks.borrow().counts().running)
        > 0
}

/// 启动时按 settings.json 里记的那套启用词典（不触发作废——启动时没有"旧成品"要作废）。
fn load_active_dictionary(ui: &MainWindow, state: &Rc<UiState>) {
    let file = settings_snapshot().dictionary;
    if let Some(f) = file.as_deref() {
        match dictionaries::load_file(&dictionaries_root(), f) {
            Ok(d) => {
                *state.active_dict_file.borrow_mut() = Some(f.to_string());
                *state.active_dict.borrow_mut() = d.entries;
            }
            Err(e) => {
                // 启用的那套坏了：退回"不启用"，但要说清（不能静默换读法）
                *state.active_dict_file.borrow_mut() = None;
                state.active_dict.borrow_mut().clear();
                ui.set_status_text(format!("上次启用的词典读不出来，已退回不启用：{e}").into());
            }
        }
    }
    refresh_dictionaries(ui, state);
}

/// 「导入音色」这套接线要用的两个 sender（与其它 wire_* 一样按引用传）。
struct VoiceLibraryCtx {
    cmd_tx: Sender<Cmd>,
    msg_tx: Sender<WorkerMsg>,
}

/// 模板文件：与 settings.json 同目录（用户备份/迁移时一处就够）。
fn templates_path() -> PathBuf {
    settings_path().with_file_name(templates::TEMPLATES_FILE)
}

/// 读模板集（小文件，按需读，不做缓存——避免"UI 里那份"和"盘上那份"两套真相）。
fn read_templates() -> Result<templates::TemplateSet, String> {
    templates::load(&templates_path())
}

/// 把当前界面的输入打包成一份模板（名字为空直接拒绝，别存出无名模板）。
fn template_from_inputs(
    name: &str,
    model: &str,
    voice_ref: Option<String>,
    voice_ref_text: Option<String>,
    speed: f32,
    gap_ms: u64,
    auto_normalize: bool,
) -> Result<templates::DubTemplate, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("先给模板起个名字（存为旁边的输入框）".into());
    }
    Ok(templates::DubTemplate {
        name: name.to_string(),
        model: model.to_string(),
        voice_ref,
        // 模板必须自带参考文本：只存路径的话，应用回来的克隆音色缺文本、跑不起来
        voice_ref_text,
        speed,
        gap_ms,
        auto_normalize,
    })
}

/// 当前工程的输入（用来和模板比对，算出"应用后要作废什么"）。
fn project_inputs_from_ui(ui: &MainWindow) -> templates::ProjectInputs {
    let model = current_model_name(ui).unwrap_or_default();
    let (voice_ref, voice_ref_text) = voice_input_from_ui(ui);
    templates::ProjectInputs {
        model,
        voice_ref_text,
        voice_ref,
        gap_ms: gap_ms_from_ui(ui),
        auto_normalize: ui.get_auto_normalize(),
    }
}

/// 当前选中的引擎名（下拉索引 → 名字）。索引非法时 None。
fn current_model_name(ui: &MainWindow) -> Option<String> {
    let idx = ui.get_voice_index();
    (idx >= 0)
        .then(|| ui.get_voice_names().row_data(idx as usize))
        .flatten()
        .map(|n| n.to_string())
}

/// 模板下拉的选项刷新（名字来自磁盘；当前选中项尽量保留）。
fn refresh_template_names(ui: &MainWindow, keep: Option<&str>) {
    let Ok(set) = read_templates() else {
        ui.set_template_names(ModelRc::from(Rc::new(VecModel::from(Vec::new()))));
        ui.set_template_index(-1);
        return;
    };
    let names: Vec<SharedString> = set.names().into_iter().map(Into::into).collect();
    let pick = keep
        .and_then(|k| names.iter().position(|n| n.eq_ignore_ascii_case(k)))
        .unwrap_or(0);
    let idx = if names.is_empty() { -1 } else { pick as i32 };
    ui.set_template_names(ModelRc::from(Rc::new(VecModel::from(names))));
    ui.set_template_index(idx);
}

fn load_settings() -> AppSettings {
    load_settings_at(&settings_path())
}

/// 从指定路径读设置（`load_settings` 的唯一实现；路径可注入才测得了"落盘后能读回"）。
fn load_settings_at(path: &Path) -> AppSettings {
    let Some(raw) = std::fs::read_to_string(path).ok() else {
        return AppSettings::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return AppSettings::default();
    };
    AppSettings {
        host: json_field(&v, "host"),
        port: json_field(&v, "port"),
        model_dir: json_field(&v, "model_dir"),
        dictionary: json_field(&v, "dictionary"),
        bgm: json_field(&v, "bgm").unwrap_or_default(),
        update_url: json_field(&v, "update_url"),
        asr_model: json_field(&v, "asr_model"),
        design_model: json_field(&v, "design_model"),
        download_mirror: json_field(&v, "download_mirror"),
        download_concurrency: json_field(&v, "download_concurrency"),
    }
}

/// 取一个字段；缺了或类型不对都返回 None（交给该字段自己的默认值）。
fn json_field<T: serde::de::DeserializeOwned>(v: &serde_json::Value, key: &str) -> Option<T> {
    v.get(key)
        .and_then(|x| serde_json::from_value(x.clone()).ok())
}

fn save_settings(s: &AppSettings) -> std::io::Result<()> {
    save_settings_at(&settings_path(), s)
}

/// 写到指定路径（`save_settings` 的唯一实现；路径可注入，理由同上）。
fn save_settings_at(path: &Path, s: &AppSettings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let raw = serde_json::to_string_pretty(s).unwrap_or_else(|_| "{}".into());
    std::fs::write(path, raw)
}

static SETTINGS: std::sync::OnceLock<std::sync::Mutex<AppSettings>> = std::sync::OnceLock::new();

fn settings() -> &'static std::sync::Mutex<AppSettings> {
    SETTINGS.get_or_init(|| std::sync::Mutex::new(load_settings()))
}

fn settings_snapshot() -> AppSettings {
    settings().lock().map(|g| g.clone()).unwrap_or_default()
}

fn default_config_path() -> PathBuf {
    // 用 home_dir()（内部走 dirs）：Windows 上 HOME 常未设置，env 版本会退化成相对路径
    home_dir().join(".local/opt/audio.cpp/server.json")
}

/// 模型清单路径：AW_SERVER_CONFIG > 已存在的候选 > HOME 旧路径（兜底）。
///
/// 候选顺序（吸收 M3 的跨平台发现，且与 `tools/platform_paths.py` **保持一致**）：
///   1. `~/.local/opt/audio.cpp/server.json`（历史路径，macOS/Linux 一直在用）
///   2. `$XDG_CONFIG_HOME/audio.cpp/server.json`（Windows: `%APPDATA%\audio.cpp\...`）
///
/// 顺序刻意是"历史优先"：两个文件同时存在时，不改变老用户现有的读取目标。
///
/// 两者都不存在时返回第 1 条（错误信息里路径更符合老用户直觉）。
///
/// 说明：清单路径只在环境变量里可覆盖，UI 上不再暴露"模型清单文件"
/// （用户口径：全局设置里是**模型目录**，不是清单文件）。
fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var("AW_SERVER_CONFIG") {
        return PathBuf::from(path);
    }
    let legacy = default_config_path();
    let mut candidates = vec![legacy.clone()];
    if let Some(config) = dirs::config_dir() {
        candidates.push(config.join("audio.cpp/server.json"));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or(legacy)
}

/// **生效**的服务清单（所有消费者读这个；解析入口只有 `parse_server_config`）。
struct ServerConfig {
    host: Option<String>,
    port: Option<u16>,
    /// 服务内存守卫要求的余量（MiB）；0 = 服务侧关掉了守卫。
    /// 缺省时按 `model_sources::DEFAULT_HEADROOM_BYTES`（schema 的默认 1024 MiB）。
    min_free_memory_mb: Option<u32>,
    models: Vec<ServerModel>,
}

/// 解析 server.json 的**唯一入口**：原始形状 → 逐字段回落随包能力清单 → 生效形状。
///
/// 这里也是"哪个来源说了算"的**唯一**答案（合并逻辑在 `model_capabilities::resolve`）；
/// 调用方只做无条件赋值，别再写第二份判断
/// （见 `LESSON_同一语义两处实现必然漂移`）。
fn parse_server_config(raw: &str) -> Result<ServerConfig, String> {
    let doc: RawServerConfig =
        serde_json::from_str(raw).map_err(|e| format!("server.json 解析失败: {e}"))?;
    // 随包清单是我们自己的产物：读不出来要如实报，不能静默当成"没有兜底"
    // ——那正是本批要消灭的失败形态（能力提示悄悄消失）。
    let cat = model_capabilities::catalog()
        .map_err(|e| format!("随包能力清单（config/model-capabilities.json）读不出来：{e}"))?;
    Ok(ServerConfig {
        host: doc.host,
        port: doc.port,
        min_free_memory_mb: doc.min_free_memory_mb,
        models: doc
            .models
            .into_iter()
            .map(|m| {
                let caps = model_capabilities::resolve(&m.caps, cat.find(&m.id));
                ServerModel {
                    id: m.id,
                    task: m.task,
                    family: m.family,
                    path: m.path,
                    url: m.url,
                    sha256: m.sha256,
                    size: m.size,
                    caps,
                }
            })
            .collect(),
    })
}

/// server.json 一条记录的**原始**形状（serde 直接解析它）。
///
/// 能力字段走 `model_capabilities::RawCaps`（全是 `Option`）：只有原文能区分
/// "服务端写了 `false`"与"服务端没写这一项"，而回落的判据恰恰是这个。
/// serde 的 `default` 会把两者抹成同一个值，所以**不能**从生效值反推。
#[derive(serde::Deserialize, Default)]
struct RawServerModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    family: String,
    #[serde(default)]
    path: String,
    /// 上游下载地址（P7）：有它才算"可下载模型"，没有就不显示下载入口。
    #[serde(default)]
    url: String,
    /// 期望 sha256（可选）：给了就下载后必校验，不匹配直接丢弃。
    #[serde(default)]
    sha256: String,
    /// 期望字节数（可选）：没有 sha256 时按它核大小。
    #[serde(default)]
    size: Option<u64>,
    #[serde(flatten)]
    caps: model_capabilities::RawCaps,
}

/// server.json 的**原始**形状（`#[serde(flatten)]` 之上再包一层）。
#[derive(serde::Deserialize, Default)]
struct RawServerConfig {
    host: Option<String>,
    port: Option<u16>,
    #[serde(default)]
    min_free_memory_mb: Option<u32>,
    #[serde(default)]
    models: Vec<RawServerModel>,
}

/// 一条**生效**的模型记录（下游消费者读的都是它）。
#[derive(Default)]
struct ServerModel {
    id: String,
    task: String,
    family: String,
    path: String,
    /// 上游下载地址（P7）：有它才算"可下载模型"，没有就不显示下载入口。
    url: String,
    /// 期望 sha256（可选）：给了就下载后必校验，不匹配直接丢弃。
    sha256: String,
    /// 期望字节数（可选）：没有 sha256 时按它核大小。
    size: Option<u64>,
    /// **生效**能力（服务端显式 → 随包清单兜底）。不是 serde 直接填的：
    /// 由 `parse_server_config` 走 `model_capabilities::resolve` 算出来。
    ///
    /// - `product_excluded`：产品层已排除；真机反例 `audio8-tts-01b` 选中后 HTTP 200
    ///   却产出听不懂的音频（可懂度 0~3%、时长乱跳），全程无报错 —— 最坏的失败形态。
    /// - `mode` / `role`：`offline` / `streaming` / `scoring` / `fast-asr` …
    /// - `requires`：后端硬要求（如 index-tts2 的 `voice_ref: true`）。
    /// - `known_issues`：在选择处**只读**展示，不参与自动决策。
    caps: model_capabilities::Capability,
}

/// 配音可选引擎的**唯一判据**：tts 任务 + 没被产品层排除 + 不是流式专用。
///
/// 下拉列表、默认引擎、能力判定都必须走它 —— 别再写第二份过滤
/// （见 `LESSON_同一语义两处实现必然漂移`：同一语义两份实现必然漂移）。
fn is_selectable_tts_engine(m: &ServerModel) -> bool {
    m.task == "tts" && !m.caps.product_excluded && !m.caps.is_streaming_only()
}

/// 清单里的可选 TTS 引擎 → 音色下拉行（配音「高级 → 引擎」消费它）。
///
/// 抽成吃 `&[ServerModel]` 的纯函数：测试可以直接喂合成清单，不必读本机 server.json
/// （进程里改环境变量是多线程 UB，见 `LESSON_多线程测试中set_var修改进程环境是UB`）。
fn tts_engine_voices(models: &[ServerModel]) -> Vec<Voice> {
    models
        .iter()
        .filter(|m| is_selectable_tts_engine(m))
        .map(|m| Voice {
            name: m.id.clone().into(),
            engine: format!("{} · 本地", m.family).into(),
            note: short_path(&m.path).into(),
            license: "仅自用".into(),
            requires_voice_ref: m.caps.requires_voice_ref(),
            requires_reference_text: m.caps.requires_reference_text(),
            known_issues: m.caps.known_issues_note().into(),
        })
        .collect()
}

/// 默认引擎：**按能力**挑第一个"不需要参考音频"的可选引擎（开箱可用），
/// 全都要参考音时退回第一个。**不写死 id** —— 清单里没有 `audio8-tts` 的机器
/// （例如只有 index-tts2）也能选到可用 TTS，而不是显示"没有可用音色"。
fn default_engine_index(voices: &[Voice]) -> i32 {
    voices
        .iter()
        .position(|v| !v.requires_voice_ref)
        .or_else(|| (!voices.is_empty()).then_some(0))
        .map(|i| i as i32)
        .unwrap_or(-1)
}

/// 读取 server.json：音色 = **可选**的 tts 引擎（判据只有一处，见
/// `is_selectable_tts_engine`：`product_excluded` 与 streaming-only 不算）；
/// 地址取 AW_SERVER，否则 host:port。
/// 文件缺失/解析失败返回空清单 + 原因说明（不 panic：服务没配时界面也可打开）。
fn discover_engine() -> (Vec<Voice>, Option<String>, String) {
    let cfg_path = config_path();
    let over = settings_snapshot();
    let env_base = std::env::var("AW_SERVER").ok();
    // **只解析一次**：地址回显与引擎列表共用同一份结果（解析两遍就是两份判据，
    // 见 `LESSON_同一语义两处实现必然漂移`）。解析失败/文件缺失都退化成"没有清单"。
    let (cfg, note) = match std::fs::read_to_string(&cfg_path) {
        Ok(raw) => match parse_server_config(&raw) {
            Ok(c) => (Some(c), String::new()),
            Err(e) => (None, e),
        },
        Err(_) => (None, format!("没找到 {}", cfg_path.display())),
    };
    // 与界面回显共用同一个解析入口（见 resolve_base 的注释）
    let (base_url, _) = resolve_base(&over, &cfg, env_base.as_deref());
    let base = has_endpoint_source(&over, &cfg, env_base.as_deref()).then_some(base_url);
    let voices = cfg
        .map(|c| tts_engine_voices(&c.models))
        .unwrap_or_default();
    (voices, base, note)
}

/// 清单里的模型总数（读不到清单就是 0，不算错误）。
fn config_summary() -> usize {
    read_server_config().map(|c| c.models.len()).unwrap_or(0)
}

/// 读模型清单（读不到就是"没有清单"）。
///
/// 解析走唯一入口 `parse_server_config`：所以服务没声明的能力字段，在这里也已经
/// 回落过随包能力清单（不是"只有 discover_engine 才回落"）。
fn read_server_config() -> Option<ServerConfig> {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|raw| parse_server_config(&raw).ok())
}

// ===========================================================================
// 质检回读（ASR）模型：候选来自服务清单，选择落 settings.json
//
// 本机 16 GiB 时 `qwen3-asr`（估 3.31 GiB + 1 GiB 余量）装不下，服务直接 503，
// 而应用把回读模型写死成它 —— 质检整条功能不可用。这里让它可选，并且**动态**列
// 清单里 `task == "asr"` 的模型（不硬编名字：清单加一个就多一项）。
// ===========================================================================

/// 一个清单条目能否作为 App 的音色设计引擎。
///
/// 不只是 `task == "vdes"`：audio.cpp 已确认支持 VoiceDesign 的 family 是
/// `qwen3_tts` 与 `breeze_tts`；流式/产品排除项不能走离线一次性生成。
/// 把“取第一个”收成显式白名单，避免清单顺序变化时静默选到一个服务端会拒的模型。
fn is_voice_design_model(m: &ServerModel) -> bool {
    m.task == "vdes"
        && !m.id.trim().is_empty()
        && !m.caps.product_excluded
        && m.caps.mode.eq_ignore_ascii_case("offline")
        && matches!(m.family.as_str(), "qwen3_tts" | "breeze_tts")
}

/// 清单里所有可用的**音色设计**模型 id（保持清单顺序，跳过空 id）。
///
/// 候选只来自服务清单，不硬编具体模型：Breeze 2、Qwen3 或以后新增的已知 family
/// 都会自动出现。`task=vdes` 是引擎硬要求，`mode=offline` 是 VoiceDesign 当前只有
/// 离线通道，`product_excluded` 保持产品层的排除权。
fn design_models_from(cfg: Option<&ServerConfig>) -> Vec<String> {
    cfg.map(|c| {
        c.models
            .iter()
            .filter(|m| is_voice_design_model(m))
            .map(|m| m.id.clone())
            .collect()
    })
    .unwrap_or_default()
}

/// 下拉里给人看的模型名。未知 id 原样显示，不猜厂商/大小。
fn design_model_label(id: &str) -> String {
    match id {
        "breeze-tts-voicedesign" => "BreezeTTS 2 · 音色设计".to_string(),
        "qwen3-tts-voicedesign" => "Qwen3-TTS 1.7B · 音色设计".to_string(),
        _ => id.to_string(),
    }
}

/// 当前生效的设计模型：设置 > 清单第一个。
///
/// 用户选的 id 不在当前清单时也**照用不改**（与 ASR 同一条约定）：VoiceDesign
/// 模型质量/速度差异明显，静默换模型会改变用户听到的结果；服务拒绝就如实报错。
fn effective_design_model_from(s: &AppSettings, models: &[String]) -> Option<String> {
    s.design_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| models.first().cloned())
}

/// 从当前清单读出的生效设计模型。
fn effective_design_model(s: &AppSettings) -> Option<String> {
    effective_design_model_from(s, &design_models_from(read_server_config().as_ref()))
}

/// 下拉要显示什么 + 每个下标对应哪个 id + 当前选中下标。
///
/// 与 ASR 下拉同一条约定：显示、id、选中下标必须来自同一份推导；当前模型不在
/// 清单里时插到第 0 项并标注，避免清单变化时静默改掉用户的生效值。
fn design_picker_view(models: &[String], current: Option<&str>) -> (Vec<String>, Vec<String>, i32) {
    let mut ids = models.to_vec();
    let current = current.map(str::trim).filter(|m| !m.is_empty());
    if let Some(cur) = current {
        if !ids.iter().any(|m| m == cur) {
            ids.insert(0, cur.to_string());
        }
    }
    let labels = ids
        .iter()
        .map(|id| {
            let label = design_model_label(id);
            if models.iter().any(|m| m == id) {
                label
            } else {
                format!("{label}（不在当前清单）")
            }
        })
        .collect();
    let index = current
        .and_then(|cur| ids.iter().position(|m| m == cur))
        .map(|i| i as i32)
        .unwrap_or(-1);
    (ids, labels, index)
}

/// 设计模型下拉的常驻说明：候选来源/当前值是否还在清单里。
fn design_model_note(models: &[String], current: Option<&str>) -> String {
    if models.is_empty() {
        return "没读到服务清单里 task=vdes 的设计模型：检查 server.json 与 audiocpp_server"
            .to_string();
    }
    let mut note = format!("候选来自服务清单（{} 个设计模型）", models.len());
    if let Some(cur) = current {
        if !models.iter().any(|m| m == cur) {
            note.push_str(&format!("·当前选的 {cur} 不在清单里，服务可能加载不了"));
        }
    }
    note
}

/// 清单里所有 `task == "asr"` 的模型 id（保持清单顺序，跳过空 id）。
fn asr_models_from(cfg: Option<&ServerConfig>) -> Vec<String> {
    cfg.map(|c| {
        c.models
            .iter()
            .filter(|m| m.task == "asr" && !m.id.trim().is_empty())
            .map(|m| m.id.clone())
            .collect()
    })
    .unwrap_or_default()
}

/// 本机清单里的 ASR 模型（读不到清单 = 空列表，不算错误）。
fn asr_models() -> Vec<String> {
    asr_models_from(read_server_config().as_ref())
}

/// 当前生效的质检回读模型：设置 > 默认。**这是唯一入口**。
///
/// 用户选的 id 若已不在当前清单里也**照用不改**：静默换成别的模型会改变质检口径
/// （词级/说话人能力都不同），换模型只能由用户点。服务拒绝就如实报错。
fn effective_asr_model(s: &AppSettings) -> String {
    s.asr_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(aw_core::DEFAULT_ASR_MODEL)
        .to_string()
}

/// 下拉要显示什么 + 每个下标对应哪个 id + 当前选中下标。
///
/// 三样一起返回，是因为它们必须来自**同一份推导**：分别算就会漂移成"显示的是 A、
/// 选出来的却是 B"。当前模型不在清单里时插到第 0 项并标注，保证下拉永远不会
/// 因为清单变化而静默改掉用户的生效值。
fn asr_picker_view(models: &[String], current: &str) -> (Vec<String>, Vec<String>, i32) {
    let mut ids: Vec<String> = models.to_vec();
    let mut labels: Vec<String> = models.to_vec();
    if !ids.iter().any(|m| m == current) {
        ids.insert(0, current.to_string());
        labels.insert(0, format!("{current}（不在当前清单）"));
    }
    let index = ids.iter().position(|m| m == current).unwrap_or(0) as i32;
    (ids, labels, index)
}

/// 一个 ASR 候选在磁盘上的权重文件大小（读不到 = None，不猜）。
fn asr_candidate_weights(cfg: Option<&ServerConfig>) -> Vec<(String, Option<u64>)> {
    cfg.map(|c| {
        c.models
            .iter()
            .filter(|m| m.task == "asr" && !m.id.trim().is_empty())
            .map(|m| (m.id.clone(), file_size_of(&m.path)))
            .collect()
    })
    .unwrap_or_default()
}

/// 单个路径的字节数：不存在 / 是目录 / 读不了都返回 None。
fn file_size_of(path: &str) -> Option<u64> {
    if path.trim().is_empty() {
        return None;
    }
    let md = std::fs::metadata(path).ok()?;
    md.is_file().then_some(md.len())
}

/// MiB → 人读（≥1024 MiB 用 GiB）。
fn humans_mib(mib: u64) -> String {
    if mib >= 1024 {
        format!("{:.2} GiB", mib as f64 / 1024.0)
    } else {
        format!("{mib} MiB")
    }
}

/// 比当前模型更小的 ASR 候选，按磁盘权重升序，最多 3 个。
///
/// 当前模型的权重读不到时退化成"清单里其它 ASR 模型"——宁可不排序，也不假装知道谁更小。
/// 数字标的是**权重文件**大小，不是服务的内存估算，文案里说清楚这一点。
fn smaller_asr_candidates(current: &str, candidates: &[(String, Option<u64>)]) -> Vec<String> {
    let cur_size = candidates
        .iter()
        .find(|(id, _)| id == current)
        .and_then(|(_, size)| *size);
    let mut rest: Vec<&(String, Option<u64>)> =
        candidates.iter().filter(|(id, _)| id != current).collect();
    if let Some(cur) = cur_size {
        rest.retain(|(_, size)| matches!(size, Some(n) if *n < cur));
    }
    // 知道的在前（按大小升序），不知道的排后面：不排序 = 不假装知道
    rest.sort_by_key(|(_, size)| size.map(|n| (0u8, n)).unwrap_or((1, 0)));
    rest.into_iter()
        .take(3)
        .map(|(id, size)| match size {
            Some(n) => format!("{id}（权重约 {}）", backup::human_bytes(*n)),
            None => id.clone(),
        })
        .collect()
}

/// 内存不足时的**可执行**提示：哪个模型装不下（含服务给的数字）、当时可用多少、
/// 本机还有哪些更小的可选。只说"失败"等于把用户扔在原地。
fn memory_shortfall_hint(
    model: &str,
    mem: Option<&aw_core::InsufficientMemory>,
    candidates: &[(String, Option<u64>)],
) -> String {
    let mut out = match mem {
        Some(m) => {
            // 服务报的是它眼里的模型名；与请求名不一致时两个都写出来
            let name = match m.model.as_deref() {
                Some(n) if n != model => format!("{n}（请求的是 {model}）"),
                _ => model.to_string(),
            };
            let need = match (m.required_mib(), m.estimated_mib, m.headroom_mib) {
                (Some(req), Some(est), Some(head)) => format!(
                    "需要约 {}（模型 {} + 余量 {}）",
                    humans_mib(req),
                    humans_mib(est),
                    humans_mib(head)
                ),
                _ => "服务没给出可解析的占用估算".to_string(),
            };
            let avail = match m.available_mib {
                Some(a) => format!("，当时可用 {}", humans_mib(a)),
                None => String::new(),
            };
            format!("质检未开始：回读模型 {name} 装不下：{need}{avail}。")
        }
        None => format!("质检未开始：回读模型 {model} 装不下（服务因内存不足拒绝加载）。"),
    };
    let smaller = smaller_asr_candidates(model, candidates);
    if smaller.is_empty() {
        out.push_str(
            "本机清单里没有更小的 ASR 模型可选：先腾出内存，或给清单加一个更小的 ASR 模型。",
        );
    } else {
        out.push_str(&format!(
            "本机更小的 ASR 模型可选：{}——在「高级 → 质检回读模型」里改选后重跑。",
            smaller.join("、")
        ));
    }
    // 换 ASR 会改变质检口径，所以只提示、不代劳
    out.push_str(
        "换回读模型会改变质检口径（audio8-asr / fun-asr 没有词级时间戳与说话人分离），需你确认，本应用不会自动换。",
    );
    out
}

/// 单句 ASR 失败之后该怎么办。
#[derive(Debug, PartialEq, Eq)]
enum EvalAsrFailure {
    /// 整轮都不可能成功（内存不足）：带着可执行提示立刻收尾
    Fatal(String),
    /// 只是这一句没测到：记数、继续下一句
    Counted,
}

/// **唯一判据**：worker 按它决定"收尾"还是"继续"。
///
/// 抽出来是为了能直接喂一个真实的 503 body 做用例——真去起服务/等退避才判得出来的话，
/// 这条行为就没有能红的回归（也不该为了测试去 set_var 改进程环境）。
fn classify_asr_failure(
    model: &str,
    err: &aw_core::ClientError,
    candidates: &[(String, Option<u64>)],
) -> EvalAsrFailure {
    match err.insufficient_memory() {
        Some(mem) => EvalAsrFailure::Fatal(memory_shortfall_hint(model, Some(&mem), candidates)),
        None => EvalAsrFailure::Counted,
    }
}

/// 回读下拉右边那句说明。只描述"候选从哪来 / 当前选的还在不在 / 换它会变什么"，
/// 不列举任何写死的模型名（候选是清单给的，写死就又多一份真相）。
fn asr_model_note(models: &[String], current: &str, in_manifest: bool) -> String {
    let mut note = if models.is_empty() {
        "没读到服务清单里 task=asr 的模型：检查 server.json 与 audiocpp_server".to_string()
    } else {
        format!("候选来自服务清单（{} 个 ASR 模型）", models.len())
    };
    if !in_manifest {
        note.push_str(&format!("·当前选的 {current} 不在清单里，服务可能加载不了"));
    }
    note.push_str("·换模型会改变质检口径（audio8-asr / fun-asr 无词级时间戳与说话人分离）");
    note
}

/// 把选中的回读模型落进 settings.json（失败也不阻断，只提示）。
///
/// 返回真正写下去的值，便于调用方用它回显——回显若另算一份就会与落盘值漂移。
fn persist_asr_model(model: &str) -> Result<String, String> {
    persist_asr_model_at(settings(), &settings_path(), model)
}

/// `persist_asr_model` 的唯一实现。settings 与路径都可注入：单测不得写用户真实的
/// settings.json（`LESSON_单测不得写用户真实运行数据须拆出注入缝`）。
fn persist_asr_model_at(
    store: &std::sync::Mutex<AppSettings>,
    path: &Path,
    model: &str,
) -> Result<String, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("回读模型不能为空".into());
    }
    let snapshot = {
        let mut guard = store.lock().map_err(|_| "设置锁不可用".to_string())?;
        guard.asr_model = Some(model.to_string());
        guard.clone()
    };
    // 先改内存再落盘是既有约定（失败时至少本次会话生效）；落盘失败要如实回报
    save_settings_at(path, &snapshot).map_err(|e| e.to_string())?;
    Ok(model.to_string())
}

/// 把回读下拉刷成"清单 + 当前生效值"的投影（候选、选中项、说明来自同一份推导）。
fn refresh_asr_models(ui: &MainWindow) {
    let models = asr_models();
    let current = effective_asr_model(&settings_snapshot());
    let (_, labels, index) = asr_picker_view(&models, &current);
    let in_manifest = models.contains(&current);
    let labels: Vec<SharedString> = labels.into_iter().map(SharedString::from).collect();
    ui.set_asr_model_names(ModelRc::from(Rc::new(VecModel::from(labels))));
    ui.set_asr_model_index(index);
    ui.set_asr_model_note(asr_model_note(&models, &current, in_manifest).into());
}

/// 质检回读模型的下拉：候选来自服务清单，切换落 settings.json。
///
/// 归属：回读模型只影响配音页的质检，所以放在本页「高级」里，不进全局抽屉
/// （`LESSON_全局容器只放全局项单Tab独有的放本页`）。
fn wire_asr_model(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, state: &Rc<UiState>) {
    let weak = ui.as_weak();
    let st = state.clone();
    let rw = rows.clone();
    ui.on_asr_model_picked(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        // 质检在跑时不让换：worker 手里那条命令已经带着当时的模型名，
        // 中途改设置只会造成"界面显示 A、这次实际用 B"。
        if project_editing_blocked(&ui, &st) || batch_in_flight(&st) {
            ui.set_status_text("任务进行中：等这轮跑完再换回读模型".into());
            refresh_asr_models(&ui);
            return;
        }
        let current = effective_asr_model(&settings_snapshot());
        // 与刷新时同一份推导：下拉显示的第 i 项就是这里取出的第 i 项
        let (ids, _, _) = asr_picker_view(&asr_models(), &current);
        let picked = ids.get(i.max(0) as usize).cloned();
        if let Some(id) = picked.filter(|id| *id != current) {
            match persist_asr_model(&id) {
                Ok(saved) => {
                    // 如实说清盘上那些分是**谁**测的：来源取自分数自己的标记，
                    // 与常驻说明同一句推导（`qa_source_note`）——以前那句"可能是上一个
                    // 模型测的"没有数据支撑，说不出是哪个模型。仍不擅自清分。
                    let sources = ledger_score_sources(&st);
                    let note = if sources.scored == 0 {
                        format!("回读模型已改为 {saved}（下次质检生效）")
                    } else {
                        format!(
                            "回读模型已改为 {saved}。{}",
                            qa_source_note(&sources, &saved)
                        )
                    };
                    ui.set_status_text(note.into());
                    // 常驻说明要跟着当前模型刷新（它按"当前模型 vs 分数来源"算）
                    sync_qa_actions(&ui, &rw, &st);
                }
                Err(e) => ui.set_status_text(
                    format!("回读模型没能保存（{e}）：重启后会回到上次的选择").into(),
                ),
            }
        }
        refresh_asr_models(&ui);
    });
}

/// 把选中的音色设计模型落进 settings.json（失败也不阻断，只提示）。
fn persist_design_model(model: &str) -> Result<String, String> {
    persist_design_model_at(settings(), &settings_path(), model)
}

/// `persist_design_model` 的唯一实现（注入缝，单测不写真实 settings.json）。
fn persist_design_model_at(
    store: &std::sync::Mutex<AppSettings>,
    path: &Path,
    model: &str,
) -> Result<String, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("设计模型不能为空".into());
    }
    let snapshot = {
        let mut guard = store.lock().map_err(|_| "设置锁不可用".to_string())?;
        guard.design_model = Some(model.to_string());
        guard.clone()
    };
    save_settings_at(path, &snapshot).map_err(|e| e.to_string())?;
    Ok(model.to_string())
}

/// 把设计模型下拉刷成"清单 + 当前生效值"的投影（候选、标签、选中项同源）。
fn refresh_design_models(ui: &MainWindow) {
    let models = design_models_from(read_server_config().as_ref());
    let current = effective_design_model_from(&settings_snapshot(), &models);
    let (_, labels, index) = design_picker_view(&models, current.as_deref());
    let labels: Vec<SharedString> = labels.into_iter().map(SharedString::from).collect();
    ui.set_design_model_names(ModelRc::from(Rc::new(VecModel::from(labels))));
    ui.set_design_model_index(index);
    ui.set_design_model_note(design_model_note(&models, current.as_deref()).into());
}

/// 音色设计模型下拉：候选来自服务清单，切换落 settings.json。
fn wire_design_model(ui: &MainWindow, state: &Rc<UiState>) {
    let weak = ui.as_weak();
    let st = state.clone();
    ui.on_design_model_picked(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_design_busy() || ui.get_running() || ui.get_busy() || tasks_in_flight(&st) {
            ui.set_design_status("任务进行中：等这轮跑完再换设计模型".into());
            refresh_design_models(&ui);
            return;
        }
        let models = design_models_from(read_server_config().as_ref());
        let current = effective_design_model_from(&settings_snapshot(), &models);
        let (ids, _, _) = design_picker_view(&models, current.as_deref());
        let picked = ids.get(i.max(0) as usize).cloned();
        if let Some(id) = picked.filter(|id| Some(id.as_str()) != current.as_deref()) {
            match persist_design_model(&id) {
                Ok(saved) => ui.set_design_status(format!("设计模型已改为 {saved}").into()),
                Err(e) => ui.set_design_status(
                    format!("设计模型没能保存（{e}）：重启后会回到上次的选择").into(),
                ),
            }
        }
        refresh_design_models(&ui);
    });
}

/// 服务地址解析（纯函数，便于单测三档优先级）。
///
/// host / port **各自独立回落**：只改地址不让端口覆盖失效——早期版本要求
/// `(Some, Some)` 成对，结果是"只改端口"这类单边修改被静默忽略并回滚成清单值。
/// `env_base`（AW_SERVER）是整串临时覆盖，优先级最高，不做字段合并。
fn resolve_endpoint(
    over_host: Option<&str>,
    over_port: Option<u16>,
    cfg_host: Option<&str>,
    cfg_port: Option<u16>,
    env_base: Option<&str>,
) -> (String, String, bool) {
    if let Some(base) = env_base {
        return (base.to_string(), String::new(), true);
    }
    let host = over_host
        .filter(|h| !h.trim().is_empty())
        .or(cfg_host)
        .unwrap_or("127.0.0.1")
        .to_string();
    let port = over_port.or(cfg_port).unwrap_or(8080);
    (host, port.to_string(), false)
}

/// 清单里的 host / port 原样取出（可能缺项，交给 resolve_endpoint 回落）。
fn split_base(cfg: &Option<ServerConfig>) -> (Option<String>, Option<u16>) {
    match cfg {
        Some(c) => (c.host.clone(), c.port),
        None => (None, None),
    }
}

/// **唯一的服务地址解析入口**：真正连服务的 `discover_engine` 与界面回显 `server_endpoint`
/// 都必须走它。审查抓到过一次事故——回显改成了独立回落、客户端仍走"host/port 成对"的旧逻辑，
/// 结果同一屏里抽屉写着 10.9.9.9、状态栏却在连 127.0.0.1。两份实现必然漂移，所以只留一份。
///
/// 返回 `(base_or_host, from_env)`：AW_SERVER 在时是第一项就是完整 URL（不做字段合并）。
fn resolve_base(
    over: &AppSettings,
    cfg: &Option<ServerConfig>,
    env_base: Option<&str>,
) -> (String, bool) {
    let (cfg_host, cfg_port) = split_base(cfg);
    let (host, port, from_env) = resolve_endpoint(
        over.host.as_deref(),
        over.port,
        cfg_host.as_deref(),
        cfg_port,
        env_base,
    );
    if from_env {
        (host, true)
    } else {
        (format!("http://{host}:{port}"), false)
    }
}

/// 有没有任何地址来源（环境变量 / 全局设置 / 清单）。都没有才认为"未发现服务"。
fn has_endpoint_source(
    over: &AppSettings,
    cfg: &Option<ServerConfig>,
    env_base: Option<&str>,
) -> bool {
    env_base.is_some() || cfg.is_some() || over.host.is_some() || over.port.is_some()
}

/// 服务地址回显：AW_SERVER > 全局设置 > 清单；第三个返回值表示"环境变量在生效"。
fn server_endpoint() -> (String, String, bool) {
    let over = settings_snapshot();
    let cfg = read_server_config();
    let env_base = std::env::var("AW_SERVER").ok();
    let (base, from_env) = resolve_base(&over, &cfg, env_base.as_deref());
    if from_env {
        return (base, String::new(), true);
    }
    let (cfg_host, cfg_port) = split_base(&cfg);
    let (host, port, _) = resolve_endpoint(
        over.host.as_deref(),
        over.port,
        cfg_host.as_deref(),
        cfg_port,
        None,
    );
    (host, port, false)
}

/// 供随包引擎启动用：返回 (base, 地址是否来自**用户显式配置**)。
///
/// "显式"= `AW_SERVER` 或全局设置里写了 host/port。只有非显式（也就是地址纯粹来自
/// 清单默认值或内置默认）时，才允许壳去拉起随包引擎 —— 用户显式配了地址就是明确
/// 表示"我自己有服务/我连别人的"，那时壳不该自作主张再起一个。
fn server_base_for_engine() -> (String, bool) {
    let over = settings_snapshot();
    let cfg = read_server_config();
    let env_base = std::env::var("AW_SERVER").ok();
    let (base, from_env) = resolve_base(&over, &cfg, env_base.as_deref());
    let explicit = from_env || over.host.is_some() || over.port.is_some();
    (base, explicit)
}

/// 随包引擎的可写数据目录（`server.json` / 日志）。**绝不落安装目录**。
fn engine_data_dir() -> PathBuf {
    documents_dir().join(WORKSHOP_DIR).join("engine")
}

/// 按需拉起随包引擎。**启动时**与**模型下载成功后**共用这一条路径 —— 引擎没有模型
/// 就拒绝启动（`config.cpp:275`），所以"刚下完第一个模型"正是它第一次能起来的时候。
///
/// 已经起过（句柄还在）就直接返回，不重复拉起；外部服务优先的判据在 `ensure_serving` 里。
fn ensure_engine_serving() -> engine_supervisor::StartOutcome {
    // 手动路径（启动、下载完成后）不受节流：那是用户明确动作之后的一次尝试。
    // 崩溃自愈的节流在 `ensure_engine_serving_throttled` 里。
    // 句柄检查/清理走单一取锁的 helper：内联写法（在 if-let 里对锁守卫取 `as_mut`
    // 后又在块内二次 lock）的**临时守卫会活到 if-let 结束**，同线程重入会**死锁**；
    // 而且只在「引擎已崩溃」这条自愈关键分支触发（2026-09-22 独立复评实测），
    // 见 `engine_supervisor::take_live_supervisor` 的注释与回归测试。
    if engine_supervisor::take_live_supervisor(&ENGINE_SUPERVISOR) {
        return engine_supervisor::StartOutcome::Started;
    }
    let (base, explicit) = server_base_for_engine();
    let cat = match model_sources::catalog() {
        Ok(c) => c,
        Err(e) => return engine_supervisor::StartOutcome::Failed(format!("模型清单不可用：{e}")),
    };
    let mut sup = engine_supervisor::EngineSupervisor::new();
    let outcome = sup.ensure_serving(&base, explicit, &model_dir(), cat, &engine_data_dir());
    if matches!(outcome, engine_supervisor::StartOutcome::Started) {
        eprintln!("随包引擎已启动：{base}");
        *ENGINE_SUPERVISOR.lock().unwrap_or_else(|e| e.into_inner()) = Some(sup);
    }
    outcome
}

/// 崩溃自愈入口：健康检查发现连不上时调用。
///
/// 与手动路径（启动 / 下载完成后）的区别有两条：
///   · **先探活**：服务在响应就直接返回 `None` —— 周期检查每约 30s 会来一次，
///     不能让"引擎好好的"也去动状态行（不刷屏）；
///   · **带节流**：引擎一起来就崩的情况下，不能每轮健康检查都重启一次
///     （那会变成重启风暴）。
///
/// 返回 `Some(outcome)` = 本次真的做了一次拉起尝试；`None` = 健康在响应或节流挡住。
fn ensure_engine_serving_throttled() -> Option<engine_supervisor::StartOutcome> {
    let (base, _) = server_base_for_engine();
    if engine_supervisor::healthy(&base) {
        return None;
    }
    if !engine_supervisor::autostart_allowed(std::time::Instant::now()) {
        return None;
    }
    // 句柄还在但 /health 不响应 = 进程挂死：如实报 Failed，**不装成 Started**——
    // 否则状态栏会说"已重新拉起"而进程根本没动过（强杀再重拉不在本批范围，
    // 下一轮 30s 还会再问；"不刷屏"靠状态行结论去重兜着）。
    if let Some(sup) = ENGINE_SUPERVISOR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        if sup.is_running() {
            return Some(engine_supervisor::StartOutcome::Failed(
                "引擎进程还在但 /health 不响应（疑似挂死）：稍后会自动再试".into(),
            ));
        }
    }
    let outcome = ensure_engine_serving();
    if let engine_supervisor::StartOutcome::Failed(why) = &outcome {
        eprintln!("随包引擎自愈失败：{why}");
    }
    Some(outcome)
}

/// 重新读 /health 并刷新状态栏的后端标签（启动、测试连接、应用并重连后都调用）。
/// duck 强度档位 → duck_gain 系数。
///
/// 语义档位（弱/中/强）是给用户看的，dB 原值不进首屏（设计稿 §3.4）。
/// 0=弱（BGM 保持存在感，压得少）、1=中（默认，≈-13dB）、2=强（人声更突出）。
pub fn duck_gain_for(index: i32) -> f32 {
    match index {
        0 => 0.35,
        2 => 0.12,
        _ => 0.22,
    }
}

/// 独立生成 BGM 的时长档位（秒）：30 / 60 / 120 / 180，越界回落 60。
pub fn bgm_standalone_seconds(index: i32) -> f64 {
    match index {
        0 => 30.0,
        2 => 120.0,
        3 => 180.0,
        _ => DEFAULT_BGM_STANDALONE_SECONDS,
    }
}

/// 三轨结果区里的行号 → 产物路径（0 人声 / 1 BGM / 2 混音）。
///
/// 独立生成的 BGM 没有 voice / mixed 两轨 → 返回 None（UI 侧那一行不显示，
/// 而不是给一个不存在的路径让用户点了报错）。
/// BGM 结果列表的下标 → 分轨导出用的轨（两处映射必须一致：列表第 0 行是人声）。
fn stem_for_track(index: i32) -> export::Stem {
    match index {
        0 => export::Stem::Voice,
        1 => export::Stem::Bgm,
        _ => export::Stem::Mixed,
    }
}

pub fn bgm_track_path(artifacts: &BgmArtifacts, index: i32) -> Option<PathBuf> {
    match index {
        0 => artifacts.voice.clone(),
        1 => Some(artifacts.bgm.clone()),
        _ => artifacts.mixed.clone(),
    }
}

fn refresh_backend_label(ui: &MainWindow) {
    let (_, base, _) = discover_engine();
    ui.set_backend_label(backend_label(base.as_deref()).into());
}

/// 启动时把持久化的 BGM 输入灌回界面（描述/两个档位）。
fn apply_bgm_settings(ui: &MainWindow) {
    let bgm = settings_snapshot().bgm;
    ui.set_bgm_prompt(bgm.prompt.into());
    ui.set_bgm_duck_index(bgm.duck_index);
    ui.set_bgm_standalone_index(bgm.standalone_index);
}

/// 启动时把「更新」区画成初始态：当前版本（编译期常量，单一来源）+ 持久化的清单地址。
fn apply_update_settings(ui: &MainWindow) {
    ui.set_update_current(update::CURRENT_VERSION.into());
    ui.set_update_manifest_url(settings_snapshot().update_url.unwrap_or_default().into());
    ui.set_update_info("还没检查过（只读检查：不会自动下载或安装）".into());
}

/// 把「更新」区的清单地址写回 settings.json。
///
/// 触发点：点「检查更新」。内网/镜像地址只填一次，重启还在。
/// 与官方默认地址相同 / 空的都存成 `None`（回落默认），免得以后换默认地址时被旧值钉住。
/// 写失败只提示、不阻断——丢的是"下次的默认值"，不是这次检查。
fn save_update_url(url: &str) {
    let trimmed = url.trim();
    let want = if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(update::DEFAULT_MANIFEST_URL) {
        None
    } else {
        Some(trimmed.to_string())
    };
    let snapshot = {
        let Ok(mut guard) = settings().lock() else {
            return;
        };
        if guard.update_url == want {
            return; // 值没变别写盘
        }
        guard.update_url = want;
        guard.clone()
    };
    let _ = save_settings(&snapshot);
}

/// 把当前界面上的 BGM 输入写回 settings.json。
///
/// 触发点：描述编辑（用户可能没生成就退出）、点生成（档位改动没有回调，只能在这里收）。
/// 写失败只提示、不阻断——丢的是"下次的默认值"，不是这次的任务。
fn save_bgm_settings(ui: &MainWindow) {
    let want = BgmSettings {
        prompt: ui.get_bgm_prompt().to_string(),
        duck_index: ui.get_bgm_duck_index(),
        standalone_index: ui.get_bgm_standalone_index(),
    };
    let snapshot = {
        let mut guard = match settings().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if guard.bgm.prompt == want.prompt
            && guard.bgm.duck_index == want.duck_index
            && guard.bgm.standalone_index == want.standalone_index
        {
            return; // 值没变就别写盘（打字时每次回调都写一遍是浪费）
        }
        guard.bgm = want;
        guard.clone()
    };
    if let Err(e) = save_settings(&snapshot) {
        ui.set_bgm_status_text(
            format!("BGM 设置没能保存（{e}）：重启后描述会回到上次保存的值").into(),
        );
    }
}

/// 当前的 BGM 输入 → 导出层要的上下文（UI 认不认这套结果 + 参数摘要）。
///
/// 模式（混音 / 独立生成）看当前工程有没有配音成品——与 worker 的判定同源
/// （有配音成品就混音，没有就独立生成）。
fn bgm_context(ui: &MainWindow) -> export::BgmContext {
    export::BgmContext::new(
        bgm_result_exportable(ui.get_bgm_has_result(), ui.get_bgm_stale()),
        has_voice_product(ui),
        &ui.get_bgm_prompt(),
        duck_gain_for(ui.get_bgm_duck_index()),
        bgm_standalone_seconds(ui.get_bgm_standalone_index()),
    )
}

/// 配音成品的可用时长（秒）：**worker 与导出侧共用同一个判定**。
///
/// 有配音成品（能读出来、时长 > 0）→ BGM 走混音模式；否则独立生成。两边各写一份
/// "有没有配音成品"迟早漂移——复核抓到的反例就是：`out/final.wav` 存在但损坏时，
/// worker 按独立生成产出并写下 standalone 摘要，而导出侧只看文件存在就按 mixed 比，
/// 刚生成的 BGM 立刻被判过期。
fn usable_voice_seconds(project_dir: &Path) -> Option<f64> {
    std::fs::read(project_dir.join("out/final.wav"))
        .ok()
        .and_then(|bytes| aw_core::dub::wav_duration(&bytes).ok())
        // `is_finite` 不是多余的：畸形 wav（采样率写成 0）会让时长算成 `inf`，
        // 只判 `> 0.0` 会把它当可用，随后 BGM 在算段数时炸在一个看不出根因的地方。
        // 这一类"数值上是正数但不是可用值"的边界，判据要写成"有限且为正"。
        .filter(|d| d.is_finite() && *d > 0.0)
}

/// 当前工程有没有可用的配音成品（决定 BGM 是混音还是独立生成）。
fn has_voice_product(ui: &MainWindow) -> bool {
    let name = file_stem(&ui.get_project_name());
    usable_voice_seconds(&project_dir(&name)).is_some()
}

/// 写产物清单时用的摘要：模式由**这次实际的生成结果**给（`mixed` 来自 worker 回报），
/// 比"再看一眼磁盘"更准。
fn bgm_digest_now(ui: &MainWindow, mixed: bool) -> String {
    export::bgm_options_digest(
        &ui.get_bgm_prompt(),
        if mixed {
            export::BgmMode::Mixed
        } else {
            export::BgmMode::Standalone
        },
        bgm_standalone_seconds(ui.get_bgm_standalone_index()),
        duck_gain_for(ui.get_bgm_duck_index()),
    )
}

/// 把「全局设置 + 模型清单」的现状回灌到界面。
fn refresh_settings_view(ui: &MainWindow) {
    let (host, port, from_env) = server_endpoint();
    let dir = model_dir();
    ui.set_model_dir(dir.display().to_string().into());
    let (exists, gguf) = scan_model_dir(&dir);
    let total = config_summary();
    let under = models_under_dir(&read_server_config(), &dir);
    ui.set_model_dir_info(
        if !exists {
            format!("目录不存在（清单里有 {total} 个模型）")
        } else {
            format!("目录内 {gguf} 个 .gguf · 清单 {total} 个模型，其中 {under} 个在这个目录下")
        }
        .into(),
    );
    // AW_SERVER 生效时：输入框**保持真实可编辑值**（放占位串会被用户连"应用"一起写进
    // settings.json ——审查抓到过这条污染），只把输入锁住 + 在状态行说明谁在生效。
    let (shown_host, shown_port) = if from_env {
        let over = settings_snapshot();
        let cfg = read_server_config();
        let (cfg_host, cfg_port) = split_base(&cfg);
        let (h, p, _) = resolve_endpoint(
            over.host.as_deref(),
            over.port,
            cfg_host.as_deref(),
            cfg_port,
            None,
        );
        (h, p)
    } else {
        (host.clone(), port.clone())
    };
    ui.set_server_host(shown_host.into());
    ui.set_server_port(shown_port.into());
    ui.set_server_locked(from_env);
    if from_env {
        ui.set_server_status(format!("环境变量 AW_SERVER 正在覆盖：{host}（输入已锁定）").into());
    }
}

/// 重新发现引擎（模型清单）并刷新界面。
///
/// `invalidate` 为 Some 时同时作废旧工程（换服务/换清单后旧产物不可信）；
/// 启动阶段还没有 worker 工程，传 None。
fn apply_engine_discovery(ui: &MainWindow, invalidate: Option<(&Sender<Cmd>, &Rc<UiState>)>) {
    let keep = (ui.get_voice_index() >= 0)
        .then(|| ui.get_voice_names().row_data(ui.get_voice_index() as usize))
        .flatten()
        .map(|n| n.to_string());
    let (voices, _, note) = discover_engine();

    // 保留用户上次选的引擎（还在列表里就继续用）；否则**按能力**挑默认，不写死 id。
    // 必须在 voices 被搬进 VecModel 之前算完。
    let pick = keep
        .and_then(|name| voices.iter().position(|v| v.name.as_str() == name.as_str()))
        .map(|i| i as i32)
        .unwrap_or_else(|| default_engine_index(&voices));

    let names: Vec<SharedString> = voices.iter().map(|v| v.name.clone()).collect();
    ui.set_voice_names(ModelRc::from(Rc::new(VecModel::from(names))));
    ui.set_voices(ModelRc::from(Rc::new(VecModel::from(voices))));

    let changed = pick != ui.get_voice_index();
    ui.set_voice_index(pick);
    // 音乐制作的引擎清单也来自同一份 server.json（task=gen），不写死 yue2 / ace-step
    refresh_song_engine_options(ui);
    // 音色设计模型候选也来自同一份 server.json（task=vdes）
    refresh_design_models(ui);
    refresh_settings_view(ui);
    refresh_voice_labels(ui);

    if changed {
        if let Some((tx, st)) = invalidate {
            invalidate_worker_project(tx, st);
            reset_bgm(ui, st);
            ui.set_has_result(false);
        }
    }
    if !note.is_empty() {
        ui.set_status_text(format!("模型清单：{note}").into());
    }
}

/// 选一个词条文件（系统文件框，后台线程 + 消息回传）。
///
/// 平台分叉（osascript / powershell / zenity）与"取消 / 起不来"的判定都在
/// `picker` 模块一处；这里只给提示语与过滤器。返回三态：`Picked` 才是选到了。
fn pick_text_file_blocking() -> picker::Outcome<String> {
    picker::pick_file(
        "选择词条文件（.tsv/.csv/.txt）",
        "词条文件",
        &["*.tsv", "*.csv", "*.txt"],
    )
}

/// 系统目录选择框：阻塞式原生对话框，必须放后台线程，结果回消息通道。
///
/// macOS 用 osascript 的 choose folder；Windows 用 PowerShell 的
/// FolderBrowserDialog；Linux 用 zenity。**三态结论由 `picker` 模块给**
/// （选到 / 用户取消 / 选择器不可用）——这里不再把后者塌成 `None`。
fn spawn_folder_pick(msg_tx: Sender<WorkerMsg>, revision: u64) {
    std::thread::spawn(move || {
        let pick = picker::pick_folder("选择模型目录");
        let _ = msg_tx.send(WorkerMsg {
            revision,
            msg: Msg::ModelDirPicked { pick },
        });
    });
}

/// 一键备份：**一条后台线程走完"选目录 → 复制"两步**。
///
/// 目录选择框是阻塞式的（与其它选择器同款），备份本身也是长 IO——两步都必须在后台，
/// 否则点下去界面就冻住了。中间先回一条 `BackupDirPicked`，让状态行能立刻显示
/// "正在备份到 <路径>"，而不是让人对着"正在打开目录选择框…"猜有没有开始。
fn spawn_backup(msg_tx: Sender<WorkerMsg>, workshop_dir: PathBuf) {
    std::thread::spawn(move || {
        let pick = picker::pick_folder("选择备份目标目录");
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::BackupDirPicked { pick: pick.clone() },
        });
        // 取消 / 选择器不可用：都不该开始复制（状态行由 UI 侧如实区分这两条路）
        let picker::Outcome::Picked(root) = pick else {
            return;
        };
        let result = backup::backup_all(&workshop_dir, Path::new(&root), &backup::stamp_now())
            .map(|s| s.note());
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::BackupDone { result },
        });
    });
}

/// 检查更新：网络请求走**后台线程**，界面不冻。
///
/// 与备份同一形状：只把终态消息（`Msg::UpdateCheckDone`）发回来，tick 里收。
/// **只读**——拉一份清单、比对版本，不碰本机任何文件，所以它不需要
/// `backup_refusal` 那套"别抄到写了一半的产物"守卫（理由见 `update_refusal`）。
fn spawn_update_check(msg_tx: Sender<WorkerMsg>, url: String, current: String) {
    std::thread::spawn(move || {
        let result = update::check(&url, &current);
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::UpdateCheckDone { result },
        });
    });
}

/// 用系统默认浏览器打开一个 **http(s)** 地址（发布页）。
///
/// 地址来自外部清单，不能把 `file://` / 自定义 scheme 丢给系统打开器
/// （`open` 会把它们当本地路径/协议处理）。判据走 `update::is_http_url`——
/// **不要在别处再写一遍前缀判断**（两份实现必然漂移）。
///
/// 三平台语义（与 docs/update.md 的安全边界同源）：
///   · macOS/Linux：直接 `open` / `xdg-open`，URL 按 argv 参数传，不经 shell；
///   · Windows：`ShellExecuteW`（见 `open_external_url_windows`），**不经 cmd**——
///     cmd.exe /C 会对整条命令行再做一次元字符解析，URL 里的 `&` 会被当成第二条命令。
/// 清单 URL 是**不可信输入**：除 http(s) 前缀外，含控制字符（`\n`/`\r`/`\0` 等）的
/// 地址也不是合法 URL，一并拒绝。
fn open_external_url(url: &str) -> Result<(), String> {
    if !release_url_is_openable(url) {
        if !update::is_http_url(url) {
            return Err(
                "发布页地址不是 http(s)：为了不让系统打开本地路径/自定义协议，这次不打开".into(),
            );
        }
        return Err(format!(
            "发布页地址带控制字符（\\n/\\r/\\0 等），不是合法 URL，这次不打开：{:?}",
            url.trim()
        ));
    }
    let url = url.trim();

    #[cfg(target_os = "windows")]
    return open_external_url_windows(url);

    #[cfg(not(target_os = "windows"))]
    {
        #[cfg(target_os = "macos")]
        let (program, args): (&str, Vec<&str>) = ("open", vec![url]);
        #[cfg(all(unix, not(target_os = "macos")))]
        let (program, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);

        let out = std::process::Command::new(program)
            .args(args)
            .output()
            .map_err(|e| format!("没能拉起系统浏览器（{program}: {e}）——发布页：{url}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "系统浏览器没打开（{program} 退出码 {:?}）——发布页：{url}",
                out.status.code()
            ))
        }
    }
}

/// 发布页地址能不能交给系统打开器：http(s) 前缀（`update::is_http_url`，唯一判据）
/// 且不含控制字符（`\n`/`\r`/`\0` 等）。
///
/// 控制字符不是合法 URL 的一部分，也不该流进打开器参数/错误文案；
/// 这是打开动作的**唯一入口判据**，别处不许再写一遍（同一语义两份实现必然漂移）。
fn release_url_is_openable(url: &str) -> bool {
    update::is_http_url(url) && !url.trim().chars().any(char::is_control)
}

/// Windows 侧打开外部链接：`ShellExecuteW` 直接交给 Shell，**不经 cmd**。
///
/// 为什么不能走 cmd：`cmd.exe /C` 收到的是整条命令行，会再做一遍自己的元字符解析；
/// `std::process::Command` 的 argv 转义在 cmd 这一层不成立（`&` 不触发引号），
/// 于是 `https://x/?a=1&b=2` 会先打开页面、再把 `b=2` 当命令执行——命令注入 +
/// 常见 URL 截断。`ShellExecuteW` 的 URL 是独立参数，由 Shell 按 scheme 分发给
/// 默认浏览器，不做 shell 解析。
///
/// 返回值语义：>32 为成功（HINSTANCE），<=32 是 SE_ERR_* 错误码。
#[cfg(target_os = "windows")]
fn open_external_url_windows(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    // UTF-16 + 显式 NUL 结尾；`1` = SW_SHOWNORMAL（常规窗口打开）。
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` 是 NUL 结尾的 UTF-16 缓冲，`as_ptr()` 得到的指针在本次调用
    // 期间有效且不会被改写；其余参数（父窗口/动作/参数/工作目录）都是 null 指针或
    // 字面量，不涉及解引用。ShellExecuteW 本身设计为可跨线程调用。
    let ret = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    } as isize;
    if ret > 32 {
        Ok(())
    } else {
        Err(format!(
            "系统浏览器没打开（ShellExecuteW 错误码 {ret}）——发布页：{url}。\
             地址若不是 http(s) 会被 `update::is_http_url` 直接拒绝，请核对清单里的发布页地址"
        ))
    }
}

/// 选一段待分离音频（系统文件框，后台线程 + 消息回传）。
fn spawn_file_pick(msg_tx: Sender<WorkerMsg>) {
    std::thread::spawn(move || {
        let pick = picker::pick_file(
            "选择要分离的音频",
            "音频",
            &["*.wav", "*.mp3", "*.flac", "*.m4a", "*.ogg"],
        );
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::SeparationInputPicked { pick },
        });
    });
}

/// 选翻唱源音频（系统文件框，后台线程 + 消息回传；与分离共用 `picker` 三态）。
fn spawn_song_source_pick(msg_tx: Sender<WorkerMsg>) {
    std::thread::spawn(move || {
        let pick = picker::pick_file(
            "选择翻唱源音频",
            "音频",
            &["*.wav", "*.mp3", "*.flac", "*.m4a", "*.ogg"],
        );
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::SongSourcePicked { pick },
        });
    });
}

/// 界面上那个「数字 / 年份规范化」开关被切换时：作废当前工程。
///
/// 它改的是 **spoken 文本**（数字/年份怎么念），所以旧音频一律不能复用——
/// 与换模型/换音色是同一类作废（重录），不是"重新拼装就行"。
/// PixelSwitch 没有回调，只能在 tick 里比对；任务在飞时不让改（把开关拨回去）。
fn sync_auto_normalize_toggle(ui: &MainWindow, state: &Rc<UiState>) {
    let now = ui.get_auto_normalize();
    let seen = state.auto_normalize_seen.get();
    if now == seen {
        return;
    }
    if project_editing_blocked(ui, state) || batch_in_flight(state) {
        // 拨回去（下一次 tick 就与 seen 一致了），并说清为什么不让改。
        // 批量也要算"进行中"：它的 auto_normalize 在提交那一刻就定格了。
        ui.set_auto_normalize(seen);
        ui.set_status_text("任务进行中：兜底规则暂不可改".into());
        return;
    }
    state.auto_normalize_seen.set(now);
    state.project_ready.set(false);
    state
        .project_revision
        .set(state.project_revision.get().wrapping_add(1));
    state.assembled.borrow_mut().take();
    reset_bgm(ui, state);
    ui.set_has_result(false);
    ui.set_status_text(if now {
        "兜底规则已开：重新合成后生效（数字/年份按规则念）".into()
    } else {
        "兜底规则已关：重新合成后生效（数字交给引擎自己念）".into()
    });
}

/// 界面上「停顿」输入 → 毫秒。留空 / 非数字回落默认 `GAP_MS`；夹在 0..=2000
/// （2000ms 已经长到能听出明显断句，再往上多半是误输入）。
fn normalize_gap_ms(text: &str) -> u64 {
    text.trim()
        .parse::<u64>()
        .map(templates::clamp_gap_ms)
        .unwrap_or(GAP_MS)
}

/// 当前界面的句间停顿（毫秒）。
///
/// 顺手把归一后的值写回输入框：用户填 3000 时实际按 2000 用，界面也必须显示 2000
/// ——"显示一套、执行一套"是最容易骗人的。**只在真正用到它的时刻回写**（提交/导出），
/// 不在每次按键时回写：那样用户想清空重填都会被立刻塞回默认值。
fn gap_ms_from_ui(ui: &MainWindow) -> u64 {
    let raw = ui.get_gap_ms_text().to_string();
    let norm = normalize_gap_ms(&raw);
    if raw.trim() != norm.to_string() {
        ui.set_gap_ms_text(norm.to_string().into());
    }
    norm
}

/// 停顿输入的即时反馈：超上限/非法时在状态行说清会按什么值用（不回写输入框，
/// 免得打断正在输入的用户）。
fn gap_hint_for(raw: &str) -> String {
    let norm = normalize_gap_ms(raw);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return format!("停顿留空 = 默认 {norm} 毫秒");
    }
    if trimmed
        .parse::<u64>()
        .map(|v| v > templates::MAX_GAP_MS)
        .unwrap_or(false)
    {
        return format!("停顿上限 {} 毫秒：会按 {norm} 应用", templates::MAX_GAP_MS);
    }
    if trimmed.parse::<u64>().is_err() {
        return format!(
            "停顿要填毫秒数（0–{}）：会按 {norm} 应用",
            templates::MAX_GAP_MS
        );
    }
    format!("句间停顿 {norm} 毫秒（改了重新导出就生效）")
}

/// 所有导出（批量导出 / 分轨导出 / BGM 逐轨导出）共用的互斥判据：
/// **有别的动作正在写这些成品时不导**。
///
/// 原因不是"读会读到半截文件"——这些文件都是原子写出来的；而是有两类问题：
///   · `assemble` 是**先发布 final.wav、再写 final.srt**：中间那一瞬扫过去会看到
///     新 WAV 配旧 SRT（或把新工程误报成"缺字幕"）；
///   · 分轨导出的源（`out/mixed.wav`、`bgm/bgm.wav`）是 BGM 混音
///     写出来的，混音跑到一半导出去就是半套。
/// 写这些产物的动作都有在飞标志：
///   · 单篇拼装/导出 / BGM 生成 / 重录 / 试听 → UI 的 `busy`；
///   · 批量 worker 每篇的拼装 → 在跑那一行是 Running（`batch_in_flight`）。
fn export_refusal(ui_busy: bool, batch_in_flight: bool) -> Option<&'static str> {
    if batch_in_flight {
        return Some("批量任务正在跑：等它跑完再导出（避免导到刚写了一半的成对产物）");
    }
    if ui_busy {
        return Some("拼装/合成/导出正在进行：等它结束再导出");
    }
    None
}

/// 能不能起一次新备份。理由与 `export_refusal` 同一族（都是"别抄到写了一半的文件"），
/// 但**判据必须比它宽**：备份抄的是整棵 `projects/`，而写 `projects/` 的不止"拼装"——
/// 歌曲写 `song/`、人声分离写 `stems/`、质检写报告、批量每篇都在写。这些任务**不设
/// 全局 `busy`**（各自页面 busy + 一条台账），所以只查 `busy` 会漏（复核第二轮抓到的阻塞）。
/// 这里把四类来源都收进来：
///   · `backup_running`：连点会起一堆线程，两批同时往同一个目标目录里写；
///   · `tasks_in_flight`：台账里还有排队/运行中的任务（配音/BGM/歌曲/分离/质检/批量都在里面）；
///   · `ui_busy` / `ui_running`：正在拼装，或单篇配音在跑（单篇设的是 `running` 不是 `busy`）。
fn backup_refusal(
    ui_busy: bool,
    ui_running: bool,
    tasks_in_flight: bool,
    batch_in_flight: bool,
    backup_running: bool,
) -> Option<&'static str> {
    if backup_running {
        return Some("备份还在进行：等它写完（工程大时要一会儿）");
    }
    if tasks_in_flight || batch_in_flight {
        return Some("还有任务在跑（配音/BGM/歌曲/人声分离/质检/批量）：等它跑完再备份（否则会抄到写了一半的产物）");
    }
    if ui_busy || ui_running {
        return Some("合成/拼装正在进行：等它结束再备份（否则会抄到写了一半的产物）");
    }
    None
}

/// 「一键备份…」当前该不该灰掉——**只做投影，不另设判据**。
///
/// 三轮复核都在同一条线上：按钮的 enabled 与回调的判据一旦各算各的，就会出现
/// 「按钮亮着却点不动」（`AW_UI_STATE=batch` 的演示行：`batch_in_flight` 真、
/// 而 Slint 侧拼的计数不真）或「按钮灰着但判据说不忙」（`AW_UI_STATE=tasks` 的
/// 演示任务：Slint 计数真、而 `tasks_in_flight` 对演示态短路为假）。
/// 所以按钮**不再自己拼计数**，一律走这一份——与点击回调同一个 `backup_refusal`。
fn backup_blocked(
    ui_busy: bool,
    ui_running: bool,
    tasks_in_flight: bool,
    batch_in_flight: bool,
    backup_running: bool,
) -> bool {
    backup_refusal(
        ui_busy,
        ui_running,
        tasks_in_flight,
        batch_in_flight,
        backup_running,
    )
    .is_some()
}

/// 把上面那份判据投影到 UI（每 tick 同步一次；值没变时 Slint 不会重绘）。
fn refresh_backup_availability(ui: &MainWindow, state: &Rc<UiState>) {
    ui.set_backup_blocked(backup_blocked(
        ui.get_busy(),
        ui.get_running(),
        tasks_in_flight(state),
        batch_in_flight(state),
        state.backup_running.get(),
    ));
}

/// 「检查更新」能不能点：**唯一判据**——按钮的 `update-blocked` 就是它的投影，
/// 点击回调也走它（两处各写一份必然漂移，见
/// `LESSON_同一语义两处实现必然漂移回显需与真实行为同源.md`）。
///
/// 只有"上一次检查还在飞"需要拦：这是**只读**网络请求，不写任何工程/产物文件，
/// 所以合成/BGM/分离/批量在跑时照样可以查更新（别把它塞进 `backup_refusal` 那套
/// "别抄到写了一半的产物"里——那是写盘守卫，与只读检查无关）。
fn update_refusal(update_running: bool) -> Option<&'static str> {
    if update_running {
        return Some("上一次检查还没回来：等结果出来再点（网络慢时可能要等几十秒）");
    }
    None
}

fn update_blocked(update_running: bool) -> bool {
    update_refusal(update_running).is_some()
}

/// 把它投影到 UI（每 tick 同步一次；值没变时 Slint 不会重绘）。
fn refresh_update_availability(ui: &MainWindow, state: &Rc<UiState>) {
    ui.set_update_blocked(update_blocked(state.update_running.get()));
    // 「打开发布页」能不能点 = 当前手里有没有一个待打开的发布页，
    // 就是 `state.update_release` 这一个 Option 的投影（同一个来源，不另设条件）
    ui.set_update_has_release(state.update_release.borrow().is_some());
}

/// 批量导出：扫 projects/ 下有成品的工程，按导出开关复制到导出目录。
///
/// 放后台线程而不是 worker：导出只读磁盘上**已经拼好**的成品（`out/final.wav` 是原子写），
/// 不碰 worker 的 `current` 工程，也就不该占用任务队列的提交守卫（导出期间还要能继续合成）。
fn spawn_batch_export(
    msg_tx: Sender<WorkerMsg>,
    projects_root: PathBuf,
    dir: PathBuf,
    wav_on: bool,
    srt_on: bool,
) {
    std::thread::spawn(move || {
        let outcome = export::export_all(&projects_root, &dir, wav_on, srt_on);
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::BatchExportDone { dir, outcome },
        });
    });
}

// ===========================================================================
// 模型下载器（M4-P7）：可下载模型 → 串行队列 → UI 行
// ===========================================================================

/// 一条真的能点的下载入口（落点 / 校验依据 / 来源 / 落点提醒）。
struct DownloadableModel {
    id: String,
    url: String,
    sha256: Option<String>,
    size: Option<u64>,
    dest: PathBuf,
    origin: model_sources::Origin,
    /// 与 server.json 声明的 path 对不上时的提醒（下完服务可能仍加载不了）。
    conflict: Option<String>,
}

/// 下载面板的完整规划：**显示与点击共用这一份**（两处各拼一次判据已经被复核抓过）。
///
/// 来源 = `server.json` ∪ 内置清单（`config/model-downloads.json`，由
/// `tools/gen_model_downloads.py` 从上游 `model_specs` 生成）。服务清单里显式给了
/// `url` 的以服务为准——服务侧可以覆盖内置清单（内网镜像、自建仓库）。
fn download_plan() -> Vec<model_sources::Row> {
    let server: Vec<model_sources::ServerEntry> = read_server_config()
        .map(|cfg| {
            cfg.models
                .into_iter()
                .map(|m| model_sources::ServerEntry {
                    id: m.id,
                    url: m.url,
                    sha256: m.sha256,
                    size: m.size,
                    path: m.path,
                })
                .collect()
        })
        .unwrap_or_default();
    model_sources::plan_rows(&server, model_sources::catalog(), &model_dir())
}

/// 当前生效的下载镜像前缀（设置里的原文；空前缀 = 用官方）。
///
/// **唯一入口**：`download_mirror_rewrite`（真正改写 URL）与界面回显
/// `download_source_note` 都从它读，免得"回显一套、行为另一套"。
fn effective_download_mirror(s: &AppSettings) -> String {
    s.download_mirror.clone().unwrap_or_default()
}

/// 把清单里的 URL 换成**当前生效的源**。这是唯一一处调用 `download_mirror::rewrite_url`
/// 的地方——非 HF 链接原样返回，填了镜像就只走镜像（不做静默回退）。
fn download_mirror_rewrite(url: &str) -> String {
    let mirror = effective_download_mirror(&settings_snapshot());
    download_mirror::rewrite_url(url, &mirror)
}

/// 「当前生效的源」那一行（抽屉里真实显示的那句）。
fn download_source_note(s: &AppSettings) -> String {
    download_mirror::source_note(&effective_download_mirror(s))
}

/// 生效并发数（唯一归一化入口：`download::effective_concurrency`）。
fn effective_download_concurrency(s: &AppSettings) -> u32 {
    download::effective_concurrency(s.download_concurrency)
}

/// 抽屉里「下载源 / 并发」两块回显的**唯一**刷新点。
///
/// 三件事一起做、不漏一件：① 输入框填回当前设置；② 生效源那一行从
/// `download_source_note` 投影；③ 并发行写明"下一次启动生效"（线程池已经起了，
/// 不这么写就是界面说谎）。
fn refresh_download_source_view(ui: &MainWindow) {
    let s = settings_snapshot();
    ui.set_download_mirror(effective_download_mirror(&s).into());
    ui.set_download_concurrency(match s.download_concurrency {
        Some(n) => n.to_string().into(),
        None => "".into(),
    });
    ui.set_download_source_note(download_source_note(&s).into());
    ui.set_download_concurrency_note(
        format!(
            "同时下载 {} 个模型（并发数改动下次启动生效；每个模型的下载/取消互不影响）",
            effective_download_concurrency(&s)
        )
        .into(),
    );
}

/// 推荐用的「本机尺度」：物理内存（平台探测）+ 服务守卫要求的余量（`server.json`
/// 的 `min_free_memory_mb`，缺省 1024 MiB —— 与 `config/models.schema.yaml` 的默认一致）。
///
/// **唯一一处**组装它：界面文案与真实判据都从这份值算，免得"回显一套、行为另一套"。
fn machine_budget() -> model_sources::MachineBudget {
    let headroom = read_server_config()
        .and_then(|cfg| cfg.min_free_memory_mb)
        .map_or(model_sources::DEFAULT_HEADROOM_BYTES, |mb| {
            u64::from(mb) * 1024 * 1024
        });
    model_sources::MachineBudget::detect(headroom)
}

/// 档位列表文案（`q8_0 1.07 GiB · f16 1.75 GiB`）。
///
/// 多档时补一句「下载按钮取的是哪一档」：按钮下的是与清单 `path` 对应的**那一档**，
/// 不写清楚就会出现"界面推荐 f16、按钮却在下 q8_0"的错位。
fn tier_line(tiers: &[model_sources::Tier]) -> String {
    if tiers.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = tiers
        .iter()
        .map(|tier| {
            let label = if tier.precision.trim().is_empty() {
                tier.format.as_str()
            } else {
                tier.precision.as_str()
            };
            match tier.download_bytes {
                Some(bytes) => format!("{label} {}", model_sources::human_bytes(bytes)),
                None => format!("{label} 体积未知"),
            }
        })
        .collect();
    let mut line = format!("档位：{}", parts.join(" · "));
    if tiers.len() > 1 {
        if let Some(entry) = tiers.iter().find(|tier| tier.is_entry) {
            let label = if entry.precision.trim().is_empty() {
                entry.format.as_str()
            } else {
                entry.precision.as_str()
            };
            line.push_str(&format!("（下载按钮取 {label}）"));
        }
    }
    line
}

/// 从规划里取一条真的能点的入口（点击时用；没有入口的模型这里返回 None）。
fn download_entry_for(key: &str) -> Option<DownloadableModel> {
    download_plan()
        .into_iter()
        .find(|r| r.id == key)
        .and_then(|r| {
            r.action.map(|a| DownloadableModel {
                id: r.id,
                url: a.url,
                sha256: a.sha256,
                size: a.size,
                dest: a.dest,
                origin: a.origin,
                conflict: a.conflict,
            })
        })
}

/// 人类可读的字节数（进度行用；与备份的 human_bytes 不同档，这里只求短）。
fn short_bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    if n as f64 >= MB {
        format!("{:.1} MB", n as f64 / MB)
    } else if n as f64 >= KB {
        format!("{:.0} KB", n as f64 / KB)
    } else {
        format!("{n} B")
    }
}

/// 一条下载任务 → 界面行。没有队列快照时按磁盘上有没有正式文件给出"未下载/已就位"。
///
/// `tiers` / `advice` 由 `model_sources` 算好（纯函数 + 注入的本机尺度），
/// 这里只做投影 —— 界面不拼判据。
fn download_row_for(
    m: &DownloadableModel,
    latest: Option<&download::Snapshot>,
    tiers: &[model_sources::Tier],
    advice: &model_sources::Advice,
) -> DownloadRow {
    let (state_text, base, progress, active) = match latest {
        Some(snap) => {
            let downloading = matches!(
                snap.state,
                download::State::Downloading | download::State::Verifying
            );
            let detail = if downloading {
                let mut d = format!(
                    "已下载 {} / {}",
                    short_bytes(snap.downloaded),
                    snap.total
                        .map(short_bytes)
                        .unwrap_or_else(|| "?".to_string())
                );
                if !snap.note.is_empty() {
                    d.push_str(" · ");
                    d.push_str(&snap.note);
                }
                d
            } else if snap.note.is_empty() {
                // 排队中还没说明：把落盘目标先摆出来
                snap.dest.display().to_string()
            } else {
                // 终态把原因/说明与落盘路径一起给（用户要知道文件去了哪）
                format!("{} · {}", snap.note, snap.dest.display())
            };
            (
                snap.state.label().to_string(),
                detail,
                snap.fraction(),
                !snap.state.is_terminal(),
            )
        }
        None if m.dest.exists() => (
            "已就位".to_string(),
            m.dest.display().to_string(),
            1.0,
            false,
        ),
        None => (
            "未下载".to_string(),
            m.dest.display().to_string(),
            0.0,
            false,
        ),
    };
    // 来源 + 落点提醒都要写在行上：否则用户看不出"权重是哪来的"，也看不出
    // "下完了服务为什么还是加载不了"。
    let mut detail = format!("{} · {}", m.origin.label(), base);
    // 没有 sha256 的条目必须如实说：校验只对大小（同大小的旧权重查不出来）。
    // 真实清单重生成后每一条都带哈希，这条基本不可达；留着是给"服务侧给了 url、
    // 两边都没 sha"的兜底形态一个诚实的说法，不许假装有哈希。
    if m.sha256.is_none() {
        detail.push_str(" · 仅校验大小");
    }
    if let Some(conflict) = &m.conflict {
        detail.push_str(" · ⚠ ");
        detail.push_str(conflict);
    }
    DownloadRow {
        key: m.id.clone().into(),
        label: m.id.clone().into(),
        state: state_text.into(),
        detail: detail.into(),
        tiers: tier_line(tiers).into(),
        advice: advice.note.clone().into(),
        progress,
        active,
        actionable: true,
    }
}

/// 没有下载源的模型也占一行：如实说为什么，**按钮不可点**。
/// 诚实边界——不给一个点了必然失败的入口（gated / 上游没有该权重的包 / 没有 spec）。
fn no_source_row_for(id: &str, reason: &str) -> DownloadRow {
    DownloadRow {
        key: id.into(),
        label: id.into(),
        state: "没有下载源".into(),
        detail: reason.into(),
        // 没有下载入口就谈不上"有哪几档"，两栏留空（不摆一个选不了的档位）
        tiers: "".into(),
        advice: "".into(),
        progress: 0.0,
        active: false,
        actionable: false,
    }
}

/// 重建"可下载模型"列表：清单元数据 + 队列里每条的最新快照。
fn refresh_download_rows(ui: &MainWindow, state: &Rc<UiState>) {
    let snapshots = state.downloads.borrow();
    let budget = machine_budget();
    let catalog = model_sources::catalog();
    let rows: Vec<DownloadRow> = download_plan()
        .into_iter()
        .map(|row| {
            // 先解构：`action` 会被 move，拆成三个局部变量后两个分支都不带部分移动
            let model_sources::Row { id, action, reason } = row;
            // 档位与本机推荐：同一个纯函数给"仅有的这几个"行算，不在这里另写判据
            let (tiers, advice) = model_sources::tiers_and_advice(catalog, &id, &budget);
            match action {
                Some(action) => {
                    let model = DownloadableModel {
                        id,
                        url: action.url,
                        sha256: action.sha256,
                        size: action.size,
                        dest: action.dest,
                        origin: action.origin,
                        conflict: action.conflict,
                    };
                    let latest = snapshots.iter().rev().find(|s| s.label == model.id);
                    download_row_for(&model, latest, &tiers, &advice)
                }
                None => no_source_row_for(&id, &reason),
            }
        })
        .collect();
    drop(snapshots);
    ui.set_download_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// 某模型当前在跑的下载任务 id（只读；借用在本函数内就还掉）。
///
/// 「读」与「摘」刻意分开：`if let Some(x) = map.borrow().get(..)` 在 Rust 2021 里
/// 临时借用会活到整个 if-let（含块），块里再 `borrow_mut` 会直接 panic——自己抓到过的真坑。
fn active_download_id(state: &Rc<UiState>, key: &str) -> Option<u64> {
    state.download_ids.borrow().get(key).copied()
}

/// 摘掉某模型的下载登记——**只有"登记的确实就是这条任务"才摘**。
///
/// 复核第二轮 B1：**同一个模型**可以被反复登记（取消后再下、重下），所以列表里可能
/// 同时存在新旧两条任务的快照。旧任务的**迟到**终态不能把用户刚登记的新任务摘掉——
/// 如果只按 label 摘、不看 `id`，UI 会以为空闲、「下载」按钮又可点，于是同一个 dest
/// 上叠出第二条任务抢同一个 `.part`。
///
/// （P7 第二段之后一条任务只推**一条**终态——`worker_loop` 收尾后那条；但"迟到快照"
/// 依然可能出现：取消与重排之间的顺序不保证，所以这道按 id 认领的守卫仍然必要。）
fn release_download_id(ids: &mut HashMap<String, u64>, label: &str, id: u64) -> bool {
    if ids.get(label) == Some(&id) {
        ids.remove(label);
        true
    } else {
        false
    }
}

/// 把一条快照并进列表：同一模型只保留**最新那条任务**的快照。
///
/// id 单调递增，"最新"就是 id 最大。旧任务的迟到快照（尤其是它的终态）不能盖掉新任务的
/// 进度与状态——否则界面会显示旧任务的「已取消 / 已完成」，而新任务其实正在跑，
/// 按钮文案与 `download_ids` 的登记也会对不上。
fn merge_download_snapshot(list: &mut Vec<download::Snapshot>, snap: download::Snapshot) {
    match list.iter_mut().find(|s| s.label == snap.label) {
        Some(slot) if snap.id >= slot.id => *slot = snap,
        Some(_) => {}
        None => list.push(snap),
    }
}

/// 一次「下载/取消」点击实际发生了什么（UI 只据它发提示）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ClickEffect {
    /// 首次请求取消：标志已置位，等这一步收尾
    CancelRequested(u64),
    /// 之前已经请求过取消、任务还在收尾：这次 no-op
    AlreadyCancelling(u64),
    /// 任务其实已经收尾（终态快照还在路上）：如实说，不谎报"已取消"
    AlreadyFinished(u64),
    /// 没有在跑的 → 新排一条
    Enqueued { id: u64, dest: PathBuf },
    /// 队列里已经有别的任务在写**同一个目标文件**（不同模型 id、同一落点，
    /// 例如 audio8-tts 与 audio8-tts-stream 指向同一份权重）：第二个 writer 会把
    /// `.part` 搅坏，所以拒绝，并说清是谁在写。
    DuplicateDestination { existing: u64 },
    /// 清单里找不到这个模型（点之前清单被改过）
    UnknownModel,
}

/// 处理一次「下载/取消」点击：有在跑的就取消，没有才排新任务。
///
/// **取消时绝不动 `download_ids`**（复核 B1）。取消是协作式的——旧任务可能还在写
/// `.part`（读循环最多再落一个 64KiB 块才看到标志）。这里若把 id 摘掉，下一次点击就会
/// 被当成"没有在跑"而再排一条，两条任务抢同一个 `.part`；只给 `size` 不给 `sha256` 的
/// 条目只按大小校验，"内容坏但长度恰好对上"会被 rename 成正式文件。id 只由 worker 的
/// 终态快照摘除（见 tick），所以取消未收尾期间再点只会是 no-op。
fn apply_download_click(
    state: &Rc<UiState>,
    key: &str,
    cancel: impl FnOnce(u64) -> download::CancelOutcome,
    enqueue: impl FnOnce() -> Option<(download::Enqueued, PathBuf)>,
) -> ClickEffect {
    if let Some(id) = active_download_id(state, key) {
        return match cancel(id) {
            download::CancelOutcome::Requested => ClickEffect::CancelRequested(id),
            download::CancelOutcome::AlreadyRequested => ClickEffect::AlreadyCancelling(id),
            download::CancelOutcome::Finished => ClickEffect::AlreadyFinished(id),
        };
    }
    match enqueue() {
        Some((download::Enqueued::Started(id), dest)) => {
            state.download_ids.borrow_mut().insert(key.to_string(), id);
            ClickEffect::Enqueued { id, dest }
        }
        // 队列层去重：**不登记**（登记了就再也取消不掉别人的任务）
        Some((download::Enqueued::Duplicate { existing }, _)) => {
            ClickEffect::DuplicateDestination { existing }
        }
        None => ClickEffect::UnknownModel,
    }
}

/// 模型下载接线：`effective_download_concurrency()` 个后台 worker + 每个可下载模型
/// 一个「下载/取消」按钮。
///
/// worker 把每条快照通过既有的 tick 消息泵发回 UI（`Msg::DownloadUpdate`，
/// 已在 `message_ignores_revision` 白名单里——与工程版本无关，改稿不该丢进度）。
fn wire_downloads(ui: &MainWindow, msg_tx: &Sender<WorkerMsg>, state: &Rc<UiState>) {
    refresh_download_rows(ui, state);

    let tx = msg_tx.clone();
    // 并发数在启动时读一次：改了设置要下次启动才换线程数（"应用并重连"会说清这一点，
    // 不假装立刻生效——线程池已经起来了，重启才是最诚实的口径）。
    let concurrency = effective_download_concurrency(&settings_snapshot());
    let downloader = Rc::new(download::Downloader::new(
        move |snap| {
            let _ = tx.send(WorkerMsg {
                revision: 0,
                msg: Msg::DownloadUpdate(snap),
            });
        },
        Some(concurrency),
    ));

    let weak = ui.as_weak();
    let st = Rc::clone(state);
    ui.on_download_model(move |key| {
        let Some(ui) = weak.upgrade() else { return };
        let key = key.to_string();
        let start_key = key.clone();
        // 回调是 FnMut：这两份 Rc 每次点击各自 clone 一次再进 FnOnce 闭包，
        // 不能在闭包外 move 进来（否则第二次点击就用不了）。
        let dl_cancel = Rc::clone(&downloader);
        let dl_start = Rc::clone(&downloader);
        let effect = apply_download_click(
            &st,
            &key,
            move |id| dl_cancel.cancel(id),
            move || {
                // 点到真正开跑之间清单可能被改过：找不到就说出来，不静默排个空
                let model = download_entry_for(&start_key)?;
                // **入队时**按当前生效的源改写 URL（唯一一处调用 rewrite_url）。
                // 填了镜像就只走镜像：地址不通就是网络错误，不静默回退官方。
                let enqueued = dl_start.enqueue(
                    download::TaskSpec {
                        label: model.id.clone(),
                        url: model.url.clone(),
                        dest: model.dest.clone(),
                        expected_sha256: model.sha256.clone(),
                        expected_size: model.size,
                    },
                    download_mirror_rewrite,
                );
                Some((enqueued, model.dest))
            },
        );
        let text = match effect {
            ClickEffect::Enqueued { dest, .. } => {
                format!("已加入下载队列：{key} → {}", dest.display())
            }
            ClickEffect::CancelRequested(_) => format!(
                "正在取消下载：{key}（取消是协作式的，等这一步收尾；这期间不会再为它排新下载）"
            ),
            ClickEffect::AlreadyCancelling(_) => {
                format!("正在取消下载：{key}（已在取消中，请稍候）")
            }
            ClickEffect::AlreadyFinished(_) => format!("这条下载已经结束了：{key}"),
            ClickEffect::DuplicateDestination { existing } => format!(
                "同一个目标文件已在下载队列里（任务 #{existing}）：不重复排第二个 writer，等它结束再操作"
            ),
            ClickEffect::UnknownModel => format!("清单里找不到可下载模型：{key}"),
        };
        ui.set_status_text(text.into());
    });
}

/// 批量导入：系统多选文件框，阻塞式，必须放后台线程（与目录/单文件选择器同款）。
fn spawn_scripts_pick(msg_tx: Sender<WorkerMsg>) {
    std::thread::spawn(move || {
        let pick = pick_scripts_blocking();
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::BatchScriptsPicked { pick },
        });
    });
}

/// 多选稿件（一次导入 N 篇是 P1 的入口）。
///
/// macOS / Linux **不过滤扩展名**：稿件可能是 .txt / .md / 无扩展名，让用户在对话框里
/// "看不到自己的文件"比进来之后告诉他"这个文件不是 UTF-8 文本"更差。
/// （Windows 沿用既有的「文本稿件|*.txt;*.md」过滤——本批不改对话框实现，
/// 所以这里按平台给过滤模式，Linux 传空表以保持"不过滤"的现状。）
/// 过滤与跳过理由都在 `batch::import_scripts`，一处判定。
fn pick_scripts_blocking() -> picker::Outcome<Vec<PathBuf>> {
    #[cfg(target_os = "windows")]
    let patterns: &[&str] = &["*.txt", "*.md"];
    #[cfg(not(target_os = "windows"))]
    let patterns: &[&str] = &[];
    picker::pick_files("选择稿件（可多选）", "文本稿件", patterns)
}

/// 「测试连接」：健康检查是阻塞 HTTP（最长 5s），放后台线程，结果回消息通道。
fn spawn_server_check(msg_tx: Sender<WorkerMsg>, revision: u64) {
    std::thread::spawn(move || {
        let (_, base, note) = discover_engine();
        let (ok, detail) = match base {
            Some(b) => {
                if Client::new(b.clone()).healthy() {
                    (true, format!("已连接 {b}"))
                } else {
                    // 连不上分两种：外部服务没起（我们不该管），或我们托管的引擎崩了
                    // （该拉起来）。内部会按"用户是否显式配了地址"重新判一遍，
                    // 所以这里无脑调用是安全的；结果不用管——测试连接的回显只看连不连得上。
                    let _ = ensure_engine_serving_throttled();
                    (false, format!("连不上 {b}：服务没起或端口不对"))
                }
            }
            None => (false, format!("没有服务地址。{note}")),
        };
        let _ = msg_tx.send(WorkerMsg {
            revision,
            msg: Msg::ServerHealth { ok, detail },
        });
    });
}

fn short_path(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn documents_dir() -> PathBuf {
    dirs::document_dir().unwrap_or_else(|| home_dir().join("Documents"))
}

/// 状态栏显示的服务后端（/health 回报的 backend）。
///
/// 注意与音色面板的 `engine-label`（当前**模型**名，如 audio8-tts）区分：
/// 这个是**服务/推理后端**（如 metal / cuda），两者不是一回事。
fn backend_label(base: Option<&str>) -> String {
    let Some(base) = base else {
        return "audio.cpp · 未发现服务".into();
    };
    match Client::new(base).backend_label() {
        Some(backend) => format!("audio.cpp · {}", backend.to_uppercase()),
        None => "audio.cpp · 服务不可达".into(),
    }
}

// ===========================================================================
// 工作线程：持有 Project，顺序处理命令（合成是串行关键路径，无需并行）
// ===========================================================================

struct WorkerCtx {
    rx: Receiver<Cmd>,
    tx: Sender<WorkerMsg>,
    /// 配音 / BGM / 音乐制作共用（它们走同一台 worker 的顺序队列）
    stop: Arc<AtomicBool>,
    /// 人声分离**单独**一个：与上面分开，避免两边互相把对方的停止请求吃掉
    /// （审查抓到过：分离运行中点配音停止，会让分离结果被当成"用户停止"丢掉）。
    sep_stop: Arc<AtomicBool>,
    /// 质检单独的停止位（与 sep_stop 同理：分开才不会互相吃掉停止请求）
    eval_stop: Arc<AtomicBool>,
    /// 工程根目录。生产走 `projects_root()`（`~/Documents/音频作坊/projects`）；
    /// 测试注入临时目录——`Cmd::Run` 会在 worker 内部按工程名拼路径，
    /// 没有这条缝就没法在不碰用户真实数据的前提下做 worker 级 e2e。
    projects_root: PathBuf,
    /// 排队任务的取消登记表（与 UI 线程共享同一张表）。
    /// 采纳自 gqf2008/Xmusic-splitter 的 per-job registry：取消按 task_id 定位，
    /// 执行方取走时摘除，表因此有界。
    cancel: cancel::CancelRegistry,
}

/// worker 取到一条任务时的统一入口：先如实回报"开始执行了"，再问它是不是已经
/// 在排队期间被取消。返回 true = 已被取消，调用方**不得执行**，直接按各自的
/// "停止"语义收尾；无论哪种结果，取消登记都在这里被摘除（表因此有界）。
fn task_take_started(ctx: &WorkerCtx, task_id: u32) -> bool {
    let _ = ctx.tx.send(WorkerMsg {
        revision: 0,
        msg: Msg::TaskStarted { task_id },
    });
    ctx.cancel.take(task_id)
}

/// 歌曲这类整段生成的任务没有中间进度：状态栏 chip 改报"已运行 N"，
/// 任务中心靠这条阶段文案 + 时长说明它没卡死。
fn song_stage_note(model_id: &str) -> &'static str {
    if model_id == "ace-step" {
        "正在请求服务端（ACE-Step 整段生成，无中间进度）"
    } else {
        "正在请求服务端（yue2 整段生成，无中间进度）"
    }
}

/// 客户端就绪后：**先**回报"正在请求服务端…"，再执行真正的请求。
///
/// 抽成函数是为了让单测能用假的 run 钉住顺序——真跑一遍 generate_song 会打服务端，
/// 测试里不能这么干；而"阶段必须晚于客户端就绪、早于请求"正是复核抓到过的那条。
fn with_song_stage<T>(ctx: &WorkerCtx, task_id: u32, model_id: &str, run: impl FnOnce() -> T) -> T {
    let _ = ctx.tx.send(WorkerMsg {
        revision: 0,
        msg: Msg::TaskStage {
            task_id,
            stage: song_stage_note(model_id).to_string(),
        },
    });
    run()
}

fn worker_loop(ctx: WorkerCtx) {
    // 当前工程：dir + project。Assemble/Redo 复用 Run 留下的那份。
    let mut current: Option<(u64, PathBuf, Project)> = None;
    while let Ok(cmd) = ctx.rx.recv() {
        match cmd {
            Cmd::OpenProject {
                revision,
                dir,
                project,
            } => {
                current = Some((revision, dir, project));
            }
            Cmd::InvalidateProject => {
                current = None;
            }
            Cmd::PreviewVoice {
                revision,
                model,
                voice_ref,
                voice_ref_text,
                text,
            } => {
                let msg = match make_client() {
                    Ok(client) => {
                        // 试听与成品必须走**同一次请求组装**（`aw_core` 里唯一的
                        // `build_synth_request` + 同一份参数白名单）：否则试听听到的
                        // 语气/音色不是成品的那一条，用户按试听选音色会被误导。
                        //
                        // 克隆同理按引擎给文本：audio8-tts 只发 voice_ref 就 500，
                        // index-tts2 不要文本（真机 200）。缺必填文本已在提交前拦
                        // （on_preview_voice 的 readiness 守卫），这里不再撞 500。
                        let outcome = match voice_ref.as_deref() {
                            Some(path) => match aw_core::VoiceClone::new(
                                path,
                                voice_ref_text.as_deref().unwrap_or_default(),
                            ) {
                                Ok(clone) => client.synth(
                                    &model,
                                    &text,
                                    Some(BASE_SEED),
                                    VoiceSource::Clone(clone),
                                ),
                                Err(e) => Err(e),
                            },
                            None => {
                                client.synth(&model, &text, Some(BASE_SEED), VoiceSource::BuiltIn)
                            }
                        };
                        match outcome {
                            Ok(wav) => Msg::VoicePreview {
                                wav,
                                label: model.clone(),
                            },
                            Err(e) => Msg::VoicePreviewFailed {
                                label: model.clone(),
                                error: e.to_string(),
                            },
                        }
                    }
                    Err(e) => Msg::VoicePreviewFailed {
                        label: model.clone(),
                        error: e,
                    },
                };
                let _ = ctx.tx.send(WorkerMsg { revision, msg });
            }
            Cmd::DesignVoice {
                revision,
                model,
                text,
                description,
            } => {
                let msg = match make_client() {
                    Ok(client) => {
                        let outcome =
                            client.synth(&model, &text, None, VoiceSource::Design(&description));
                        design_voice_msg(model, text, outcome)
                    }
                    Err(e) => Msg::DesignVoiceFailed { error: e, model },
                };
                let _ = ctx.tx.send(WorkerMsg { revision, msg });
            }
            Cmd::Run {
                revision,
                task_id,
                script,
                model,
                voice_ref,
                voice_ref_text,
                project_name,
                gap_ms,
                auto_normalize,
                dict,
                retry_failed,
            } => {
                if task_take_started(&ctx, task_id) {
                    // 排队期间被停掉：不载入、不合成，按"用户停止"收尾
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::RunDone {
                            failed: 0,
                            stopped: true,
                            reused: 0,
                            error: None,
                        },
                    });
                    continue;
                }
                let dir = ctx.projects_root.join(file_stem(&project_name));
                let loaded = match load_resumable(
                    &dir,
                    &script,
                    &model,
                    voice_ref,
                    voice_ref_text,
                    gap_ms,
                    auto_normalize,
                    &dict,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::Fatal(e),
                        });
                        continue;
                    }
                };
                let reused = loaded.reused;
                let mut project = loaded.project;
                let _ = ctx.tx.send(WorkerMsg {
                    revision,
                    msg: Msg::ProjectLoaded {
                        project: project.clone(),
                        reused,
                    },
                });
                let client = match make_client() {
                    Ok(c) => c,
                    Err(e) => {
                        current = Some((revision, dir, project));
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::Fatal(e),
                        });
                        continue;
                    }
                };
                // 用户释放内存后点「继续合成」：只喂上次失败的工程下标，done 句
                // 绝不重新请求。正常首次/续跑则 `None`，由 aw-core 跳过 done。
                let only_failed = if retry_failed {
                    Some(project.failed_sentence_indices())
                } else {
                    None
                };
                if matches!(only_failed.as_ref(), Some(v) if v.is_empty()) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::RunDone {
                            failed: 0,
                            stopped: false,
                            reused,
                            error: None,
                        },
                    });
                    current = Some((revision, dir, project));
                    continue;
                }
                let tx = ctx.tx.clone();
                let started = RefCell::new(std::collections::HashSet::new());
                let mut first_error: Option<String> = None;
                let run = project.synthesize_stoppable(
                    &client,
                    &dir,
                    only_failed.as_deref(),
                    Some(&ctx.stop),
                    |idx, note| {
                        // 优先把 OOM 留作整轮摘要；若第一条只是普通失败、
                        // 后面才有 OOM，也不能让可执行的内存提示被前面的错误盖掉。
                        let is_oom = note.starts_with("error: oom");
                        match first_error.as_deref() {
                            None if note.starts_with("error") => {
                                first_error = Some(note.to_string());
                            }
                            Some(existing) if is_oom && !existing.starts_with("error: oom") => {
                                first_error = Some(note.to_string());
                            }
                            _ => {}
                        }
                        report_progress(&tx, revision, &mut started.borrow_mut(), idx, note);
                    },
                );
                let stopped = ctx.stop.load(Ordering::Relaxed);
                current = Some((revision, dir, project));
                match run {
                    Ok(failed) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::RunDone {
                                failed,
                                stopped,
                                reused,
                                error: first_error,
                            },
                        });
                    }
                    Err(e) => {
                        // 落盘/解码等无法继续的失败不能伪装成 usize::MAX 个失败句。
                        // current 已保留，磁盘恢复后可直接重跑。
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::Fatal(format!("合成中止: {e}")),
                        });
                    }
                }
            }
            Cmd::UnloadModels => {
                let (ok, note) = match make_client() {
                    Ok(client) => match client.unload_all_models() {
                        Ok(note) => (true, note),
                        Err(e) => (false, e.to_string()),
                    },
                    Err(e) => (false, e),
                };
                let _ = ctx.tx.send(WorkerMsg {
                    revision: 0,
                    msg: Msg::ModelsUnloaded { ok, note },
                });
            }
            Cmd::RunBatch {
                revision,
                model,
                voice_ref,
                voice_ref_text,
                gap_ms,
                auto_normalize,
                dict,
                items,
            } => {
                let total = items.len();
                let mut done = 0usize;
                let mut failed = 0usize;
                let mut skipped = 0usize;
                // 整批中止（用户按了停止）。**不能 break**：剩下的篇目也要各回一条终态，
                // 否则任务台账里会永远挂着 Pending，`tasks_in_flight` 一直为真，
                // 用户之后连单篇都提交不了（没人再回报那几条）。
                let mut abort = false;
                for (index, item) in items.into_iter().enumerate() {
                    if abort || ctx.stop.load(Ordering::Relaxed) {
                        abort = true;
                        skipped += 1;
                        // 之前登记过"这条已取消"的要摘掉：登记表只在 worker 取走时收缩，
                        // 不摘就会随着"先取消几条、再停整批"越积越多
                        let _ = ctx.cancel.take(item.task_id);
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: batch_item_done(
                                item.task_id,
                                index,
                                item.name,
                                BatchItemOutcome::Skipped {
                                    note: Some("整批已停止：这篇没跑".into()),
                                },
                            ),
                        });
                        continue;
                    }
                    // 排队中就被取消的那篇：不加载、不合成，如实收尾（不消耗算力）
                    if task_take_started(&ctx, item.task_id) {
                        skipped += 1;
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: batch_item_done(
                                item.task_id,
                                index,
                                item.name,
                                BatchItemOutcome::Skipped { note: None },
                            ),
                        });
                        continue;
                    }
                    let dir = ctx.projects_root.join(file_stem(&item.name));
                    let loaded = match load_resumable(
                        &dir,
                        &item.script,
                        &model,
                        voice_ref.clone(),
                        voice_ref_text.clone(),
                        gap_ms,
                        auto_normalize,
                        &dict,
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            failed += 1;
                            let _ = ctx.tx.send(WorkerMsg {
                                revision: 0,
                                msg: batch_item_done(
                                    item.task_id,
                                    index,
                                    item.name,
                                    BatchItemOutcome::Failed {
                                        failed: 0,
                                        error: e,
                                        reused: 0,
                                    },
                                ),
                            });
                            continue;
                        }
                    };
                    let reused = loaded.reused;
                    let mut project = loaded.project;
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::BatchItemStarted {
                            task_id: item.task_id,
                            index,
                            total,
                            name: item.name.clone(),
                        },
                    });
                    let client = match make_client() {
                        Ok(c) => c,
                        Err(e) => {
                            failed += 1;
                            let _ = ctx.tx.send(WorkerMsg {
                                revision: 0,
                                msg: batch_item_done(
                                    item.task_id,
                                    index,
                                    item.name,
                                    BatchItemOutcome::Failed {
                                        failed: 0,
                                        error: e,
                                        reused,
                                    },
                                ),
                            });
                            continue;
                        }
                    };
                    // 句级进度：批量只用"第几/共几句"，所以走 BatchItemProgress，
                    // 不套单篇那套 Msg::Sentence（那套按 revision 过滤，会串台）
                    let sentences_total = project.sentences.len();
                    let counter = std::cell::Cell::new(0usize);
                    let progress_tx = ctx.tx.clone();
                    let task_id = item.task_id;
                    let stop = Arc::clone(&ctx.stop);
                    let run = project.synthesize_stoppable(
                        &client,
                        &dir,
                        None,
                        Some(&stop),
                        |_, note| {
                            if note.starts_with("done ") || note.starts_with("error") {
                                let now = counter.get() + 1;
                                counter.set(now);
                                let _ = progress_tx.send(WorkerMsg {
                                    revision: 0,
                                    msg: Msg::BatchItemProgress {
                                        task_id,
                                        index,
                                        done: now,
                                        total: sentences_total,
                                    },
                                });
                            }
                        },
                    );
                    let item_stopped = ctx.stop.load(Ordering::Relaxed);
                    let outcome = match run {
                        Ok(failed_sentences) => match project.assemble(&dir) {
                            Ok(a) => {
                                done += 1;
                                BatchItemOutcome::Done {
                                    wav: a.wav,
                                    srt: a.srt,
                                    failed: failed_sentences,
                                    reused,
                                }
                            }
                            Err(e) => {
                                failed += 1;
                                BatchItemOutcome::Failed {
                                    failed: failed_sentences,
                                    error: format!("拼装中止: {e}"),
                                    reused,
                                }
                            }
                        },
                        Err(e) => {
                            failed += 1;
                            // 彻底跑不动（落盘/保存失败）：这是"这篇失败"，不是"N 句失败"。
                            // 句数写 0，原因走 error——不要用 usize::MAX 这种哨兵值，
                            // 它一旦被谁当句数显示出来就是个假数字。
                            BatchItemOutcome::Failed {
                                failed: 0,
                                error: format!("合成中止: {e}"),
                                reused,
                            }
                        }
                    };
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: batch_item_done(task_id, index, item.name, outcome),
                    });
                    if item_stopped {
                        abort = true;
                    }
                }
                // 计数天然对得上：跑到哪算到哪，剩下的篇目在循环里被逐条记成 skipped
                debug_assert_eq!(done + failed + skipped, total);
                let _ = ctx.tx.send(WorkerMsg {
                    revision,
                    msg: Msg::BatchDone {
                        done,
                        failed,
                        skipped,
                        stopped: abort,
                    },
                });
            }
            Cmd::RunBgm {
                revision,
                task_id,
                prompt,
                duck_gain,
                standalone_seconds,
                dir,
            } => {
                if task_take_started(&ctx, task_id) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::BgmStopped { done: 0 },
                    });
                    continue;
                }
                // 有已载入工程就用它的目录（与配音同一份）；没有就用命令里带来的目录
                // （独立生成 BGM 不该因为"没跑过配音"而被拒）。
                let dir = match current.as_ref() {
                    Some((current_revision, loaded_dir, _)) => {
                        if *current_revision != revision {
                            let _ = ctx.tx.send(WorkerMsg {
                                revision,
                                msg: Msg::BgmFailed("工程版本已变更：先重新载入配音工程".into()),
                            });
                            continue;
                        }
                        loaded_dir.clone()
                    }
                    None => dir,
                };
                let dir = &dir;
                // 有配音成品 → 按它对齐并混音；没有（或读不出时长）→ 独立生成（只出 BGM 轨）。
                let dub_seconds = usable_voice_seconds(dir);
                let (target_seconds, mix) = match dub_seconds {
                    Some(v) => (v, true),
                    None => (
                        standalone_seconds
                            .filter(|v| *v > 0.0)
                            .unwrap_or(DEFAULT_BGM_STANDALONE_SECONDS),
                        false,
                    ),
                };
                let client = match make_client() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmFailed(e),
                        });
                        continue;
                    }
                };
                let options = BgmOptions {
                    prompt,
                    target_seconds,
                    duck_gain,
                    ..Default::default()
                };
                let tx = ctx.tx.clone();
                let stop = Arc::clone(&ctx.stop);
                let segments = match generate_segments_stoppable(
                    &client,
                    dir,
                    &options,
                    |done, total, note| {
                        let _ = tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmProgress { done, total },
                        });
                        let _ = note;
                    },
                    || stop.load(Ordering::Relaxed),
                ) {
                    Ok(BgmRun::Done(n)) => n,
                    Ok(BgmRun::Stopped(done)) => {
                        // 段间停止：不混音、不产出成品；已完成的分段留在目录里，
                        // 下次同 prompt 再生成时按 manifest 复用（manifest 未写成有效）。
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmStopped { done },
                        });
                        continue;
                    }
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmFailed(format!("BGM 生成失败: {e}")),
                        });
                        continue;
                    }
                };
                if let Err(e) = assemble_bgm(dir, &options) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::BgmFailed(format!("BGM 对齐失败: {e}")),
                    });
                    continue;
                }
                let finished = if mix {
                    mix_project(dir, &options)
                } else {
                    // 独立生成：只有 BGM 一轨（voice/mixed/srt 都是 None）
                    bgm_only_artifacts(dir, &options)
                };
                match finished {
                    Ok(artifacts) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmDone {
                                artifacts,
                                segments,
                                mixed: mix,
                            },
                        });
                    }
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmFailed(format!(
                                "{}失败: {e}",
                                if mix { "BGM 混音" } else { "BGM 收尾" }
                            )),
                        });
                    }
                }
            }
            Cmd::RunSeparation {
                revision: _revision,
                task_id,
                input,
                out_dir,
                stem,
                model_dir,
                chunk_seconds,
            } => {
                if task_take_started(&ctx, task_id) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::SeparationStopped { task_id },
                    });
                    continue;
                }
                let tx = ctx.tx.clone();
                // 读分离**自己的**停止位
                let stop = Arc::clone(&ctx.sep_stop);
                // 终态要带回“提交时”的输入与工程内输出目录：期间改工程名不能把
                // 成功记录写到另一个工程目录里。
                let history_input = input.clone();
                let history_out_dir = out_dir.clone();
                let req = aw_core::separate::SeparationRequest {
                    input,
                    out_dir,
                    stem,
                    model_dir,
                    chunk_seconds,
                };
                let progress_tx = tx.clone();
                let result = aw_core::separate::separate_tracks(
                    &req,
                    |p| {
                        let _ = progress_tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SeparationProgress {
                                task_id,
                                percent: p.percent,
                                note: p.note,
                            },
                        });
                    },
                    || stop.load(Ordering::Relaxed),
                );
                let msg = match result {
                    Ok(aw_core::separate::SeparationOutcome::Done(t)) => Msg::SeparationDone {
                        task_id,
                        input: history_input,
                        out_dir: history_out_dir,
                        vocals: t.vocals,
                        accompaniment: t.accompaniment,
                    },
                    Ok(aw_core::separate::SeparationOutcome::Stopped) => {
                        Msg::SeparationStopped { task_id }
                    }
                    Err(e) => Msg::SeparationFailed { task_id, error: e },
                };
                let _ = tx.send(WorkerMsg { revision: 0, msg });
            }
            Cmd::RunEval {
                // 质检的终态消息靠 task_id 自证身份，与 revision 无关
                revision: _revision,
                task_id,
                dir,
                model,
            } => {
                if task_take_started(&ctx, task_id) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::EvalStopped { task_id },
                    });
                    continue;
                }
                // 参考文本从工程里读：UI 再传一份就有两个真相源了
                let project = match Project::load_if_present(&dir) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::EvalFailed {
                                task_id,
                                error: "没有工程文件：先合成一轮再质检".into(),
                            },
                        });
                        continue;
                    }
                    Err(note) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::EvalFailed {
                                task_id,
                                error: note,
                            },
                        });
                        continue;
                    }
                };
                let client = match make_client() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::EvalFailed { task_id, error: e },
                        });
                        continue;
                    }
                };
                let done: Vec<usize> = project
                    .sentences
                    .iter()
                    .filter(|s| s.status == "done")
                    .map(|s| s.index)
                    .collect();
                if done.is_empty() {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::EvalFailed {
                            task_id,
                            error: "还没有已合成的句子：先合成再质检".into(),
                        },
                    });
                    continue;
                }
                let total = done.len();
                let mut project = project;
                let mut issues: Vec<EvalIssue> = Vec::new();
                // (句 index, 可懂度%, 这份分的来源模型)。用真模型 `model` 写下，
                // 不另算一份——报告与工程标记必须同源。
                let mut scores: Vec<(usize, f64, Option<String>)> = Vec::new();
                let mut rows: Vec<EvalRow> = Vec::new();
                let mut sum = 0.0f64;
                let mut scored = 0usize;
                let mut asr_failed = 0usize;
                let mut asr_error: Option<String> = None;
                let mut stopped = false;
                // 内存不足这类"整轮都不可能成功"的错误：记下来，跳出循环后带着可执行提示收尾
                let mut fatal: Option<String> = None;
                for (n, idx) in done.iter().enumerate() {
                    // 协作式停止：一句转写完再停（ASR 调用本身中断不了）
                    if ctx.eval_stop.load(Ordering::Relaxed) {
                        stopped = true;
                        break;
                    }
                    let wav = dir.join(format!("sentences/{idx:03}.wav"));
                    match client.asr_with(&model, &wav) {
                        Ok(hypothesis) => {
                            // clone 一份参考文本：下面还要 mut 借 project.sentences 写分数
                            let reference = project
                                .sentences
                                .iter()
                                .find(|s| s.index == *idx)
                                .map(|s| s.text.clone())
                                .unwrap_or_default();
                            let score = aw_core::intelligibility(&reference, &hypothesis);
                            let snippet =
                                aw_core::diff_snippet(&reference, &hypothesis).unwrap_or_default();
                            sum += score.percent;
                            scored += 1;
                            scores.push((*idx, score.percent, Some(model.clone())));
                            rows.push(EvalRow {
                                index: *idx,
                                reference: reference.clone(),
                                hypothesis: hypothesis.clone(),
                                percent: score.percent,
                                snippet: snippet.clone(),
                            });
                            // 顺手写进工程：质检结果要能跨会话留存（否则每次开都要重跑 N 句 ASR）
                            // 分数与来源模型**同一处写入**（`record_eval_score`）——
                            // 报告念的也是同一个 `model` 变量，两边不会各说一套。
                            record_eval_score(&mut project, *idx, score.percent, &model);
                            // 只收"有差异"的句子：worst 为空就等于全部一致
                            // （否则满分句也会被列成"最差 第 1 句 100%"，复核指出过）
                            if score.distance > 0 {
                                issues.push(EvalIssue {
                                    index: *idx,
                                    percent: score.percent,
                                    snippet,
                                });
                            }
                        }
                        Err(e) => {
                            // 内存不足不是"这一句没测到"：模型根本加载不了，后面每一句都会
                            // 同样失败，继续跑只会让用户白等。判据在 `classify_asr_failure`
                            // （识别口径与用例同一份），这里只执行结论。
                            match classify_asr_failure(
                                &model,
                                &e,
                                &asr_candidate_weights(read_server_config().as_ref()),
                            ) {
                                EvalAsrFailure::Fatal(error) => {
                                    fatal = Some(error);
                                    break;
                                }
                                // 其余单句转写失败不致命：记数并在汇总里如实报出来，不混进平均分。
                                // 这句的旧分数（如果有）**保留**——它描述的是磁盘上那段音频，
                                // 而这次只是没测到；保留的分数要一起回给 UI，否则 UI 与磁盘不一致。
                                EvalAsrFailure::Counted => {
                                    asr_failed += 1;
                                    // 保留第一条服务端原文（含 OOM 三个动作）；后续同一
                                    // 错误不再重复堆积，摘要只展示一份可执行说明。
                                    if asr_error.is_none() {
                                        asr_error = Some(e.to_string());
                                    }
                                }
                            }
                        }
                    }
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::EvalProgress {
                            task_id,
                            done: n + 1,
                            total,
                            model: model.clone(),
                        },
                    });
                }
                if let Some(error) = fatal {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::EvalFailed { task_id, error },
                    });
                    continue;
                }
                if stopped {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::EvalStopped { task_id },
                    });
                    continue;
                }
                // 把"这次没评上、但旧分仍在工程里"的句子也纳入 scores：UI 之后是整体替换，
                // 少给就会让磁盘有分、界面没分（复核抓到的不一致）。
                // 来源规则（必须带**当年的**模型，不是本轮这个）在 `carried_over_scores` 里，
                // 那里有隔离用例。
                let carried = carried_over_scores(&project, &scores);
                scores.extend(carried);
                // 分数落盘：失败不算质检失败（分数本身有效），但要如实报出来
                let saved = project.save(&dir);
                let mut warnings: Vec<String> = Vec::new();
                if let Some(e) = saved.as_ref().err() {
                    warnings.push(format!("分数未写入工程：{e}"));
                }
                // 质检报告：逐句「参考 vs 回读」只有当场拿得到（分数虽然留在工程里，
                // 但 ASR 原文不存）——写成文件供用户存档/对比。
                let project_name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "未命名工程".to_string());
                let report = qa_report_markdown(
                    &project_name,
                    &model,
                    &rows,
                    if scored == 0 {
                        0.0
                    } else {
                        sum / scored as f64
                    },
                    scored,
                    asr_failed,
                );
                let report_file = dir.join("qa-report.md");
                let report_path =
                    match aw_core::dub::write_atomic_explained(&report_file, report.as_bytes()) {
                        Ok(()) => Some(report_file),
                        Err(e) => {
                            warnings.push(format!("质检报告未写入：{e}"));
                            None
                        }
                    };
                let persist_warning = if warnings.is_empty() {
                    None
                } else {
                    Some(warnings.join("·"))
                };
                // 落盘成功后把 worker 手里的 current 一起更新：否则后续 Redo/Assemble
                // 用旧 Project 再 save 一次，会把刚写下的分数刷掉（复核抓到的阻塞项）
                if saved.is_ok() {
                    if let Some((_rev, cur_dir, cur_project)) = current.as_mut() {
                        if *cur_dir == dir {
                            *cur_project = project.clone();
                        }
                    }
                }
                issues.sort_by(|a, b| a.percent.partial_cmp(&b.percent).unwrap());
                issues.truncate(3);
                let percent = if scored == 0 {
                    0.0
                } else {
                    sum / scored as f64
                };
                let _ = ctx.tx.send(WorkerMsg {
                    revision: 0,
                    msg: Msg::EvalDone {
                        task_id,
                        summary: EvalSummary {
                            model: model.clone(),
                            percent,
                            scored,
                            asr_failed,
                            asr_error,
                            worst: issues,
                            scores,
                            persist_warning,
                            report_path,
                        },
                    },
                });
            }
            Cmd::RunSong {
                // 歌曲的终态消息靠 task_id 自证身份（见 Msg::SongDone），不需要 revision
                revision: _revision,
                task_id,
                project_name,
                model,
                lyrics,
                style,
            } => {
                if task_take_started(&ctx, task_id) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::SongStopped { task_id },
                    });
                    continue;
                }
                let dir = ctx
                    .projects_root
                    .join(file_stem(&project_name))
                    .join("song");
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::SongFailed {
                            task_id,
                            error: e.to_string(),
                        },
                    });
                    continue;
                }
                let client = match make_client() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SongFailed { task_id, error: e },
                        });
                        continue;
                    }
                };
                let model_id = model.clone();
                // 未知 gen 引擎**显式失败**，不静默当 yue2 用（见 song_model_for_id）
                let model = match song_model_for_id(&model_id) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SongFailed { task_id, error: e },
                        });
                        continue;
                    }
                };
                let options = SongOptions {
                    model,
                    lyrics,
                    style,
                    ..Default::default()
                };
                // 目录与客户端都已就绪，下面这一下才是真的发请求：这时才回报阶段
                // （复核指出：早于 create_dir_all / make_client 回报会在失败时报假进度）
                let outcome = with_song_stage(&ctx, task_id, &model_id, || {
                    generate_song(&client, &dir, "song", &options)
                });
                match outcome {
                    Ok(path) => {
                        let duration = std::fs::read(&path)
                            .ok()
                            .and_then(|bytes| aw_core::dub::wav_duration(&bytes).ok())
                            .unwrap_or(0.0);
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SongDone {
                                task_id,
                                path,
                                duration,
                            },
                        });
                    }
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SongFailed {
                                task_id,
                                error: format!("歌曲生成失败: {e}"),
                            },
                        });
                    }
                }
            }
            Cmd::RunCover {
                // 翻唱与 RunSong 共用 task_id + SongDone/Stopped/Failed 终态（不另造通道）
                revision: _revision,
                task_id,
                project_name,
                lyrics,
                style,
                source_audio,
            } => {
                if task_take_started(&ctx, task_id) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::SongStopped { task_id },
                    });
                    continue;
                }
                if !source_audio.is_file() {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::SongFailed {
                            task_id,
                            error: format!("源音频不存在或不可读：{}", source_audio.display()),
                        },
                    });
                    continue;
                }
                let dir = ctx
                    .projects_root
                    .join(file_stem(&project_name))
                    .join("song");
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::SongFailed {
                            task_id,
                            error: e.to_string(),
                        },
                    });
                    continue;
                }
                let client = match make_client() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SongFailed { task_id, error: e },
                        });
                        continue;
                    }
                };
                let options = SongOptions {
                    model: SongModel::Yue2,
                    lyrics,
                    style,
                    ..Default::default()
                };
                // 翻唱固定 yue2：generate_cover 内部会再强制一次（源音频 → ABC → yue2 cot=melody）
                let outcome = with_song_stage(&ctx, task_id, "yue2", || {
                    generate_cover(&client, &dir, &source_audio, "cover", &options)
                });
                match outcome {
                    Ok(path) => {
                        let duration = std::fs::read(&path)
                            .ok()
                            .and_then(|bytes| aw_core::dub::wav_duration(&bytes).ok())
                            .unwrap_or(0.0);
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SongDone {
                                task_id,
                                path,
                                duration,
                            },
                        });
                    }
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision: 0,
                            msg: Msg::SongFailed {
                                task_id,
                                error: format!("翻唱失败: {e}"),
                            },
                        });
                    }
                }
            }
            Cmd::Redo { revision, index } => {
                let Some((current_revision, dir, project)) = current.as_mut() else {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::RedoDone {
                            index,
                            error: Some("工程已变更：先开始合成再重录单句".into()),
                        },
                    });
                    continue;
                };
                if *current_revision != revision {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::RedoDone {
                            index,
                            error: Some("工程版本已变更：先开始合成再重录单句".into()),
                        },
                    });
                    continue;
                }
                // 重录的 voice_ref 在**工程里**（UI 路径只是它当时的投影），所以请求
                // 前按工程的 voice_ref 走迁移/裁剪（migrate_overlong_voice_ref）：
                // load_resumable 已迁过 Run 装入的工程，这里兜 OpenProject（启动恢复）
                // 灌进来的——规则不变：请求实际使用 ≤15s 的路径。裁剪失败按红字拦截，
                // 不发起任何请求（发出去就会把引擎进程打死）。
                if let Err(note) = migrate_overlong_voice_ref(dir, project) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::RedoDone {
                            index,
                            error: Some(note),
                        },
                    });
                    continue;
                }
                let client = match make_client() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::RedoDone {
                                index,
                                error: Some(e),
                            },
                        });
                        continue;
                    }
                };
                let tx = ctx.tx.clone();
                let started = RefCell::new(std::collections::HashSet::new());
                match project.redo(
                    &client,
                    dir,
                    index,
                    None,
                    |t| aw_core::normalize(t, &Default::default()),
                    |idx, note| {
                        report_progress(&tx, revision, &mut started.borrow_mut(), idx, note);
                    },
                ) {
                    Ok(n) => {
                        let error = if n > 0 {
                            Some(format!("第 {} 句重录失败（见句子状态，可再试）", index + 1))
                        } else {
                            None
                        };
                        let _ = tx.send(WorkerMsg {
                            revision,
                            msg: Msg::RedoDone { index, error },
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerMsg {
                            revision,
                            msg: Msg::RedoDone {
                                index,
                                error: Some(format!("重录失败: {e}")),
                            },
                        });
                    }
                }
            }
            Cmd::Assemble { revision, gap_ms } => {
                let Some((current_revision, dir, project)) = current.as_mut() else {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::AssembleFailed("工程已变更：先开始合成再导出".into()),
                    });
                    continue;
                };
                if *current_revision != revision {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::AssembleFailed("工程版本已变更：先开始合成再导出".into()),
                    });
                    continue;
                }
                // 停顿是拼装参数：这里应用当前值（改了停顿的用户点「导出」即可生效）。
                // **先存副本、成功再改内存**：直接改字段的话，save 失败会留下
                // "内存里 300、project.json 里 250"，下一次同值调用因为字段已相等而跳过
                // save，最后拼出与工程记录不一致的成品。
                if project.gap_ms != gap_ms {
                    let mut updated = project.clone();
                    updated.gap_ms = gap_ms;
                    if let Err(e) = updated.save(dir) {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::AssembleFailed(format!("保存工程失败: {e}")),
                        });
                        continue;
                    }
                    project.gap_ms = gap_ms;
                }
                match project.assemble(dir) {
                    Ok(a) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::Assembled {
                                wav: a.wav,
                                srt: a.srt,
                                duration: a.duration,
                                done: a.done,
                                skipped: a.skipped,
                            },
                        });
                    }
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::AssembleFailed(format!("拼装中止: {e}")),
                        });
                    }
                }
            }
        }
    }
}

/// 批量里一条稿子的终态（四个出口共用：跳过 / 加载失败 / 合成失败 / 跑完）。
///
/// 抽出来是因为 `BatchItemDone` 字段多：四条路径各写一遍字段顺序，早晚会有一条
/// 把 `failed` 写到 `skipped` 上——而这两种"没跑成"在界面上是完全不同的两件事。
enum BatchItemOutcome {
    Done {
        wav: PathBuf,
        srt: PathBuf,
        failed: usize,
        reused: usize,
    },
    Failed {
        failed: usize,
        error: String,
        reused: usize,
    },
    /// 没跑的那篇：`note` 说清为什么没跑（排队中被取消 / 整批被停止）
    Skipped { note: Option<String> },
}

fn batch_item_done(task_id: u32, index: usize, name: String, outcome: BatchItemOutcome) -> Msg {
    match outcome {
        BatchItemOutcome::Done {
            wav,
            srt,
            failed,
            reused,
        } => Msg::BatchItemDone {
            task_id,
            index,
            name,
            wav: Some(wav),
            srt: Some(srt),
            failed,
            reused,
            skipped: false,
            error: None,
            note: None,
        },
        BatchItemOutcome::Failed {
            failed,
            error,
            reused,
        } => Msg::BatchItemDone {
            task_id,
            index,
            name,
            wav: None,
            srt: None,
            failed,
            reused,
            skipped: false,
            error: Some(error),
            note: None,
        },
        BatchItemOutcome::Skipped { note } => Msg::BatchItemDone {
            task_id,
            index,
            name,
            wav: None,
            srt: None,
            failed: 0,
            reused: 0,
            skipped: true,
            error: None,
            note,
        },
    }
}

/// progress 回调 → Msg::Sentence。用 started 集区分「句首回调（note=spoken）」
/// 与「结果回调（done/error）」——不能靠文本前缀判断：spoken 本身可能以
/// "done"/"error" 开头。
fn report_progress(
    tx: &Sender<WorkerMsg>,
    revision: u64,
    started: &mut std::collections::HashSet<usize>,
    idx: usize,
    note: &str,
) {
    if idx == usize::MAX || note == "stopped" {
        return; // 控制消息，不映射句子状态
    }
    if !started.insert(idx) {
        let (status, duration) = if let Some(rest) = note.strip_prefix("done ") {
            let d = rest.trim_end_matches('s').parse::<f64>().ok();
            ("done".to_string(), d)
        } else if note.starts_with("error") {
            // 终态 note 就是 aw-core 落盘的完整句状态（含 `error: oom` 与
            // ClientError 的唯一可执行文案）；原样带给 UI，不在这里二次拼提示。
            (note.to_string(), None)
        } else {
            ("running".to_string(), None)
        };
        let _ = tx.send(WorkerMsg {
            revision,
            msg: Msg::Sentence {
                index: idx,
                status,
                duration,
            },
        });
    } else {
        let _ = tx.send(WorkerMsg {
            revision,
            msg: Msg::Sentence {
                index: idx,
                status: "running".into(),
                duration: None,
            },
        });
    }
}

fn make_client() -> Result<Client, String> {
    let (_, base, note) = discover_engine();
    let base = base.ok_or_else(|| format!("没有服务地址。{note}"))?;
    Ok(Client::new(base))
}

/// 应用数据目录：~/Documents/音频作坊/
///
/// 工程、设置、模板、词典库、音色库都在这一层——也就是一键备份要打包的全部内容
/// （见 `src/backup.rs` 的 `ITEMS`，两边必须同源）。
fn workshop_dir() -> PathBuf {
    documents_dir().join(WORKSHOP_DIR)
}

/// 工程目录：~/Documents/音频作坊/projects/<stem>/
fn projects_root() -> PathBuf {
    workshop_dir().join("projects")
}

fn project_dir(stem: &str) -> PathBuf {
    projects_root().join(stem)
}

/// 音色设计产物的落点：~/Documents/音频作坊/voice-design/（与应用设置、导出目录同级）。
fn voice_design_dir() -> PathBuf {
    workshop_dir().join("voice-design")
}

/// 超长参考音频的裁剪副本目录：~/Documents/音频作坊/voice-trimmed/（应用自管目录，
/// 用户原文件绝不改动）。
fn voice_trimmed_dir() -> PathBuf {
    // cfg(test) 注入缝：单测不得往用户真实数据目录写裁剪副本（见
    // `LESSON_单测不得写用户真实运行数据须拆出注入缝`）——整个测试进程共用临时
    // 目录里的一份（OnceLock，跨 worker 线程可见，无并发改值竞态）。
    #[cfg(test)]
    {
        use std::sync::OnceLock;
        static OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
        OVERRIDE
            .get_or_init(|| {
                let dir = std::env::temp_dir().join(format!(
                    "audio-workshop-test-voice-trimmed-{}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&dir).expect("建测试 voice-trimmed 目录失败");
                dir
            })
            .clone()
    }
    #[cfg(not(test))]
    {
        workshop_dir().join("voice-trimmed")
    }
}

/// 参考音频在**发起克隆请求前**的应用侧唯一入口：超长 → voice-trimmed 副本，
/// 原样/读不出 → 原路径（fail-open，取舍见 `aw_core::ref_audio` 模块注释）。
///
/// 四个会发克隆请求的入口（开始合成 / 批量提交 / 单句重录 / 音色试听）都用它返回
/// 的路径发请求——规则只有一条：**请求实际使用 ≤15s 的路径**。旧工程的 voice_ref
/// 迁移（load_resumable / Cmd::Redo 分支）也走这里（见 [`migrate_overlong_voice_ref`]）。
///
/// 故意不带 ui 参数：worker 侧（load_resumable / Cmd::Redo）也要用，而 ui 只能由
/// UI 线程碰——信息提示（`note`）由调用点在能碰 ui 的地方就地 set_status_text。
///
/// 源码守卫 `every_clone_request_entrance_uses_prepared_reference` 钉住调用点，
/// 加新入口必须同步加。
#[derive(Debug)]
struct PreparedReference {
    /// 本次请求该用的参考音频路径（原路径或 voice-trimmed 副本）。
    path: PathBuf,
    /// 超长被裁剪时的信息提示（状态行用）；原样使用/读不出时长为 None。
    note: Option<String>,
}

fn prepare_reference_for_clone(
    path: &str,
    reference_text: Option<&str>,
) -> Result<PreparedReference, String> {
    let src = Path::new(path);
    let Some(secs) = aw_core::reference_duration_seconds(src) else {
        // 时长读不出 → 原样返回（fail-open：最坏结果与没有护栏一样，取舍见
        // aw_core::ref_audio 模块注释——不把"探测坏了"放大成"克隆不可用"）。
        return Ok(PreparedReference {
            path: src.to_path_buf(),
            note: None,
        });
    };
    if secs <= aw_core::REFERENCE_MAX_SECONDS {
        // 已经是被 0.1.10 裁过的缓存副本（工程迁移后 voice_ref 就是它）：当时没有旁车，
        // 而它不会再触发裁剪 → 必须在这里补上，否则「84 字音频 × 1077 字文本」的错配
        // 会一直留着（用户实测的坏声音就是这么来的）。
        if src.parent() == Some(voice_trimmed_dir().as_path()) {
            ensure_trimmed_reference_text_sidecar(src, secs, reference_text);
        }
        return Ok(PreparedReference {
            path: src.to_path_buf(),
            note: None,
        });
    }
    match aw_core::trim_reference_first_seconds(src, &voice_trimmed_dir(), aw_core::REFERENCE_MAX_SECONDS) {
        Ok(trimmed) => {
            ensure_trimmed_reference_text_sidecar(&trimmed, secs, reference_text);
            Ok(PreparedReference {
                path: trimmed,
                note: Some(format!(
                    "参考音频 {secs:.1} 秒 → 将使用前 {:.0} 秒（参考文本已同步为对应片段，原文件未改动）",
                    aw_core::REFERENCE_MAX_SECONDS
                )),
            })
        }
        // 裁剪失败（解码/写盘失败）→ 退回红色拦截文案：15s 上限 + 手动处理建议，
        // 并说清「自动裁剪失败」的原因。
        Err(reason) => Err(format!(
            "参考音频 {secs:.1} 秒，超过 {:.0} 秒上限，自动取前 {:.0} 秒失败（{reason}）。请手动裁到 {:.0} 秒内再合成。",
            aw_core::REFERENCE_MAX_SECONDS, aw_core::REFERENCE_MAX_SECONDS, aw_core::REFERENCE_MAX_SECONDS
        )),
    }
}

/// 裁剪副本的「参考文本」旁车：优先 ASR 转写裁剪段，失败退回按比例截断用户文本。
///
/// 为什么必须有：裁剪只改音频不改文本时，audio8-tts 会收到「84 字音频 + 1077 字
/// 文本」的错配，克隆出完全不对的声音（v0.1.10 用户实测）。旁车只在裁剪时写，
/// 请求组装（aw-core `build_synth_request`）自动优先读取，用户工程里的原文不动。
fn ensure_trimmed_reference_text_sidecar(
    trimmed: &Path,
    full_secs: f64,
    reference_text: Option<&str>,
) {
    if aw_core::ref_audio::paired_reference_text(trimmed).is_some() {
        return;
    }
    let Some(user_text) = reference_text.map(str::trim).filter(|t| !t.is_empty()) else {
        // 用户没给参考文本（如 index-tts2）：不造一个出来，按引擎原语义走
        return;
    };
    // 1）ASR 真读一遍裁剪段（最贴音频内容）；引擎没有 ASR 模型/不可用 → None
    let asr = make_client()
        .ok()
        .and_then(|c| c.asr(trimmed).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    // 2）兜底：同一段录音语速大致均匀，按保留比例截断用户文本（零依赖）；
    //    原时长不可知（已迁移的缓存副本）时按估算语速截断。
    let text = asr.unwrap_or_else(|| {
        if full_secs > aw_core::REFERENCE_MAX_SECONDS {
            aw_core::ref_audio::proportional_reference_text(
                user_text,
                aw_core::REFERENCE_MAX_SECONDS,
                full_secs,
            )
        } else {
            aw_core::ref_audio::estimated_reference_text(user_text, full_secs)
        }
    });
    if let Err(e) = aw_core::ref_audio::write_reference_text_sidecar(trimmed, &text) {
        // 旁车写不进去不阻塞克隆（最坏退回旧行为），但要留下诊断信息
        eprintln!("参考文本旁车写入失败（不阻塞克隆）：{e}");
    }
}

/// 旧工程迁移：工程里存的 voice_ref 超过 15s → 换成 voice-trimmed 里的裁剪副本，
/// 并更新 voice_ref_hash 一起落盘（**显式迁移**，不是"下次提交时顺手改"）。
///
/// 为什么必须落盘：project.json 里存着超长路径的话（例如真机打崩引擎的 192.9s
/// 那条），`Cmd::Redo` 这类由 worker 直接用工程 voice_ref 发请求的路径仍会拿超长
/// 文件打引擎；只有把迁移写回 project.json，之后所有读工程的路径才都拿到 ≤15s
/// 的路径。读不出时长按 fail-open 跳过（与入口同口径）。
///
/// 返回 Ok(Some(note)) = 迁移过（信息提示可交给用户），Ok(None) = 无需迁移，
/// Err = 自动裁剪失败（调用方把文案交给用户并按红字拦截）。
fn migrate_overlong_voice_ref(
    dir: &Path,
    project: &mut aw_core::Project,
) -> Result<Option<String>, String> {
    let Some(path) = project.voice_ref.clone() else {
        return Ok(None);
    };
    let prepared = prepare_reference_for_clone(&path, project.voice_ref_text.as_deref())?;
    if prepared.path == Path::new(&path) {
        return Ok(None);
    }
    // 先把新 hash 算出来再一次性改两个字段：hash 读失败时内存态不能先变成新路径
    // （否则会留下「内存已迁移、磁盘没迁移」两套状态；2026-09-22 复评 M2）。
    let new_path = prepared.path.to_string_lossy().into_owned();
    let new_hash = sha256_file(&prepared.path)?;
    project.voice_ref = Some(new_path);
    project.voice_ref_hash = Some(new_hash);
    project
        .save(dir)
        .map_err(|e| format!("工程落盘失败: {e}"))?;
    Ok(prepared.note)
}

/// 把生成的音色设计 wav 落盘，返回完整路径。
///
/// 文件名带毫秒时间戳：每次生成都是**新文件**。不能写死一个名字——用户先"用作配音
/// 音色"把 voice_ref 指到它，再重新设计一次，写死名字会把已经指着的参考音频覆盖掉。
fn save_design_voice(wav: &[u8]) -> Result<PathBuf, String> {
    let dir = voice_design_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}", e))?;
    let path = dir.join(format!("voice-design-{}.wav", now_ms()));
    std::fs::write(&path, wav).map_err(|e| format!("{}", e))?;
    Ok(path)
}

#[derive(Debug)]
struct LoadedProject {
    project: Project,
    /// 新稿件中按文本继承的已合成句数（音色/模型不变时才可能 >0）。
    reused: usize,
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use std::fmt::Write as _;

    let bytes =
        std::fs::read(path).map_err(|e| format!("参考音频不可读（{}）: {e}", path.display()))?;
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

fn voice_ref_matches(
    saved: &Project,
    voice_ref: &Option<String>,
    voice_ref_hash: &Option<String>,
    voice_ref_text: &Option<String>,
) -> bool {
    saved.voice_ref.as_ref() == voice_ref.as_ref()
        && saved.voice_ref_hash.as_ref() == voice_ref_hash.as_ref()
        // 参考文本也是音色的一部分：同一段音频换个转写文本 = 另一个声音。
        // 少了这一条，"让用户确认/修正 ASR 转写"就成了空转——改完文本点开始合成，
        // 句子全是 done 直接被复用，用户听到的还是旧转写合成的声音。
        && saved.voice_ref_text.as_ref() == voice_ref_text.as_ref()
}

/// "旧工程的音频能不能给这份新设置复用"的判据：**模型 / 兜底开关 / 参考音
/// （路径 + 内容哈希 + 参考文本）全一致**。停顿（gap_ms）只影响拼装，不算在内。
///
/// worker 的 `load_resumable`（续作/改稿继承）与版本回滚的"按文本继承"**共用这一条**：
/// 两处各写一遍的话，回滚就可能把 B 模型合成的音频标成"版本显示 A"的已合成
/// （复核给的反例）。
fn settings_allow_reuse(
    saved: &Project,
    model: &str,
    voice_ref: &Option<String>,
    voice_ref_hash: &Option<String>,
    voice_ref_text: &Option<String>,
    auto_normalize: bool,
    dict_fingerprint: &str,
) -> bool {
    saved.model == model
        && saved.auto_normalize == auto_normalize
        // 词典与兜底开关同类：它改的是 spoken 文本，换词典就不能复用旧音频。
        // 旧工程/没启用时 `dict_hash` 是 None —— 按"空词典"处理，与当前空词典等价。
        && effective_dict_hash(saved) == dict_fingerprint
        && voice_ref_matches(saved, voice_ref, voice_ref_hash, voice_ref_text)
}

/// 工程记录的词典指纹：None（旧工程 / 从没启用过）等价于"空词典"。
fn effective_dict_hash(saved: &Project) -> String {
    saved
        .dict_hash
        .clone()
        .unwrap_or_else(|| dictionaries::fingerprint(&Default::default()))
}

/// 旁车修复判定：修复前缺失 + 修复后出现 = 需要作废旧音频（重录一遍）。
///
/// 拆成纯函数是为了可测：`load_resumable` 里的 `voice-trimmed/` 路径依赖应用数据目录，
/// 不适合单测，这两条布尔才是真正的判据。
fn reference_text_was_repaired(before_missing: bool, now_present: bool) -> bool {
    before_missing && now_present
}

fn sentence_texts_match(project: &Project, script: &str) -> bool {
    project
        .sentences
        .iter()
        .map(|s| s.text.as_str())
        .eq(
            aw_core::split_sentences(script, DEFAULT_PUNCTUATION, MAX_CHARS)
                .iter()
                .map(|s| s.as_str()),
        )
}

struct ReuseTemp(PathBuf);

impl Drop for ReuseTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct StagedReuse {
    new_index: usize,
    temp: ReuseTemp,
    dst: PathBuf,
    old_sentence: aw_core::Sentence,
}

/// 稿件变化时按“未变句文本”继承旧工程的 done 音频；新下标先全部暂存，最后统一
/// rename，避免编辑/重排后边复制边覆盖仍会被后续句子使用的源文件。
fn reuse_done_sentences(new: &mut Project, old: &Project, dir: &Path) -> Result<usize, String> {
    let mut available: HashMap<String, VecDeque<aw_core::Sentence>> = HashMap::new();
    for sentence in &old.sentences {
        if sentence.status == "done" {
            available
                .entry(sentence.text.clone())
                .or_default()
                .push_back(sentence.clone());
        }
    }

    let sentences_dir = dir.join("sentences");
    std::fs::create_dir_all(&sentences_dir)
        .map_err(|e| format!("建逐句目录失败（{}）: {e}", sentences_dir.display()))?;
    let mut staged: Vec<StagedReuse> = Vec::new();
    for i in 0..new.sentences.len() {
        let text = new.sentences[i].text.clone();
        let Some(old_sentence) = available.get_mut(&text).and_then(VecDeque::pop_front) else {
            continue;
        };
        let src = sentences_dir.join(format!("{:03}.wav", old_sentence.index));
        if !src.is_file() {
            continue;
        }
        let dst = sentences_dir.join(format!("{:03}.wav", new.sentences[i].index));
        let temp = ReuseTemp(sentences_dir.join(format!(
            ".reuse-{:03}.wav.tmp{}",
            new.sentences[i].index,
            std::process::id()
        )));
        // 复用 = 把旧工程的 wav 复制进新工程：失败文案要说清是哪一句、哪个文件、做什么
        std::fs::copy(&src, &temp.0).map_err(|e| {
            format!(
                "复用第 {} 句失败：{}",
                old_sentence.index,
                aw_core::dub::write_failure_note(&temp.0, 0, &e)
            )
        })?;
        aw_core::dub::sync_file(&temp.0).map_err(|e| {
            format!(
                "复用第 {} 句落盘失败：{}",
                old_sentence.index,
                aw_core::dub::write_failure_note(&temp.0, 0, &e)
            )
        })?;
        staged.push(StagedReuse {
            new_index: i,
            temp,
            dst,
            old_sentence,
        });
    }

    let mut reused = 0;
    for staged in staged {
        std::fs::rename(&staged.temp.0, &staged.dst).map_err(|e| {
            format!(
                "复用句落盘失败：{}",
                aw_core::dub::write_failure_note(&staged.dst, 0, &e)
            )
        })?;
        let sentence = &mut new.sentences[staged.new_index];
        sentence.seed = staged.old_sentence.seed;
        sentence.duration = staged.old_sentence.duration;
        sentence.start = None;
        sentence.status = "done".into();
        reused += 1;
    }
    Ok(reused)
}

/// 恢复或新建工程：模型与音色一致且句子文本完全一致 → 原样续作；
/// 稿件变化时按未变句文本继承 done 音频；模型/音色变化则整工程重录。
/// 按调用方给的输入造一份新工程。
///
/// **唯一入口**：worker 的 `load_resumable`（造新工程）与版本留档（快照当前界面）
/// 都走它——两处各写一遍的话，"界面留档的工程"与"实际合成的工程"迟早不是同一份。
fn new_project_from_inputs(
    script: &str,
    model: &str,
    voice_ref: Option<String>,
    voice_ref_text: Option<String>,
    gap_ms: u64,
    auto_normalize: bool,
    dict: &std::collections::BTreeMap<String, String>,
) -> Project {
    let mut project = Project::new(
        script,
        model,
        gap_ms,
        BASE_SEED,
        voice_ref.clone(),
        DEFAULT_PUNCTUATION,
        MAX_CHARS,
        |t| {
            if auto_normalize {
                // 数字/年份规范化**会顺带应用词典**（aw_core::normalize 的实现就是先词典后规则）
                aw_core::normalize(t, dict)
            } else {
                // 关掉兜底 ≠ 关掉词典：词典是"这个词该怎么念"，与数字规则是两件事
                aw_core::apply_dictionary(t, dict)
            }
        },
    );
    project.auto_normalize = auto_normalize;
    project.dict_hash = Some(dictionaries::fingerprint(dict));
    // 参考文本与参考音**同进同出**：没有参考音就不该留下孤立的文本（否则"已切回内置音色"
    // 之后旧文本还在，下次选回同一段音频会静默沿用上一次的转写）。
    project.voice_ref_text = match project.voice_ref {
        Some(_) => voice_ref_text,
        None => None,
    };
    // 参考音哈希**不在这里算**：`load_resumable` 已经算过一份（算两遍纯属浪费），
    // 版本留档那边由 `project_from_ui` 自己补。这里只管"按输入造工程"。
    project
}

/// 参考文本只属于**当前这一段**参考音；路径一换，它就作废。
///
/// 返回 true = 文本仍然有效（还是同一段音频），不用清。
///
/// 为什么"必须清"不是洁癖（复核真机复现的静默缺陷）：文本非空 ⇒
/// `reference_text_missing`（本文件下方）拦不住；而 `settings_allow_reuse` 见
/// `voice_ref` 变了会**重录** ⇒ 于是**拿 A 音频的转写去条件 B 音频**。
/// 服务端不报错（正确文本 273241B / 完全无关文本 289628B —— 产物已经变了），
/// 用户只会觉得"换了音频但音色怪怪的"。
fn keeps_reference_text(old_path: &str, new_path: &str) -> bool {
    let old = old_path.trim();
    !old.is_empty() && old == new_path.trim()
}

/// 清掉参考文本与它的状态行。**唯一入口**：清除参考音、换参考音、手改路径都走它。
fn clear_reference_text(ui: &MainWindow) {
    ui.set_voice_ref_text("".into());
    ui.set_voice_ref_text_status("".into());
}

/// 换参考音频：写路径，并**在路径真的变了的时候**把名下的文本一起清掉。
///
/// 这是全文件**唯一一处**写 `voice-ref-path` 的地方 —— 源码级守卫
/// `reference_path_writes_go_through_this_helper` 钉住了这点，谁再绕过它就红。
fn set_voice_ref(ui: &MainWindow, path: &str) {
    if !keeps_reference_text(&ui.get_voice_ref_path(), path) {
        clear_reference_text(ui);
    }
    ui.set_voice_ref_path(path.into());
}

/// 界面上的音色输入：**唯一入口**。
///
/// 返回 `(参考音频路径, 参考音频的文本)`，并在这里统一执行"没有参考音就不带孤立文本"
/// 这条口径——单篇 / 批量 / 试听 / 模板比对都走它，免得某条路径漏掉一半。
/// （孤立文本会让"切回内置音色再选回同一段音频"静默沿用上一份转写。）
fn voice_input_from_ui(ui: &MainWindow) -> (Option<String>, Option<String>) {
    let path = non_empty(ui.get_voice_ref_path().to_string());
    let text = path
        .as_ref()
        .and_then(|_| non_empty(ui.get_voice_ref_text().to_string()));
    (path, text)
}

/// 「当前引擎要求参考文本」的提示：点名引擎 + 给两条出路。
///
/// 提交拦截与界面提示共用**同一个函数**（逐字一致，不另写一份，
/// 见 `LESSON_同一语义两处实现必然漂移回显需与真实行为同源`）。
/// 音频与文本成对是**按引擎**的要求，不再是全局硬要求（index-tts2 只给 voice_ref
/// 就 200，真机实测）。
fn reference_text_required_note(engine: &str) -> String {
    format!(
        "{engine} 要求参考文本：点「自动转写」把参考音频实际念的内容填上并核对，或换用不要求参考文本的引擎（如 index-tts2）。"
    )
}

/// "选中引擎的克隆音色还差参考文本吗"——提交前的拦截判据。
///
/// **按选中引擎**判断：只有"该引擎 `requires.reference_text`（如 audio8-tts）&&
/// 填了参考音 && 文本为空"才拦。可选引擎（index-tts2）直接放行。
/// 返回 true = 该拦住。
fn reference_text_missing(
    engine_requires_text: bool,
    voice_ref: &Option<String>,
    voice_ref_text: &Option<String>,
) -> bool {
    if !engine_requires_text {
        return false;
    }
    match voice_ref.as_deref() {
        Some(path) => {
            !path.trim().is_empty()
                && voice_ref_text
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .is_empty()
        }
        None => false,
    }
}

// 8 个参数确实多，但每个都是调用方必须显式给出的决策（路径/稿子/引擎/音色两项/停顿/
// 兜底/词典）。打包成配置结构体只是把同一串东西换个地方写，调用点反而更啰嗦——
// 与 `Project::new` 的处理一致。
#[allow(clippy::too_many_arguments)]
fn load_resumable(
    dir: &Path,
    script: &str,
    model: &str,
    voice_ref: Option<String>,
    voice_ref_text: Option<String>,
    gap_ms: u64,
    auto_normalize: bool,
    dict: &std::collections::BTreeMap<String, String>,
) -> Result<LoadedProject, String> {
    // 没有参考音就不要带着孤立的参考文本走（与 new_project_from_inputs 同一口径）
    let voice_ref_text = if voice_ref.is_some() {
        voice_ref_text
    } else {
        None
    };
    let dict_hash = dictionaries::fingerprint(dict);
    // 旁车修复快照（必须在任何 prepare 之前取）：0.1.10 只裁音频不同步文本，
    // 这类工程的已合成音频是在「文本错配」下发出来的。旁车一旦补上，旧音频
    // 必须整体作废重录一次，否则用户升级后仍听到旧错声音。
    let input_sidecar_missing_before = voice_ref
        .as_deref()
        .map(|p| aw_core::ref_audio::paired_reference_text(Path::new(p)).is_none())
        .unwrap_or(false);
    // ① 输入侧先收敛到 ≤15s：四个 UI 入口已 prepare 过，这里在 worker 侧再走一遍
    // 不是重复——覆盖直接调用方与旧工程输入，保证**进工程的 voice_ref 一定是
    // ≤15s 的路径**（超长→voice-trimmed 副本），并顺手在这算内容哈希。
    let (voice_ref, voice_ref_hash) = match voice_ref.as_deref() {
        Some(path) => {
            let prepared = prepare_reference_for_clone(path, voice_ref_text.as_deref())?;
            let hash = sha256_file(&prepared.path)?;
            (
                Some(prepared.path.to_string_lossy().into_owned()),
                Some(hash),
            )
        }
        None => (None, None),
    };
    // 损坏的工程在这里必须**中止**：`.ok()` 会把它当成"没有工程"，已合成句全变待合成，
    // 随后第一次落盘还会覆盖掉损坏文件（现场丢失）。见 Project::load_if_present。
    let mut saved = Project::load_if_present(dir)?;
    let saved_sidecar_missing_before = saved
        .as_ref()
        .and_then(|p| p.voice_ref.as_deref())
        .map(|p| aw_core::ref_audio::paired_reference_text(Path::new(p)).is_none())
        .unwrap_or(false);
    // ② 旧工程显式迁移：工程里存的 voice_ref 仍可能是超长路径（旧上限 30s 或更早
    // 的无上限工程）——把该工程的 voice_ref 一次性迁移为裁剪副本（更新 voice_ref
    // 与新 voice_ref_hash 并落盘）。与输入同源时两者会裁出同一个副本名，
    // settings_allow_reuse 仍能匹配 → 续作不丢（见 migrate_overlong_voice_ref）。
    if let Some(saved) = saved.as_mut() {
        migrate_overlong_voice_ref(dir, saved)?;
    }
    // 参考文本旁车此前缺失、现在已补上 ⇒ 旧音频用的是错配文本，必须重录一遍。
    // 只影响这一次：重录后旁车已在，后续加载不再作废。
    let text_repaired = reference_text_was_repaired(
        input_sidecar_missing_before || saved_sidecar_missing_before,
        [
            voice_ref.as_deref(),
            saved.as_ref().and_then(|p| p.voice_ref.as_deref()),
        ]
        .into_iter()
        .flatten()
        .any(|p| aw_core::ref_audio::paired_reference_text(Path::new(p)).is_some()),
    );
    if let Some(saved) = saved.as_ref() {
        // 兜底开关与模型/音色同类：它变了，spoken 文本就变，旧音频不能算数。
        // 停顿不进这个条件——它只影响拼装，改了不必重录（下面就直接改字段）。
        if !text_repaired
            && settings_allow_reuse(
                saved,
                model,
                &voice_ref,
                &voice_ref_hash,
                &voice_ref_text,
                auto_normalize,
                &dict_hash,
            )
            && sentence_texts_match(saved, script)
        {
            let mut project = saved.clone();
            if project.gap_ms != gap_ms {
                project.gap_ms = gap_ms;
                project
                    .save(dir)
                    .map_err(|e| format!("工程落盘失败: {e}"))?;
            }
            return Ok(LoadedProject { project, reused: 0 });
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("建工程目录失败: {e}"))?;
    let mut project = new_project_from_inputs(
        script,
        model,
        voice_ref.clone(),
        voice_ref_text.clone(),
        gap_ms,
        auto_normalize,
        dict,
    );
    // 用上面已经算好的哈希（少读一次参考音文件）
    project.voice_ref_hash = voice_ref_hash.clone();
    let reused = if let Some(saved) = saved.as_ref() {
        // 与快路径同一条判据（含旁车修复）：开关变了或文本配对被修过，就不能逐句继承
        // ——快路径只管整体复用，旧 done wav 仍会从慢路径被拷回来（复评实测）。
        if !text_repaired
            && settings_allow_reuse(
                saved,
                model,
                &voice_ref,
                &voice_ref_hash,
                &voice_ref_text,
                auto_normalize,
                &dict_hash,
            )
        {
            reuse_done_sentences(&mut project, saved, dir)?
        } else {
            0
        }
    } else {
        0
    };
    project
        .save(dir)
        .map_err(|e| format!("工程落盘失败: {e}"))?;
    Ok(LoadedProject { project, reused })
}

// ===========================================================================
// 界面共享状态（UI 线程单线程 Rc）
// ===========================================================================

struct AssembledInfo {
    wav: PathBuf,
    duration: f64,
}

/// 批量列表里的一行（Rust 侧真相）。
struct BatchRowState {
    name: String,
    /// 稿件正文（提交时随命令一起进 worker；ui 不持有）
    script: String,
    /// 切句后的句数：只有这一行真的跑起来才知道（导入时还没切句）
    sentences: usize,
    /// 提交后拿到任务台账 id；用来把进度/终态消息按 id 对回这一行
    task_id: Option<u32>,
    state: batch::ItemState,
    detail: String,
    /// 出片后的 (成品 wav, 字幕 srt)：批量收尾时用它告诉用户文件落在哪
    out: Option<(PathBuf, PathBuf)>,
}

/// 人声分离历史在 UI 侧的只读快照：条目 + 已经过安全解析的两轨。
#[derive(Clone)]
struct SepHistoryItem {
    entry: sep_history::SeparationHistoryEntry,
    tracks: sep_history::TrackResolution,
}

#[derive(Default)]
struct UiState {
    /// 最近一次拼装结果（导出复制 / 全篇试听用）
    assembled: RefCell<Option<AssembledInfo>>,
    /// 当前工程目录（试听找句子 wav、恢复进度用）
    project_dir: RefCell<Option<PathBuf>>,
    /// 试听播放的总时长（秒）：播放头 = position / total
    playing_total: std::cell::Cell<f32>,
    /// worker 当前是否持有与本 UI 一致的工程；变更后到下一轮 ProjectLoaded 前为 false。
    project_ready: std::cell::Cell<bool>,
    /// 工程输入的单调版本；任何会改变工程语义的 UI 修改都递增。
    project_revision: std::cell::Cell<u64>,
    /// 最近一次 BGM 三轨产物，供试听和导出。
    bgm_artifacts: RefCell<Option<BgmArtifacts>>,
    /// 最近一次歌曲产物（路径、时长）。
    song_artifact: RefCell<Option<(PathBuf, f64)>>,
    /// 文本描述生成音色的结果：None = 还没有可用结果。
    /// (音频文件路径, 生成时用的试听文本) —— 点「用作配音音色」时据此填
    /// voice_ref/reference_text，不能再去读界面输入框（那个值可能已被用户改过）。
    design_result: RefCell<Option<(PathBuf, String)>>,
    /// 翻唱源音频路径（None = 未选择）。与 `sep_input` 同族：路径是真相，
    /// UI 的 source-path / source-summary / source-picked 都只是它的投影。
    song_source: RefCell<Option<String>>,
    /// 人声分离：当前输入路径（用于 stale 判定）与两轨产物
    sep_input: RefCell<Option<String>>,
    sep_tracks: RefCell<Option<(PathBuf, PathBuf)>>,
    /// 最近分离历史的只读快照（最多 5 条，路径已经过工程内白名单校验）
    sep_history: RefCell<Vec<SepHistoryItem>>,
    sep_task: std::cell::Cell<Option<u32>>,
    /// 质检任务的 id（配音页那一条）
    eval_task: std::cell::Cell<Option<u32>>,
    /// 质检分数（句 index → 可懂度%）。重新合成/改稿后要清掉——分数会失效
    eval_scores: RefCell<HashMap<usize, f64>>,
    /// 每份分数**是哪个回读模型测的**（句 index → `Some(model)` / `None`=来源未知）。
    ///
    /// 与 `eval_scores` 同生共死：**所有写入都必须走 `replace_eval_ledger` /
    /// `clear_eval_ledger` / `drop_eval_ledger`**，由 `eval_ledger_is_written_from_one_place`
    /// 那条源码守卫钉住。单独改一份会造出"分数在、来源丢"，界面会把那种组合
    /// 误报成"来源未知的旧记录"。
    eval_models: RefCell<HashMap<usize, Option<String>>>,
    /// 句子列表当前是否按质检分数升序展示。只影响视图，不改工程。
    qa_sorted: std::cell::Cell<bool>,
    /// 连续点「跳到最差句」时给滚动目标加个不可见的亚像素偏移，
    /// 即使目标行没变也重新触发一次 viewport 更新。
    qa_scroll_phase: std::cell::Cell<bool>,
    /// 跨 Tab 任务台账（配音 / BGM / 音乐制作 / 人声分离共用一份）。
    tasks: RefCell<tasks::TaskQueue>,
    /// 各类任务当前的 id（进度/收尾消息按 id 回填）
    dub_task: std::cell::Cell<Option<u32>>,
    bgm_task: std::cell::Cell<Option<u32>>,
    song_task: std::cell::Cell<Option<u32>>,
    /// 重录是配音类任务里的独立一条
    redo_task: std::cell::Cell<Option<u32>>,
    /// 排队任务的取消登记表（UI 侧登记，worker 侧取走；见 src/cancel.rs）
    cancel: cancel::CancelRegistry,
    /// 批量队列（M4-P1）：导入的稿件行。真相在 Rust 侧（每行带着自己的 task_id 与
    /// 脚本正文），ui 里的 BatchRow 只是投影——ui 不该存正文，也不该自己算状态。
    batch_rows: RefCell<Vec<BatchRowState>>,
    /// 整批在跑（决定面板按钮文案与"能不能再导入/清空"）
    batch_running: std::cell::Cell<bool>,
    /// 导入时被跳过的稿件（原因文案）：批量结束后要如实带出来，不能只报成功的篇数
    batch_skipped_notes: RefCell<Vec<String>>,
    /// 批量导出是否在跑（后台线程）：防连点起一堆线程；导出与 worker 互不干扰
    batch_export_running: std::cell::Cell<bool>,
    /// 模型下载（P7）：队列里每条任务的最新快照（按 id 覆盖，真相在 download 侧）。
    downloads: RefCell<Vec<download::Snapshot>>,
    /// 模型 id → 当前在跑的下载任务 id（点「取消」时按它找回任务）
    download_ids: RefCell<HashMap<String, u64>>,
    /// 一键备份是否在跑（后台线程，**含正在弹目录选择框那段**）。
    /// 防连点起一堆线程、两批同时往同一个目标目录里写。这是真相，
    /// ui 的 `backup-running` 只是它的投影（按钮据此禁用）。
    backup_running: std::cell::Cell<bool>,
    /// 检查更新是否在飞（后台线程）：防连点。这是真相，
    /// ui 的 `update-running` 与按钮的 `update-blocked` 都只是它的投影。
    update_running: std::cell::Cell<bool>,
    /// 手里那份"有新版"的发布清单（None = 当前没有可打开的发布页）。
    /// 「打开发布页」能不能点就是这个 Option 的投影——**再存一个 bool 必然漂移**。
    update_release: RefCell<Option<update::Release>>,
    /// 当前启用的发音词典：库内文件名（None = 不启用）与词条内容（送给 worker 的那份）。
    active_dict_file: RefCell<Option<String>>,
    active_dict: RefCell<std::collections::BTreeMap<String, String>>,
    /// 「数字/年份规范化」开关上次被 tick 看到的值。PixelSwitch 没有回调，
    /// 靠它发现"用户切换了"→ 作废工程（spoken 文本会变，旧音频不能算数）。
    auto_normalize_seen: std::cell::Cell<bool>,
    /// 任务中心里"已排队 / 已运行 N"的上次刷新时刻：40ms 的 tick 不能每次都重建模型。
    last_task_refresh: std::cell::Cell<Option<Instant>>,
    /// 服务地址是否来自用户显式配置（AW_SERVER / 全局设置）：周期自愈判据的输入。
    /// 启动 / 应用设置 / 测试连接时刷新；显式 = 用户自己的服务，壳绝不碰它。
    engine_explicit: std::cell::Cell<bool>,
    /// 上次对托管引擎做周期自愈尝试的时刻（tick 每约 30s 问一次，纯判据在
    /// `engine_supervisor::should_periodic_heal`）。
    last_engine_heal: std::cell::Cell<Option<Instant>>,
    /// 上次写进状态行的自愈结论：与上次相同就不再刷（"不刷屏"）；引擎恢复健康后
    /// 清空，下一次故障才能再次提示。
    last_engine_heal_note: RefCell<Option<String>>,
    /// 截图/演示态（`AW_UI_STATE=tasks`）。演示任务只是给任务中心摆样子、没有对应的
    /// worker 命令，所以**不能参与**"有没有任务在飞"的判断——否则演示态下点开始配音
    /// 会被这些假任务挡住（复核指出）。只有 debug 构建会置位。
    demo_tasks: std::cell::Cell<bool>,
}

fn main() -> Result<(), slint::PlatformError> {
    // 监护线程入口：必须在起界面之前判，否则它也会开一个界面窗口。
    // 见 engine_supervisor::run_monitor 的说明（壳被强杀时用它收引擎）。
    {
        let args: Vec<String> = std::env::args().collect();
        if engine_supervisor::monitor_entry(&args) {
            return Ok(());
        }
    }
    let ui = MainWindow::new()?;

    let rows: Rc<VecModel<Sentence>> = Rc::new(VecModel::default());

    // ── 随包引擎：用户没自己配地址、且回环地址上没有服务时，拉起内置的那份 ──
    if let engine_supervisor::StartOutcome::Failed(why) = ensure_engine_serving() {
        eprintln!("随包引擎启动失败：{why}");
    }

    // ── 引擎发现 → 模型清单（默认优先 audio8-tts）──
    let (_, base, discover_note) = discover_engine();
    if !discover_note.is_empty() {
        ui.set_status_text(format!("引擎发现: {discover_note}").into());
    }
    apply_engine_discovery(&ui, None);

    ui.set_export_dir(export_dir().into());
    apply_bgm_settings(&ui);
    apply_update_settings(&ui);
    ui.set_backend_label(backend_label(base.as_deref()).into());
    ui.set_project_name(DEFAULT_PROJECT.into());
    ui.set_sentences(ModelRc::from(rows.clone()));
    ui.set_script_text(SAMPLE_SCRIPT.into());
    rebuild(&ui, &rows, SAMPLE_SCRIPT);
    ui.set_status_text(ready_note(base.as_deref()).into());

    // ── 播放器（音频输出不可用时试听给出明确报错，不拖垮界面）──
    let player = Rc::new(player::Player::open().unwrap_or_else(|e| {
        eprintln!("音频输出不可用（试听将报错）: {e}");
        player::Player::disabled()
    }));

    // ── 线程通道 + 停止位（UI 与工作线程共享同一个 stop）──
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (msg_tx, msg_rx) = channel::<WorkerMsg>();
    // UI 侧留一份 sender：worker 线程拿走 msg_tx 后，健康检查还要用它回消息
    let msg_tx_ui = msg_tx.clone();
    // 启动即探一次服务：状态栏 / 全局设置里立刻能看到连不连得上
    spawn_server_check(msg_tx_ui.clone(), 0);
    // 配音/BGM/歌曲共用这一个停止位（它们走同一台 worker 的顺序队列）；
    // **分离单独一个**：否则一边的停止请求会被另一边的"清零/置位"吃掉
    // （审查抓到：分离运行中点配音停止，会把分离结果当"用户停止"丢掉）。
    let stop = Arc::new(AtomicBool::new(false));
    let sep_stop = Arc::new(AtomicBool::new(false));
    // 质检自己的停止位（与 sep_stop 同理：共用会被对方的停/清吃掉）
    let eval_stop = Arc::new(AtomicBool::new(false));
    // 排队任务的取消登记表：UI 侧登记、worker 侧在开始执行前取走
    let cancel = cancel::CancelRegistry::new();
    let cancel_worker = cancel.clone();
    let state = Rc::new(UiState {
        // 只有"试听总时长"需要一个非零默认值；其余字段都走 Default，
        // 这样以后加字段不会再打破这里的构造（以及测试里的构造）
        playing_total: std::cell::Cell::new(1.0),
        cancel,
        ..UiState::default()
    });
    // 周期自愈的"显式地址恒跳过"判据输入：启动时先取一次，应用设置/测试连接后再刷新
    state.engine_explicit.set(server_base_for_engine().1);
    {
        let stop = Arc::clone(&stop);
        let sep_stop = Arc::clone(&sep_stop);
        let eval_stop = Arc::clone(&eval_stop);
        std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop,
                sep_stop,
                eval_stop,
                projects_root: projects_root(),
                cancel: cancel_worker,
            })
        });
    }

    // 启动时恢复默认工程的进度（断点续作可见）
    restore_project(&ui, &rows, &state, &cmd_tx);

    // ── 窗口控制 ──
    slint_pixel::install_title_bar_controls(&ui);
    slint_pixel::install_window_resize(&ui);

    wire_theme(&ui);
    // 模板下拉的名字来自 templates.json；坏了就在状态行说一声（不静默变空列表）
    if let Err(e) = read_templates() {
        ui.set_status_text(e.into());
    }
    refresh_template_names(&ui, None);
    wire_script(&ui, &rows, &cmd_tx, &state);
    wire_engine_changes(&ui, &cmd_tx, &state);
    wire_templates(&ui, &cmd_tx, &state);
    wire_versions(&ui, &rows, &cmd_tx, &state);
    wire_dictionary(&ui, &rows, &cmd_tx, &msg_tx_ui, &state);
    load_active_dictionary(&ui, &state);
    wire_asr_model(&ui, &rows, &state);
    refresh_asr_models(&ui);
    wire_design_model(&ui, &state);
    // voice-clone 批给这个函数加了 msg_tx_ui（「自动转写」要后台跑 ASR 再回消息）
    wire_voice_panel(&ui, &cmd_tx, &msg_tx_ui, &state);
    wire_voice_library(
        &ui,
        &VoiceLibraryCtx {
            cmd_tx: cmd_tx.clone(),
            msg_tx: msg_tx_ui.clone(),
        },
        &state,
    );
    refresh_voice_library(&ui);
    wire_global_settings(&ui, &msg_tx_ui, &cmd_tx, &state);
    wire_downloads(&ui, &msg_tx_ui, &state);
    // 启动时把「下载源 / 并发」回显填上（同一份设置投影，别在 Slint 里拼）
    refresh_download_source_view(&ui);
    wire_sentence_actions(&ui, &rows, &cmd_tx, &player, &state);
    wire_run(&ui, &rows, &cmd_tx, &player, &stop, &state);
    wire_export(&ui, &cmd_tx, &msg_tx_ui, &state);
    wire_bgm(&ui, &cmd_tx, &state, &player, &stop);
    wire_song(&ui, &cmd_tx, &msg_tx_ui, &state, &player);
    wire_keys(&ui, &rows, &player, &state);
    wire_task_center(&ui, &state, &stop, &sep_stop, &eval_stop);
    wire_batch(&ui, &cmd_tx, &msg_tx_ui, &state, &stop);
    wire_separation(&ui, &msg_tx_ui, &cmd_tx, &state, &player, &sep_stop);
    wire_quality_check(&ui, &cmd_tx, &state, &eval_stop);

    // 启动就把"任务 · 空闲"画上（状态栏 chip 与任务中心都读同一份台账）
    refresh_tasks(&ui, &state);
    #[cfg(debug_assertions)]
    seed_shot_tasks(&ui, &state);
    #[cfg(debug_assertions)]
    seed_shot_batch(&ui, &state);
    #[cfg(debug_assertions)]
    seed_shot_bgm_artifacts(&ui, &state);

    // 产截图 / 演示用初始态（仅 debug；release 无此旁路）
    // mem-shortfall 的 "oom" 态要 rows，auto-update 的 "update" 态要 state —— 两个都传
    apply_shot_state(&ui, &rows, &state);

    // ── 主循环 ──
    let timer = Timer::default();
    {
        let weak = ui.as_weak();
        let rows = rows.clone();
        let msg_rx = Rc::new(RefCell::new(msg_rx));
        let cmd_tx_tick = cmd_tx.clone();
        let msg_tx_tick = msg_tx_ui.clone();
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(TICK_MS),
            move || {
                if let Some(ui) = weak.upgrade() {
                    tick(
                        &ui,
                        &rows,
                        &msg_rx,
                        &player,
                        &state,
                        &cmd_tx_tick,
                        &msg_tx_tick,
                    );
                }
            },
        );
    }

    let result = ui.run();
    // 只回收自己拉起的那份（外部服务不在我们句柄里，本来就动不到）
    if let Some(mut sup) = ENGINE_SUPERVISOR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        sup.stop();
    }
    result
}

fn ready_note(base: Option<&str>) -> String {
    match base {
        Some(b) => format!(
            "就绪：示例稿已切句 · 服务 {b} · 配音的切句/倍速/导出在本页「高级」，全局设置在右上角"
        ),
        None => "就绪：未发现服务（server.json），合成会失败——先启动 audiocpp_server".into(),
    }
}

/// 产截图 / 演示用初始态（`AW_UI_STATE=selected|drawer|dark`；仅 debug 构建存在，
/// release 整个函数被编译掉，验证：`strings target/release/audio-workshop | grep -c AW_UI_STATE` → 0）
#[cfg(debug_assertions)]
fn apply_shot_state(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, ui_state: &Rc<UiState>) {
    let Ok(state) = std::env::var("AW_UI_STATE") else {
        return;
    };
    match state.as_str() {
        "selected" => {
            ui.set_selected(1);
            ui.set_status_text("已选中第 2 句 · 试听 / 重录就在行下方".into());
        }
        "oom" => {
            let body = r#"{"error":{"message":"cannot load model \u0027qwen3-asr\u0027: estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}"#;
            if let Some(detail) = aw_core::memory_shortfall_note(body) {
                set_status(rows, 0, &format!("error: oom: {detail}"));
                ui.set_selected(0);
                ui.set_status_text("内存不足：展开的句子行应显示「释放模型内存」按钮".into());
            }
        }
        "drawer" => {
            ui.set_drawer_open(true);
            ui.set_status_text(
                "全局设置：外观 / 工程 / 服务 / 模型；各 Tab 独有的参数放它们自己页面".into(),
            );
        }
        "dark" => {
            ui.set_theme_scheme("dark".into());
            ui.set_status_text("主题已切换：暗色".into());
        }
        "downloads" => {
            // 渲染核对（不是真跑）：把下载队列的四种状态 + 没有下载源 + 落点警告各摆一条
            ui.set_drawer_open(true);
            ui.set_download_rows(ModelRc::from(Rc::new(VecModel::from(vec![
                DownloadRow {
                    key: "audio8-tts".into(),
                    label: "audio8-tts".into(),
                    state: "下载中".into(),
                    detail: "来源：内置下载清单 · 已下载 412 MB / 2.1 GB · 从 412 MB 字节断点续传".into(),
                    tiers: "".into(),
                    advice: "".into(),
                    progress: 0.19,
                    active: true,
                    actionable: true,
                },
                DownloadRow {
                    key: "indextts2".into(),
                    label: "indextts2".into(),
                    state: "排队".into(),
                    detail: "来源：server.json（服务清单） · /Users/me/models/indextts2".into(),
                    tiers: "档位：q8_0 3.26 GiB · f16 4.24 GiB · orig 7.34 GiB（下载按钮取 q8_0）".into(),
                    advice: "本机推荐 f16：估算占用 6.48 GiB + 余量 1.00 GiB ≤ 物理内存 16.00 GiB 的一半 8.00 GiB".into(),
                    progress: 0.0,
                    active: true,
                    actionable: true,
                },
                DownloadRow {
                    key: "yue2".into(),
                    label: "yue2".into(),
                    state: "已完成".into(),
                    detail: "来源：内置下载清单 · 下载完成 · /Users/me/models/yue2.gguf".into(),
                    tiers: "".into(),
                    advice: "".into(),
                    progress: 1.0,
                    active: false,
                    actionable: true,
                },
                DownloadRow {
                    key: "ace-step".into(),
                    label: "ace-step".into(),
                    state: "失败".into(),
                    detail: "来源：内置下载清单 · 网络错误：服务器返回 HTTP 404 · /Users/me/models/ace.gguf".into(),
                    tiers: "档位：q8_0 5.76 GiB · bf16 9.40 GiB（下载按钮取 q8_0）".into(),
                    advice: "本机装不下任何一档：最小档 q8_0 要估算占用 8.77 GiB + 余量 1.00 GiB = 9.77 GiB > 物理内存 16.00 GiB 的一半 8.00 GiB".into(),
                    progress: 0.0,
                    active: false,
                    actionable: true,
                },
                DownloadRow {
                    key: "qwen3-asr".into(),
                    label: "qwen3-asr".into(),
                    state: "未下载".into(),
                    detail: "来源：内置下载清单 · /Volumes/DataExt/models/Qwen3-ASR-0.6B-GGUF/qwen3-asr-0.6b-q8_0.gguf · ⚠ 落点与 server.json 对不上：清单 path 是 /Volumes/DataExt/models/Qwen3-ASR-0.6B-GGUF/qwen3-asr-0.6b-q8_0.gguf，本次会下到 /Users/me/models/Qwen3-ASR-0.6B-GGUF/qwen3-asr-0.6b-q8_0.gguf —— 下完服务仍可能加载不到".into(),
                    tiers: "档位：q8_0 1.07 GiB · f16 1.75 GiB（下载按钮取 q8_0）".into(),
                    advice: "本机推荐 f16：估算占用 3.27 GiB + 余量 1.00 GiB ≤ 物理内存 16.00 GiB 的一半 8.00 GiB".into(),
                    progress: 0.0,
                    active: false,
                    actionable: true,
                },
                DownloadRow {
                    key: "yue-2-no-source".into(),
                    label: "yue2".into(),
                    state: "没有下载源".into(),
                    detail: "上游 model_specs 里没有 family=yue2 的 spec —— 暂无下载源，不猜地址".into(),
                    tiers: "".into(),
                    advice: "".into(),
                    progress: 0.0,
                    active: false,
                    actionable: false,
                },
            ]))));
            ui.set_status_text(
                "模型下载：队列 / 断点续传 / 校验 / 没有下载源 / 落点提醒（示例数据）".into(),
            );
        }
        "download-source" => {
            // 演示态：把「当前生效的源」两种口径都摆出来（本态只读，**不写用户设置**）。
            //
            // 这里刻意不走 `settings_snapshot()`——那是**用户真机设置**，给演示态读真值会
            // 让截图随每个人的配置漂移。文案与真实行为都来自同一个纯函数
            // `download_mirror::{source_note, rewrite_url}`。
            ui.set_drawer_open(true);
            eprintln!(
                "AW_UI_STATE=download-source：无镜像 -> {}",
                download_mirror::source_note("")
            );
            eprintln!(
                "AW_UI_STATE=download-source：有镜像 -> {}",
                download_mirror::source_note("https://hf-mirror.com")
            );
            for line in download_source_demo_lines() {
                eprintln!("{line}");
            }
            ui.set_download_mirror("https://hf-mirror.com".into());
            ui.set_download_source_note(
                download_mirror::source_note("https://hf-mirror.com").into(),
            );
            ui.set_download_concurrency("2".into());
            ui.set_download_concurrency_note(
                "同时下载 2 个模型（并发数改动下次启动生效；每个模型的下载/取消互不影响）".into(),
            );
            ui.set_status_text("模型下载源：填了镜像就只走镜像，连不上如实报错（示例）".into());
        }
        "model-sources" => {
            // 真机态：不灌示例数据，直接按「server.json ∪ 内置清单」渲一遍，
            // 并报出"几个真的有下载入口、几个如实标了没有源"。
            ui.set_drawer_open(true);
            let plan = download_plan();
            let ready = plan.iter().filter(|r| r.action.is_some()).count();
            let missing = plan.len() - ready;
            let conflicts = plan
                .iter()
                .filter(|r| r.action.as_ref().is_some_and(|a| a.conflict.is_some()))
                .count();
            // 无头核对用（这个态就是给"跑一次看真实数字"用的）：把每行的结论打到 stderr，
            // 屏幕上也能看，但屏幕锁着时只有 stderr 拿得到。
            //
            // 「模型目录」与「清单推出的模型根」都打出来：落点冲突的根因就是这两个值，
            // 只看行数看不出是谁的问题（见 default_model_dir 的注释）。
            eprintln!(
                "AW_UI_STATE=model-sources：模型目录 = {}（清单推出的模型根 = {}）",
                model_dir().display(),
                default_model_dir().display()
            );
            // 抽屉里那条「模型目录」提示也打出来：以前默认目录不存在时会显示
            // “目录不存在（清单里有 N 个模型）”。读的是启动时 `apply_engine_discovery`
            // → `refresh_settings_view` 已经投影好的 UI 真实值，不在这里另拼一份文案。
            eprintln!(
                "AW_UI_STATE=model-sources：模型区提示 = {}",
                ui.get_model_dir_info()
            );
            eprintln!(
                "AW_UI_STATE=model-sources：{ready} 个有下载入口 / {missing} 个没有下载源 / {conflicts} 条带「落点与 server.json 对不上」"
            );
            let budget = machine_budget();
            eprintln!(
                "AW_UI_STATE=model-sources：物理内存 = {}（服务余量 = {}）",
                budget
                    .physical_memory
                    .map(model_sources::human_bytes)
                    .unwrap_or_else(|| "拿不到".into()),
                model_sources::human_bytes(budget.headroom_bytes)
            );
            for r in &plan {
                match &r.action {
                    Some(a) => {
                        let warn = match &a.conflict {
                            Some(c) => format!(" ⚠ {c}"),
                            None => String::new(),
                        };
                        eprintln!(
                            "  {} → {}（{}）{warn}",
                            r.id,
                            a.dest.display(),
                            a.origin.label()
                        )
                    }
                    None => eprintln!("  {} → 没有下载源：{}", r.id, r.reason),
                }
                // 档位与本机推荐（同一份纯函数算出来的，界面渲染的就是这两行）
                let (tiers, advice) =
                    model_sources::tiers_and_advice(model_sources::catalog(), &r.id, &budget);
                if !tiers.is_empty() {
                    eprintln!("      {}", tier_line(&tiers));
                }
                if !advice.note.is_empty() {
                    eprintln!("      {}", advice.note);
                }
            }
            refresh_download_rows(ui, ui_state);
            ui.set_status_text(
                format!("模型下载源：{ready} 个有下载入口 · {missing} 个如实标了没有源").into(),
            );
        }
        "qa-tag" => {
            // 只读演示/核对态：把"换过回读模型"的三条口径都摆出来（纯函数，不碰任何配置）。
            // 不读用户真设置当输入，否则输出会随每个人的 settings.json 漂移。
            let mismatch = ScoreSources {
                models: vec!["fun-asr".into()],
                unknown: 0,
                scored: 12,
            };
            let same = ScoreSources {
                models: vec!["audio8-asr".into()],
                unknown: 0,
                scored: 12,
            };
            let legacy = ScoreSources {
                models: vec![],
                unknown: 9,
                scored: 9,
            };
            eprintln!(
                "AW_UI_STATE=qa-tag：换过模型 -> {}",
                qa_source_note(&mismatch, "audio8-asr")
            );
            eprintln!(
                "AW_UI_STATE=qa-tag：同一模型 -> {}",
                qa_source_note(&same, "audio8-asr")
            );
            eprintln!(
                "AW_UI_STATE=qa-tag：旧工程（无来源） -> {}",
                qa_source_note(&legacy, "audio8-asr")
            );
            // 再走一遍真实路径：灌台账 → sync_qa_actions（界面读的就是它）
            let n = rows.row_count().max(1);
            replace_eval_ledger(
                ui_state,
                &(0..n)
                    .map(|i| (i, 90.0 + i as f64, Some("fun-asr".to_string())))
                    .collect::<Vec<_>>(),
            );
            sync_qa_actions(ui, rows, ui_state);
            eprintln!(
                "AW_UI_STATE=qa-tag：界面 qa-source-note / 换过模型（当前回读模型 = {}）-> {}",
                effective_asr_model(&settings_snapshot()),
                ui.get_qa_source_note()
            );
            // 旧工程那条也过一遍**真实路径**（`None` = 来源未知），证明它确实能上屏，
            // 不是只活在纯函数里。
            replace_eval_ledger(
                ui_state,
                &(0..n)
                    .map(|i| (i, 90.0 + i as f64, None))
                    .collect::<Vec<_>>(),
            );
            sync_qa_actions(ui, rows, ui_state);
            eprintln!(
                "AW_UI_STATE=qa-tag：界面 qa-source-note / 旧工程（当前回读模型 = {}）-> {}",
                effective_asr_model(&settings_snapshot()),
                ui.get_qa_source_note()
            );
        }
        "engines" => {
            // 真机核对（不截图）：把**实际清单**算出来的引擎列表打到 stderr。
            // 数字必须来自真实 server.json，不能是代码里推断的。
            let models = read_server_config().map(|c| c.models).unwrap_or_default();
            let voices = tts_engine_voices(&models);
            let dflt = default_engine_index(&voices);
            eprintln!("=== 配音引擎下拉 · 清单 {}", config_path().display());
            eprintln!(
                "清单里 task==tts 共 {} 个 → 过滤后剩 {} 个",
                models.iter().filter(|m| m.task == "tts").count(),
                voices.len()
            );
            for (i, v) in voices.iter().enumerate() {
                eprintln!(
                    "  [{i}] {} · 要求参考音={} · 要求参考文本={} · 默认选中={} · known_issues={:?}",
                    v.name,
                    v.requires_voice_ref,
                    v.requires_reference_text,
                    i as i32 == dflt,
                    v.known_issues
                );
                // 开工判据按"当前参考音路径 + 参考文本"实算（通常是空 = 用内置音色）
                let refp = ui.get_voice_ref_path().to_string();
                let reft = ui.get_voice_ref_text().to_string();
                let exists = !refp.trim().is_empty() && std::path::Path::new(&refp).is_file();
                eprintln!(
                    "      开工判据（参考音「{}」/ 文本「{}」）：{:?}",
                    refp,
                    reft,
                    voice_readiness(Some(v), &refp, exists, &reft)
                );
            }
            for m in models
                .iter()
                .filter(|m| m.task == "tts" && !is_selectable_tts_engine(m))
            {
                eprintln!(
                    "  [不出现在下拉] {} · product_excluded={} · mode={:?} · role={:?}",
                    m.id, m.caps.product_excluded, m.caps.mode, m.caps.role
                );
            }
            let song = song_engine_options(&models);
            eprintln!("=== 音乐制作引擎（清单 task==gen）");
            for o in &song {
                eprintln!("  {} → {}", o.label, o.id);
            }
        }
        "voice" => {
            ui.set_dub_voice(true);
            refresh_voice_labels(ui);
            ui.set_status_text("音色：内置默认 / 参考音频克隆；换音色不是换模型".into());
        }
        "design" => {
            ui.set_scene(4);
            ui.set_status_text(
                "音色设计：参考音频克隆 + 文本描述生成音色（task vdes / options.instruction）"
                    .into(),
            );
        }
        "advanced" => {
            ui.set_dub_advanced(true);
            ui.set_status_text(
                "高级：重新切句 / 倍速 / 规范化 / 任务 / 导出（配音独有，放本页）".into(),
            );
        }
        "apply" => {
            // 复现审查的阻塞场景：默认模型目录不存在时点「应用并重连」，
            // 以前会 early-return 连端口一起丢；现在应写出 settings.json。
            ui.set_server_host("127.0.0.1".into());
            ui.set_server_port("8080".into());
            ui.invoke_apply_server_settings();
        }
        "both" => {
            // 内容超高场景：音色浮层 + 高级区同时展开，验证 Body 区出滚动条而不是顶掉状态栏
            ui.set_dub_voice(true);
            ui.set_dub_advanced(true);
            ui.set_status_text("音色 + 高级同时展开：Body 区应出垂直滚动条".into());
        }
        "update" => {
            // 有新版的样子（**不真发请求**）。真相必须在 `state` 里：
            // tick 的 refresh_update_availability 每拍都按 state 重投影，
            // 只改 UI 的话「打开发布页」会被立刻按下——演示态不是只有截图才用的边角
            // （见 LESSON_同一语义两处实现必然漂移 的补充实例）。
            ui.set_drawer_open(true);
            let demo = update::Release {
                version: "v0.2.0".into(),
                notes: "示例：立体声时长修正 / 音色库 / 词典库".into(),
                url: "https://github.com/gqf2008/audio-workshop/releases".into(),
                sha256: None,
                size: Some(48 * 1024 * 1024),
            };
            ui.set_update_info(demo.summary().into());
            *ui_state.update_release.borrow_mut() = Some(demo);
            ui.set_status_text("更新：有新版（演示态）".into());
        }
        "sep" => {
            ui.set_scene(2);
            ui.set_status_text("人声分离：选一段音频，本地模型拆人声 / 伴奏".into());
        }
        "sep-done" => {
            // 结果态渲染核对（不是真跑）：直接灌两轨标签与状态
            ui.set_scene(2);
            ui.set_sep_input_path("/tmp/aw-sep-src.wav".into());
            ui.set_sep_input_summary("待分离：aw-sep-src.wav".into());
            ui.set_sep_has_result(true);
            ui.set_sep_result_available(true);
            ui.set_sep_vocals_label("人声 · cli_vocals.wav".into());
            ui.set_sep_accompaniment_label("伴奏 · cli_accompaniment.wav".into());
            ui.set_sep_status_text("两轨已生成 · 可分别试听和导出".into());
            ui.set_status_text("人声分离：结果就绪态（示例数据，用于核对两轨列表）".into());
        }
        "sep-run" => {
            // 走 UI 代码路径真跑一次：输入用已存在的测试音频，模型已在缓存里
            ui.set_scene(2);
            ui.set_sep_input_path("/tmp/aw-sep-src.wav".into());
            ui.set_sep_input_summary("待分离：aw-sep-src.wav".into());
            ui.set_sep_result_available(false);
            ui.set_sep_chunk_seconds("30".into());
            ui.invoke_sep_run();
        }
        "bgm-done" => {
            // 结果区渲染核对（不是真跑）：灌三轨标签 + has-result
            ui.set_scene(1);
            ui.set_bgm_has_result(true);
            ui.set_bgm_stale(false);
            // 三轨都存在（这个态就是用来核对三轨结果区的）
            ui.set_bgm_has_voice_track(true);
            ui.set_bgm_has_mixed_track(true);
            ui.set_bgm_voice_label("人声 · 示例工程 · 频道口播_voice.wav".into());
            ui.set_bgm_track_label("BGM · 示例工程 · 频道口播_bgm.wav".into());
            ui.set_bgm_mixed_label("混音 · 示例工程 · 频道口播_mixed.wav".into());
            ui.set_bgm_status_text("5 段 · 混音 03:42 · 已完成（示例数据）".into());
            ui.set_status_text("BGM：结果就绪态（示例数据，用于核对三轨结果区）".into());
        }
        "bgm-standalone-run" => {
            // 真跑：无配音成品 → 用「高级」里的 30 秒档独立生成 BGM（只出 BGM 一轨）
            ui.set_scene(1);
            ui.set_bgm_prompt("轻快的木吉他循环，无人声，适合口播背景".into());
            ui.set_bgm_standalone_index(0);
            ui.invoke_bgm_generate();
        }
        "song" => {
            ui.set_scene(3);
            ui.set_status_text("音乐制作：写歌 / 文生音乐（yue2 · ace-step）".into());
        }
        "music-cover" => {
            // 翻唱模式渲染核对（不真跑：sheetsage2 + yue2 要 8 分钟级）
            ui.set_scene(3);
            ui.set_song_mode_index(1);
            ui.set_song_status_text("翻唱：选源音频 → sheetsage2 转谱 → yue2 唱新词".into());
            ui.set_status_text("音乐制作：翻唱模式（源音频入口 + 引擎固定 yue2）".into());
        }
        "music-done" => {
            // 结果条渲染的真机证据：不真跑生成（song 要 6–8 分钟），直接置 has-result
            ui.set_scene(3);
            ui.set_song_has_result(true);
            ui.set_song_status_text("歌曲 02:48 · 已生成（示例状态，用于核对结果条）".into());
            ui.set_status_text("音乐制作：结果就绪态（结果条含试听 / 导出）".into());
        }
        "music-adv" => {
            ui.set_scene(3);
            ui.set_song_advanced(true);
            ui.set_status_text("音乐制作·高级：引擎（yue2 / ACE-Step）在这里，不占首屏".into());
        }
        "bgm" => {
            ui.set_scene(1);
            ui.set_status_text("BGM：按描述生成，自动对齐配音时长并 ducking".into());
        }
        "gated" => {
            ui.set_scene(2);
            ui.set_status_text("人声分离：后端未接入，占位".into());
        }
        _ => {}
    }
}

#[cfg(not(debug_assertions))]
fn apply_shot_state(_ui: &MainWindow, _rows: &Rc<VecModel<Sentence>>, _state: &Rc<UiState>) {}

fn apply_project_to_rows(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    project: &Project,
) -> usize {
    let done = apply_project_sentence_statuses(rows, project);
    recompute_total(rows);
    ui.set_done_count(done as i32);
    ui.set_progress(done as f32 / rows.row_count().max(1) as f32);
    ui.set_has_result(done > 0);
    done
}

/// `apply_project_to_rows` 的状态回灌主体（不碰 MainWindow，便于无头回归）。
/// 关键是 error 分支传**完整** `sentence.status`：磁盘上的 `error: oom: ...`
/// 不能在这里被压成裸 "error"，否则重开后释放模型内存按钮会消失。
fn apply_project_sentence_statuses(rows: &Rc<VecModel<Sentence>>, project: &Project) -> usize {
    let mut done = 0usize;
    for sentence in &project.sentences {
        let Some(i) = row_position(rows, sentence.index) else {
            continue;
        };
        if sentence.status == "done" {
            done += 1;
            set_status(rows, i, "done");
            if let Some(d) = sentence.duration {
                set_row_duration(rows, i, d as f32);
            }
        } else if sentence.status.starts_with("error") {
            // 完整状态串要带下去：`error: oom: ...` 里的详情就是「释放模型内存」
            // 按钮的可见性来源，压成裸 "error" 会让重开工程后按钮消失。
            set_status(rows, i, &sentence.status);
        } else {
            set_status(rows, i, "pending");
        }
    }
    done
}

fn mark_running_rows_failed(rows: &Rc<VecModel<Sentence>>) {
    for i in 0..rows.row_count() {
        if rows
            .row_data(i)
            .map(|row| row.status.as_str() == "合成中")
            .unwrap_or(false)
        {
            set_status(rows, i, "error");
        }
    }
}

fn restore_voice_index(ui: &MainWindow, model: &str) -> bool {
    for i in 0..ui.get_voice_names().row_count() {
        if ui
            .get_voice_names()
            .row_data(i)
            .map(|name| name == model)
            .unwrap_or(false)
        {
            ui.set_voice_index(i as i32);
            return true;
        }
    }
    false
}

/// 启动时恢复工程：project.json 在 → 句子列表/状态/时长/模型/参考音全部回灌，
/// 并把同一工程交给 worker，保证重开后 Redo/Assemble 不用先重跑合成。
fn restore_project(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    state: &Rc<UiState>,
    cmd_tx: &Sender<Cmd>,
) {
    let stem = file_stem(&ui.get_project_name());
    let dir = project_dir(&stem);
    let project = match Project::load_if_present(&dir) {
        Ok(Some(p)) => p,
        Ok(None) => return, // 没有工程文件：全新开始，正常路径
        Err(note) => {
            // 读不了就说清路径与处置建议。以前这里静默 return，用户看到的是
            // "进度凭空消失"，连哪里坏了都不知道。
            ui.set_status_text(note.into());
            return;
        }
    };
    *state.project_dir.borrow_mut() = Some(dir.clone());
    // 输入框与列表同源：把保存的句子文本回填到输入框。
    // 每个保存句已经 trim 且折叠过空白，重新拼接后再 split 是幂等的。
    let script: String = project.sentences.iter().map(|s| s.text.as_str()).collect();
    ui.set_script_text(script.clone().into());
    rebuild(ui, rows, &script);
    let model_restored = restore_voice_index(ui, &project.model);
    if !model_restored {
        // 不保留默认 index，否则“开始合成”会把缺失模型静默换成另一音色。
        ui.set_voice_index(-1);
    }
    // 参考文本与参考音一起回灌：只回灌路径的话，重开应用后克隆音色会因为"没文本"
    // 被前置拦截挡住，用户得凭空重填一次（其实工程里存着）。顺序不能反——
    // `set_voice_ref` 会因为"换了段音频"先把文本清掉。
    set_voice_ref(ui, &project.voice_ref.clone().unwrap_or_default());
    ui.set_voice_ref_text(project.voice_ref_text.clone().unwrap_or_default().into());
    refresh_voice_labels(ui);
    // 工程里记着的两个输入回灌界面：兜底开关（决定怎么念）与句间停顿（决定拼装）
    ui.set_auto_normalize(project.auto_normalize);
    state.auto_normalize_seen.set(project.auto_normalize);
    ui.set_gap_ms_text(project.gap_ms.to_string().into());
    let done = apply_project_to_rows(ui, rows, &project);
    // 质检分数是句级持久化的：启动就把它们贴回行上（否则"重开还能看到"要等下一次合成）。
    // **分数连同它的来源模型一起回灌**：只灌分数会让旧工程被误报成"来源未知"。
    replace_eval_ledger(state, &eval_ledger_from_project(&project));
    apply_eval_labels(rows, &state.eval_scores.borrow());
    sync_qa_actions(ui, rows, state);
    // BGM 产物是落盘的：重开应用也要看到上次那几轨（否则"昨天混好的分轨今天导不出来"）
    restore_bgm_from_disk(ui, state, &dir);
    refresh_versions(ui, state);
    let _ = cmd_tx.send(Cmd::OpenProject {
        revision: state.project_revision.get(),
        dir,
        project: project.clone(),
    });
    state.project_ready.set(true);

    let mut notes = Vec::new();
    if done > 0 {
        notes.push(format!(
            "已恢复上次进度：{done}/{} 句已合成（断点续作）",
            rows.row_count()
        ));
    }
    if !model_restored {
        notes.push(format!(
            "工程模型 {} 不在当前 server.json，请先启动/下载或另选音色",
            project.model
        ));
    }
    if !notes.is_empty() {
        ui.set_status_text(notes.join(" · ").into());
    }
}

// ===========================================================================
// 回调接线
// ===========================================================================

fn wire_theme(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_theme_picked(move |scheme| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_theme_scheme() == scheme {
            return;
        }
        let label = if scheme == "dark" { "暗色" } else { "浅色" };
        ui.set_theme_scheme(scheme);
        ui.set_status_text(format!("主题已切换：{label}").into());
    });

    let weak = ui.as_weak();
    ui.on_scene_changed(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let note = SCENE_NOTES.get(i.max(0) as usize).copied().unwrap_or("");
        // 切 Tab 时顺手把 BGM 输入落盘：duck/时长档位没有回调，
        // 「改了档位但没生成就退出」本来会丢，这里把窗口收窄
        save_bgm_settings(&ui);
        ui.set_status_text(note.into());
    });
}

/// 把 UI 行按工程真实 index 找回原顺序。
fn restore_project_order(mut rows: Vec<Sentence>) -> Vec<Sentence> {
    rows.sort_by_key(|row| row.index);
    rows
}

/// 质检排序：有分数的句子在前，按可懂度升序；没分数的句子排后面。
///
/// 同分时按工程 index 升序，因此排序是稳定且可复现的；只返回行副本，
/// 不改 `Project`，也不依赖 `no` 或当前显示位置。
fn sort_rows_for_eval(mut rows: Vec<Sentence>, scores: &HashMap<usize, f64>) -> Vec<Sentence> {
    rows.sort_by(|a, b| {
        let a_project = usize::try_from(a.index).ok();
        let b_project = usize::try_from(b.index).ok();
        let ascore = a_project.and_then(|i| scores.get(&i));
        let bscore = b_project.and_then(|i| scores.get(&i));
        match (ascore, bscore) {
            (Some(a_score), Some(b_score)) => a_score
                .total_cmp(b_score)
                .then_with(|| a_project.cmp(&b_project)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a_project.cmp(&b_project),
        }
    });
    rows
}

/// 按工程真实 index 找当前 UI 行位置。排序只改变这个位置，不改变工程句子本身。
fn row_position_in_slice(rows: &[Sentence], project_index: usize) -> Option<usize> {
    rows.iter()
        .position(|row| usize::try_from(row.index).ok() == Some(project_index))
}

fn row_position(rows: &Rc<VecModel<Sentence>>, project_index: usize) -> Option<usize> {
    (0..rows.row_count()).find(|&i| {
        rows.row_data(i)
            .and_then(|row| usize::try_from(row.index).ok())
            == Some(project_index)
    })
}

/// 把当前选中行换成工程 index；未选中（负值）保持“无选中”，不要默认吸到第 0 行。
fn selected_project_index(selected: i32, rows: &[Sentence]) -> Option<usize> {
    usize::try_from(selected)
        .ok()
        .and_then(|i| rows.get(i))
        .and_then(|row| usize::try_from(row.index).ok())
}

fn rows_as_vec(rows: &Rc<VecModel<Sentence>>) -> Vec<Sentence> {
    (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .collect()
}

fn apply_row_order(rows: &Rc<VecModel<Sentence>>, ordered: Vec<Sentence>) {
    rows.set_vec(ordered);
}

/// 当前最低可懂度句：分数最低优先，同分取工程 index 最小。
///
/// 不缓存这个结果。`eval_scores` 是唯一真相源，重启回灌或任一句失效后，
/// 下一次动作直接按当前分数重新算；这样不会留下“分数还在但最差索引已清空”的死状态。
fn lowest_scored_index(rows: &[Sentence], scores: &HashMap<usize, f64>) -> Option<usize> {
    rows.iter()
        .filter_map(|row| {
            let index = usize::try_from(row.index).ok()?;
            scores.get(&index).map(|score| (index, *score))
        })
        .min_by(|(a_index, a_score), (b_index, b_score)| {
            a_score
                .total_cmp(b_score)
                .then_with(|| a_index.cmp(b_index))
        })
        .map(|(index, _)| index)
}

/// 质检跑完（`Msg::EvalDone`）自动选中的那一句 + 追加到状态行的文案。
///
/// 判据与「跳到最差句」同源：都是 `lowest_scored_index`（工程**当前完整分数集**里的
/// 最低分句）。这里不能改用 `summary.worst.first()`——那份只收**本轮有差异**的句子，
/// ASR 失败而保留下来的旧分不在其中，同一份分数集会在两条路径上指向不同的句。
/// 返回值第二项为 `None` 时（一句都没分）调用方要清掉选中。
fn eval_done_selection(rows: &[Sentence], scores: &HashMap<usize, f64>) -> (Option<usize>, String) {
    match lowest_scored_index(rows, scores) {
        Some(index) => (Some(index), format!("·已选中第 {} 句", index + 1)),
        None => (None, String::new()),
    }
}

/// 一份分数集的「来源」投影。
///
/// 三种情况必须分开，这正是本批要修的（以前只有一句笼统的"可能是上一个模型测的"）：
/// · 有明确来源 → 按模型归组；
/// · **有分但来源未知** → 旧工程在记录来源之前测的，既不能冒充"匹配"、也不能当成"没测过"；
/// · 没分的句子根本不进这里（那才是真的"没测过"）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScoreSources {
    /// 出现过的来源模型（去重 + 排序，保证同输入同输出）
    models: Vec<String>,
    /// 有分但**来源未知**的句数
    unknown: usize,
    /// 有分的总句数
    scored: usize,
}

/// 从 UI 台账投影出"这些分是谁测的"。
///
/// 判据只看台账（不看 `Sentence`）：排序 / 最差句按钮消费的也是这份台账，
/// 口径不会分叉。台账里缺来源条目按"未知"处理——宁可说不知道，也不冒充匹配。
fn ledger_score_sources(state: &UiState) -> ScoreSources {
    let scores = state.eval_scores.borrow();
    let models = state.eval_models.borrow();
    let mut out = ScoreSources::default();
    for index in scores.keys() {
        out.scored += 1;
        match models.get(index).and_then(|m| m.as_deref()) {
            Some(model) if !model.trim().is_empty() => out.models.push(model.to_string()),
            _ => out.unknown += 1,
        }
    }
    out.models.sort();
    out.models.dedup();
    out
}

/// 「现有分数是谁测的」这一句：只陈述事实 + 给下一步，不做任何自动动作
/// （不自动作废、不自动重测——那是产品决定，见 `docs/quality-check.md` §5）。
///
/// `current` 是当前生效的回读模型（唯一入口 `effective_asr_model`）。
fn qa_source_note(sources: &ScoreSources, current: &str) -> String {
    if sources.scored == 0 {
        return String::new();
    }
    let scored = sources.scored;
    let unknown = sources.unknown;
    let known = scored - unknown;
    let models = sources.models.join("、");
    if sources.models.is_empty() {
        // 全是来源未知的旧记录：既不能说"匹配"，也不能说"没测过"
        return format!(
            "现有 {scored} 句的分数是【来源未知】的旧记录（早于本应用记录来源的版本）：\
             无法判断它们是不是 {current} 测的，建议重新质检一次再看结论"
        );
    }
    if unknown > 0 {
        return format!(
            "现有 {scored} 句的分数里：{models} 测的 {known} 句；另 {unknown} 句来源未知（旧记录）。\
             当前回读模型是 {current}——来源未知的那部分无法判断是不是它测的，建议重新质检一次"
        );
    }
    if sources.models.len() == 1 && sources.models[0] == current {
        // 来源明确且就是当前模型：不误报
        return format!("现有 {scored} 句的分数就是 {current} 测的");
    }
    format!(
        "现有 {scored} 句的分数是 {models} 测的，当前回读模型是 {current}\
         ——换模型会改变质检口径，建议重新质检一次再看结论"
    )
}

/// 排序按钮和跳转按钮共用的可用性判据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QaActionView {
    enabled: bool,
    worst_index: Option<usize>,
    worst_row: Option<usize>,
}

fn qa_action_view(rows: &[Sentence], scores: &HashMap<usize, f64>, blocked: bool) -> QaActionView {
    let worst_index = lowest_scored_index(rows, scores);
    let worst_row = worst_index.and_then(|index| row_position_in_slice(rows, index));
    QaActionView {
        enabled: !blocked && worst_row.is_some(),
        worst_index,
        worst_row,
    }
}

fn qa_action_view_for_ui(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    state: &Rc<UiState>,
) -> QaActionView {
    qa_action_view(
        &rows_as_vec(rows),
        &state.eval_scores.borrow(),
        ui.get_running()
            || ui.get_busy()
            || ui.get_batch_running()
            || state.eval_task.get().is_some(),
    )
}

fn sync_qa_actions(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, state: &Rc<UiState>) {
    let view = qa_action_view_for_ui(ui, rows, state);
    // 来源说明与"按钮能不能点"在同一次刷新里算：两者都只在分数集 / 当前回读模型变化时变。
    // 分两处刷就会出现"按钮按当前状态算、文案还停在上一轮"（本仓反复抓的形态）。
    // 当前回读模型的**唯一入口**是 `effective_asr_model`（与 worker 请求、报告同源）。
    let current = effective_asr_model(&settings_snapshot());
    ui.set_qa_source_note(qa_source_note(&ledger_score_sources(state), &current).into());
    if rows.row_count() == 0 {
        return;
    }
    // 第 0 行只承载这两个按钮的视图元数据；不写进 Project，也不展示。
    let Some(mut row) = rows.row_data(0) else {
        return;
    };
    let enabled = view.enabled;
    let sorted = state.qa_sorted.get();
    if row.qa_enabled == enabled && row.qa_sorted == sorted {
        return;
    }
    row.qa_enabled = enabled;
    row.qa_sorted = sorted;
    rows.set_row_data(0, row);
}

/// 现有 `select-sentence` 回调里保留的质检视图指令。
/// 负值不可能与真实 UI 行号冲突；按钮和 Rust 判定共用同一套语义。
const QA_SORT_COMMAND: i32 = -1;
const QA_JUMP_WORST_COMMAND: i32 = -2;

fn toggle_qa_sort(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, state: &Rc<UiState>) {
    let view = qa_action_view_for_ui(ui, rows, state);
    if !view.enabled {
        return;
    }
    let selected_project = selected_project_index(ui.get_selected(), &rows_as_vec(rows));
    let scores = state.eval_scores.borrow().clone();
    let sorted = !state.qa_sorted.get();
    state.qa_sorted.set(sorted);
    let ordered = if sorted {
        sort_rows_for_eval(rows_as_vec(rows), &scores)
    } else {
        restore_project_order(rows_as_vec(rows))
    };
    apply_row_order(rows, ordered);
    if let Some(index) = selected_project {
        if let Some(i) = row_position(rows, index) {
            ui.set_selected(i as i32);
        }
    }
    sync_qa_actions(ui, rows, state);
    ui.set_status_text(
        if sorted {
            "已按可懂度升序排列：最差句在前（只改查看顺序）"
        } else {
            "已恢复工程原顺序"
        }
        .into(),
    );
}

fn jump_to_worst(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, state: &Rc<UiState>) {
    let view = qa_action_view_for_ui(ui, rows, state);
    if !view.enabled {
        return;
    }
    let Some(i) = view.worst_row else { return };
    let Some(row) = rows.row_data(i) else { return };
    ui.set_selected(i as i32);
    // 行高固定 48px；选中的展开行在目标位置自身，前面行仍按 48px 计算。
    // 加一个不可见的亚像素抖动，让“同一句再点一次”也重新触发滚动。
    let phase = !state.qa_scroll_phase.get();
    state.qa_scroll_phase.set(phase);
    let y = i as f32 * 48.0 + if phase { 0.01 } else { 0.0 };
    // 行序会随排序变化；把目标广播到每一行，避免第 0 行换人后滚动目标漂移。
    for row_index in 0..rows.row_count() {
        if let Some(mut row) = rows.row_data(row_index) {
            if row.qa_scroll_y == y {
                continue;
            }
            row.qa_scroll_y = y;
            rows.set_row_data(row_index, row);
        }
    }
    ui.set_status_text(format!("已跳到最差第 {} 句：可试听或重录", row.no).into());
}

/// 分数失效的核心状态迁移：清分数、取消排序视角并回到工程原顺序。
/// 拆出来让单测不用构造窗口也能验证“失效后回原序”。
fn reset_eval_view(rows: &Rc<VecModel<Sentence>>, state: &UiState) {
    clear_eval_ledger(state);
    state.qa_sorted.set(false);
    apply_row_order(rows, restore_project_order(rows_as_vec(rows)));
}

/// 分数失效的入口：清分数、取消排序视角并回到工程原顺序。
fn clear_eval_scores(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, state: &Rc<UiState>) {
    reset_eval_view(rows, state);
    sync_qa_actions(ui, rows, state);
}

fn invalidate_worker_project(cmd_tx: &Sender<Cmd>, state: &Rc<UiState>) {
    state.project_ready.set(false);
    state
        .project_revision
        .set(state.project_revision.get().wrapping_add(1));
    state.assembled.borrow_mut().take();
    // 工程作废 → 配音成品也不再可用（BGM 的混音前置条件随之失效）
    // 注意：调用方是否持有 ui 不一定，所以这里只清状态，UI 侧的 flag 由调用点刷新。
    let _ = cmd_tx.send(Cmd::InvalidateProject);
}

/// 把任务台账回灌到界面：状态栏 chip 文案 + 任务中心列表 + 三个计数。
/// 把 Rust 侧的批量行投影成 ui 模型。
///
/// 与任务中心一样：ui 只拿投影，真相在这里——否则"任务中心说失败、批量面板说合成本"
/// 这类两套状态源的分叉迟早会出现。
fn refresh_batch_rows(ui: &MainWindow, state: &Rc<UiState>) {
    let rows: Vec<BatchRow> = state
        .batch_rows
        .borrow()
        .iter()
        .map(|r| BatchRow {
            name: r.name.clone().into(),
            sentences: r.sentences as i32,
            state: r.state.label().into(),
            detail: r.detail.clone().into(),
        })
        .collect();
    ui.set_batch_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// 批量面板上那一句汇总：导入被跳过的原因 + 本次跑批的结果都要能看见。
fn refresh_batch_summary(ui: &MainWindow, state: &Rc<UiState>, tail: &str) {
    let skipped = state.batch_skipped_notes.borrow();
    let count = state.batch_rows.borrow().len();
    let mut text = if count == 0 {
        "还没导入稿件".to_string()
    } else {
        format!("{count} 篇待跑")
    };
    if !skipped.is_empty() {
        text.push_str(&format!(
            "（导入时跳过 {} 篇：{}）",
            skipped.len(),
            skipped.join("；")
        ));
    }
    if !tail.is_empty() {
        text.push_str(" · ");
        text.push_str(tail);
    }
    ui.set_batch_summary(text.into());
}

/// 批量某条收尾：台账里按 **id** 收，不走单篇那个 dub_task 槽位——
/// 批量有 N 条同时在台账里，槽位表达不了"哪一条"。
fn finish_task_by_id(
    ui: &MainWindow,
    state: &Rc<UiState>,
    id: u32,
    task_state: tasks::TaskState,
    detail: impl Into<String>,
) {
    state.tasks.borrow_mut().finish(id, task_state, detail);
    refresh_tasks(ui, state);
}

/// 这一行稿件在批量列表里的下标（按任务 id 找）。
fn batch_row_index_for_task(state: &Rc<UiState>, task_id: u32) -> Option<usize> {
    state
        .batch_rows
        .borrow()
        .iter()
        .position(|r| r.task_id == Some(task_id))
}

/// 批量任务在飞时，提交守卫要挡住单篇合成与编辑（它们会改同一批工程目录）。
fn batch_in_flight(state: &Rc<UiState>) -> bool {
    state.batch_running.get()
        || state
            .batch_rows
            .borrow()
            .iter()
            .any(|r| matches!(r.state, batch::ItemState::Running))
}

fn refresh_tasks(ui: &MainWindow, state: &Rc<UiState>) {
    let q = state.tasks.borrow();
    let counts = q.counts();
    let slots = TaskSlots::from_state(state);
    ui.set_task_rows(ModelRc::from(Rc::new(VecModel::from(task_rows(
        &q, &slots,
    )))));
    ui.set_task_pending(counts.pending as i32);
    ui.set_task_running(counts.running as i32);
    ui.set_task_failed(counts.failed as i32);
    ui.set_task_finished(counts.finished as i32);
    ui.set_task_chip(task_chip_text(&q).into());
}

/// 各类任务当前在飞的 id。任务中心给「停止」按钮的判据要用它：
/// 停止位是**按类**的（配音/BGM 共用一个、分离一个、歌曲只有排队期），
/// 光看台账里的 kind 会把"重录（也是 Dub 类）"误判成可以停。
#[derive(Clone, Default)]
struct TaskSlots {
    dub: Option<u32>,
    bgm: Option<u32>,
    sep: Option<u32>,
    song: Option<u32>,
    eval: Option<u32>,
    /// 批量队列里各条的 id。批量有 N 条同时挂在台账上，槽位（Option<u32>）表达不了，
    /// 所以单独一张表：排队中的那些要能在任务中心**逐条**取消（硬取消、不消耗算力）。
    batch: Vec<u32>,
}

impl TaskSlots {
    fn from_state(state: &Rc<UiState>) -> Self {
        Self {
            dub: state.dub_task.get(),
            bgm: state.bgm_task.get(),
            sep: state.sep_task.get(),
            song: state.song_task.get(),
            eval: state.eval_task.get(),
            batch: state
                .batch_rows
                .borrow()
                .iter()
                .filter_map(|r| r.task_id)
                .collect(),
        }
    }
}

/// 这条任务现在能不能从任务中心停掉。纯函数，便于单测（含反例）。
///
/// 判据 = 种类能力 × 当前状态 × **它是不是该 Tab 那条在飞的任务**：
///  · 配音 / BGM：运行中才有停止位（协作式，当前句/分段跑完才停）；
///  · 人声分离：排队中硬取消、运行中协作停止（上游整轮跑完丢弃结果）;
///  · 音乐制作：**只有排队中**能取消——请求发出去就中断不了，不摆假按钮；
///  · 其它（重录 / 拼装）：不显示。
fn can_stop_task(t: &tasks::Task, slots: &TaskSlots) -> bool {
    // 槽位匹配只写一份（stop_target）；这里只加"该类任务在什么状态下能停"
    match stop_target(t.id, t.kind, slots) {
        Some(StopTarget::Dub | StopTarget::Bgm) => t.state == tasks::TaskState::Running,
        Some(StopTarget::Separation) => !t.state.is_final(),
        Some(StopTarget::Song) => t.state == tasks::TaskState::Pending,
        // 质检：排队中硬取消、运行中协作停止（句间检查）
        Some(StopTarget::Eval) => !t.state.is_final(),
        // 批量：只有**排队中**能在这里停（硬取消那一条）。正在跑的那条要停就是停整批，
        // 那个动作在批量面板的「停止批量」里，这里不摆一个语义不同的同名按钮。
        Some(StopTarget::Batch) => t.state == tasks::TaskState::Pending,
        None => false,
    }
}

/// 台账 → 任务中心行（新建在前）。抽成纯函数以便单测「排队中 #N」「能不能停」这类判定，
/// 不必启动 Slint 窗口。
fn task_rows(q: &tasks::TaskQueue, slots: &TaskSlots) -> Vec<TaskRow> {
    q.tasks_newest_first()
        .map(|t| {
            // 排队中的条目把位次写进状态文案：只显示"排队中"用户不知道还要等几个
            let state = match t.state {
                tasks::TaskState::Pending => {
                    format!("排队中 #{}", q.queue_position(t.id).unwrap_or(1))
                }
                _ => t.state.label().to_string(),
            };
            TaskRow {
                id: t.id as i32,
                kind: t.kind.label().into(),
                title: t.title.clone().into(),
                state: state.into(),
                detail: task_detail(t, q.elapsed(t.id)).into(),
                progress: t.progress,
                tab: t.kind.tab(),
                can_stop: can_stop_task(t, slots),
            }
        })
        .collect()
}

/// 任务中心的副标题：阶段文案 + 已经等了/跑了多久。
///
/// 时长是**排队+运行**的总时长（`Task::enqueued_at` 起算）——用户等的是这个数，
/// 不是"轮到我之后跑了多久"。终态条目不挂时长（由各自的结果文案收尾）。
fn task_detail(t: &tasks::Task, elapsed: Option<Duration>) -> String {
    let clock = elapsed.map(format_elapsed);
    match t.state {
        tasks::TaskState::Pending => match clock {
            Some(c) => format!("已排队 {c}"),
            None => "已排队".to_string(),
        },
        tasks::TaskState::Running => match (t.detail.is_empty(), clock) {
            (false, Some(c)) => format!("{} · 已运行 {c}", t.detail),
            (false, None) => t.detail.clone(),
            (true, Some(c)) => format!("已运行 {c}"),
            (true, None) => String::new(),
        },
        _ => t.detail.clone(),
    }
}

/// 时长文案：秒 / 分 / 时三档（任务中心的"已排队 / 已运行"用）。
/// 只写已过去的整秒，不四舍五入——8 分钟的任务不该在第 7 分 30 秒时显示 8m00s。
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// 台账里有未终态任务时，按秒刷新任务中心（"已运行 N"才会走字）。
/// tick 是 40ms 一次，不能每次都重建 VecModel。
fn maybe_refresh_task_times(ui: &MainWindow, state: &Rc<UiState>) {
    if !tasks_in_flight(state) {
        return;
    }
    let now = Instant::now();
    let due = state
        .last_task_refresh
        .get()
        .map(|prev| now.duration_since(prev) >= Duration::from_secs(1))
        .unwrap_or(true);
    if due {
        state.last_task_refresh.set(Some(now));
        refresh_tasks(ui, state);
    }
}

/// 状态栏那枚 chip 的文案：优先显示"正在跑什么"，其次失败，再次完成。
fn task_chip_text(q: &tasks::TaskQueue) -> String {
    if let Some(t) = q.running() {
        let c = q.counts();
        let head = if t.kind.reports_progress() {
            let pct = (t.progress * 100.0).round() as i32;
            format!("{} {}%", t.kind.label(), pct)
        } else {
            // 整段生成的任务没有中间进度：写 0% 会让人以为卡死，改报已运行时长
            match q.elapsed(t.id) {
                Some(d) => format!("{} 已运行 {}", t.kind.label(), format_elapsed(d)),
                None => t.kind.label().to_string(),
            }
        };
        // 运行中还排着队：chip 上说清"后面还有几条"，否则用户以为只跑这一条
        if c.pending > 0 {
            return format!("任务 · {head} · 另排队 {}", c.pending);
        }
        return format!("任务 · {head}");
    }
    let c = q.counts();
    if c.pending > 0 {
        let next = q.pending().next().map(|t| t.kind.label()).unwrap_or("任务");
        return format!("任务 · 排队 {}（下一个：{}）", c.pending, next);
    }
    if let Some(t) = q.last_failed() {
        return format!("任务 · {} 个失败（{}）", c.failed, t.kind.label());
    }
    if c.finished > 0 {
        // 「结束」= 完成 + 已停止，不写"完成"以免把用户停掉的任务算成成功
        return format!("任务 · 已结束 {}", c.finished);
    }
    "任务 · 空闲".to_string()
}

/// 登记一个**立刻运行**的任务并刷新界面（配音 / BGM 这类互斥任务用：提交前已确认
/// 没有同组任务在跑）。返回任务 id，调用方要把它带进命令里（worker 用它做排队提升
/// 与取消检查）。
fn start_task(
    ui: &MainWindow,
    state: &Rc<UiState>,
    slot: &std::cell::Cell<Option<u32>>,
    kind: tasks::TaskKind,
    title: impl Into<String>,
) -> u32 {
    let id = state.tasks.borrow_mut().start(kind, title);
    slot.set(Some(id));
    refresh_tasks(ui, state);
    id
}

/// 登记一个**排队中**的任务并刷新界面（人声分离 / 音乐制作用：可以排在正在跑的任务
/// 后面，worker 轮到它时回报 TaskStarted 再提升为运行中）。
fn enqueue_task(
    ui: &MainWindow,
    state: &Rc<UiState>,
    slot: &std::cell::Cell<Option<u32>>,
    kind: tasks::TaskKind,
    title: impl Into<String>,
) -> u32 {
    let id = state.tasks.borrow_mut().enqueue(kind, title);
    slot.set(Some(id));
    refresh_tasks(ui, state);
    id
}

/// 排队任务真正开始跑时，把对应 Tab 的文案从"排队中"切成"运行中"。
/// 目前只有人声分离与音乐制作能排队；配音 / BGM 提交时就已经在跑，不需要切文案。
fn set_task_running_text(ui: &MainWindow, state: &Rc<UiState>, task_id: u32) {
    let kind = state
        .tasks
        .borrow()
        .tasks_newest_first()
        .find(|t| t.id == task_id)
        .map(|t| t.kind);
    match kind {
        Some(tasks::TaskKind::Separation) => {
            ui.set_sep_status_text("正在加载模型并分离（首次会下载约 200MB 模型）…".into());
        }
        Some(tasks::TaskKind::Eval) => {
            ui.set_status_text("质检中：正在逐句 ASR 回读…".into());
        }
        Some(tasks::TaskKind::Song) => {
            // 轮到它了："取消排队"的窗口关闭（已发出的歌曲请求中断不了，不给假停止）
            ui.set_song_queued(false);
            ui.set_song_status_text(
                "歌曲生成中（yue2 可能约 8 分钟，ACE-Step 120s 约 6.4 分钟）…".into(),
            );
        }
        _ => {}
    }
}

/// 台账里是否还有没跑完的任务（排队中或运行中）。
///
/// 配音 / BGM 这类互斥任务用它做提交前提：它们会改写 worker 持有的工程，
/// 不能与别的任务并行——判据从"几个散落的 busy 标志"收敛成台账一处。
fn tasks_in_flight(state: &Rc<UiState>) -> bool {
    // 演示态（AW_UI_STATE=tasks）里的条目没有对应 worker 命令：它们只用于截图，
    // 不能把真实提交守卫挡住。
    if state.demo_tasks.get() {
        return false;
    }
    let c = state.tasks.borrow().counts();
    c.pending + c.running > 0
}

/// 排队位次文案（1 基，位次只数排队项）。已在跑 / 已结束的条目返回 None。
fn queue_note(state: &Rc<UiState>, id: u32) -> Option<String> {
    let q = state.tasks.borrow();
    let pos = q.queue_position(id)?;
    // 有人正在跑 → 如实说排在第几、在等谁；空闲提交（pos 恒为 1）不提示，
    // 免得刚点下按钮就闪一下"排队中"，随后立刻被 TaskStarted 改成运行中
    q.running()
        .map(|t| format!("排队中 #{pos} · 当前在跑：{}", t.kind.label()))
}

/// 收尾一个任务并刷新（slot 里没有 id 时是空操作：例如启动前就失败）。
fn finish_task(
    ui: &MainWindow,
    state: &Rc<UiState>,
    slot: &std::cell::Cell<Option<u32>>,
    task_state: tasks::TaskState,
    detail: impl Into<String>,
) {
    let Some(id) = slot.take() else { return };
    state.tasks.borrow_mut().finish(id, task_state, detail);
    refresh_tasks(ui, state);
}

fn progress_task(
    ui: &MainWindow,
    state: &Rc<UiState>,
    slot: &std::cell::Cell<Option<u32>>,
    progress: f32,
    detail: impl Into<String>,
) {
    let Some(id) = slot.get() else { return };
    state.tasks.borrow_mut().progress(id, progress, detail);
    refresh_tasks(ui, state);
}

/// 工程编辑守卫：合成/其它任务在飞，**或质检在飞**。
///
/// 质检结果按"第 N 句"报出来；质检期间改稿会让这个序号指向另一句话，所以质检
/// （含排队中）也要挡住稿件/工程名编辑。质检本身很短（N×0.3s 量级）。
fn project_editing_blocked(ui: &MainWindow, state: &Rc<UiState>) -> bool {
    ui.get_running() || ui.get_busy() || state.eval_task.get().is_some()
}

/// 导出分轨时，"这一套 BGM 结果还算不算当前"。
///
/// 判据是 `has_result && !stale`：改 BGM 描述后 UI 只置 `stale`（结果还在、还能试听），
/// 所以光看 `has_result` 会把旧结果当当前成品导出去（复核指出）。从磁盘恢复的那套
/// 一律按 stale 处理（见 `restore_bgm_from_disk`）。
fn bgm_result_exportable(has_result: bool, stale: bool) -> bool {
    has_result && !stale
}

/// 把一套 BGM 产物灌进界面（含"有哪几轨显示哪几轨"）。
///
/// 抽出来是为了让**从磁盘恢复**（重开应用打开旧工程）与"刚跑完混音"走同一段界面更新，
/// 不然两条路径的轨道行/标签迟早不一致。
fn apply_bgm_artifacts(ui: &MainWindow, state: &Rc<UiState>, artifacts: &BgmArtifacts) {
    ui.set_bgm_has_result(true);
    ui.set_bgm_stale(false);
    ui.set_bgm_progress(1.0);
    ui.set_bgm_has_voice_track(artifacts.voice.is_some());
    ui.set_bgm_has_mixed_track(artifacts.mixed.is_some());
    ui.set_bgm_voice_label(
        artifacts
            .voice
            .as_ref()
            .map(|p| format!("人声 · {}", file_label(p)))
            .unwrap_or_default()
            .into(),
    );
    ui.set_bgm_track_label(format!("BGM · {}", file_label(&artifacts.bgm)).into());
    ui.set_bgm_mixed_label(
        artifacts
            .mixed
            .as_ref()
            .map(|p| format!("混音 · {}", file_label(p)))
            .unwrap_or_default()
            .into(),
    );
    *state.bgm_artifacts.borrow_mut() = Some(artifacts.clone());
}

/// 从磁盘恢复 BGM 产物（启动打开旧工程时用）：只恢复**仍然配套**的那套——
/// 独立生成的 BGM（没有配音成品）直接可用；混音过的要过 `mix_is_current`，
/// 配音成品变过就说明这套混音过期了，宁可不显示也不拿旧结果当当前产物。
fn restore_bgm_from_disk(ui: &MainWindow, state: &Rc<UiState>, dir: &Path) {
    if !dir.join("bgm/bgm.wav").is_file() {
        return;
    }
    let ctx = bgm_context(ui);
    // 参数（已持久化）与配音指纹都对得上 → 这套仍是当前结果，可以直接导分轨；
    // 对不上（改过描述/改过稿、或老工程没有清单）→ 只能查看/试听。
    let current = export::bgm_result_is_current(dir, &ctx.options_digest);
    if let Ok(artifacts) = aw_core::bgm_artifacts_from_disk(dir) {
        apply_bgm_artifacts(ui, state, &artifacts);
        if current {
            ui.set_bgm_status_text(
                format!(
                    "已恢复上次的 BGM 产物：{} 段 · 成品 {:.1}s（参数与配音成品都对得上，可直接导分轨）",
                    artifacts.segments, artifacts.duration
                )
                .into(),
            );
        } else {
            ui.set_bgm_stale(true);
            ui.set_bgm_status_text(
                format!(
                    "上次的 BGM 产物（{} 段 · {:.1}s）与当前参数/配音成品对不上，仅供查看/试听；要导分轨请重新生成并混音（分段有缓存）",
                    artifacts.segments, artifacts.duration
                )
                .into(),
            );
        }
    }
}

fn reset_bgm(ui: &MainWindow, state: &Rc<UiState>) {
    state.bgm_artifacts.borrow_mut().take();
    ui.set_bgm_has_result(false);
    ui.set_bgm_progress(0.0);
}

fn wire_script(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state0 = state.clone();
    ui.on_project_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &state0) {
            ui.set_status_text("任务进行中：工程名暂不可改".into());
            return;
        }
        let stem = file_stem(&ui.get_project_name());
        *state0.project_dir.borrow_mut() = Some(project_dir(&stem));
        invalidate_worker_project(&tx, &state0);
        reset_bgm(&ui, &state0);
        ui.set_has_result(false);
        // 分离结果和历史都属于工程目录；换工程名后要切到新目录，
        // 不能把上一个工程的两轨当成当前结果继续试听/导出。
        clear_separation_result(&ui, &state0);
        refresh_separation_history(&ui, &state0);
        refresh_versions(&ui, &state0);
        ui.set_status_text(format!("工程名：{}", ui.get_project_name()).into());
    });

    // 停顿输入：只给即时反馈，不打断输入（真正的回写在 gap_ms_from_ui 里）
    let weak = ui.as_weak();
    ui.on_gap_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_status_text(gap_hint_for(&ui.get_gap_ms_text()).into());
    });

    let weak = ui.as_weak();
    let rows1 = rows.clone();
    let tx1 = cmd_tx.clone();
    let state1 = state.clone();
    ui.on_script_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &state1) {
            ui.set_status_text("任务进行中：等这轮跑完或先停止，再编辑稿件".into());
            return;
        }
        let text = ui.get_script_text();
        rebuild(&ui, &rows1, &text);
        invalidate_worker_project(&tx1, &state1);
        clear_eval_scores(&ui, &rows1, &state1);
        reset_bgm(&ui, &state1);
        ui.set_status_text(
            format!(
                "稿件已更新：{} 字 / {} 句 · 状态已重置（重跑合成时自动续作未变句）",
                text.chars().count(),
                rows1.row_count()
            )
            .into(),
        );
    });

    let weak = ui.as_weak();
    let rows2 = rows.clone();
    let tx2 = cmd_tx.clone();
    let state2 = state.clone();
    ui.on_use_sample(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &state2) {
            ui.set_status_text("任务进行中：暂不能载入示例稿".into());
            return;
        }
        ui.set_script_text(SAMPLE_SCRIPT.into());
        rebuild(&ui, &rows2, SAMPLE_SCRIPT);
        invalidate_worker_project(&tx2, &state2);
        clear_eval_scores(&ui, &rows2, &state2);
        reset_bgm(&ui, &state2);
        ui.set_status_text(format!("已载入示例稿：{} 句", rows2.row_count()).into());
    });

    let weak = ui.as_weak();
    let rows3 = rows.clone();
    let tx3 = cmd_tx.clone();
    let state3 = state.clone();
    ui.on_clear_script(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &state3) {
            ui.set_status_text("任务进行中：暂不能清空稿件".into());
            return;
        }
        ui.set_script_text("".into());
        rebuild(&ui, &rows3, "");
        invalidate_worker_project(&tx3, &state3);
        clear_eval_scores(&ui, &rows3, &state3);
        reset_bgm(&ui, &state3);
        ui.set_status_text("稿件已清空，粘一段口播稿试试".into());
    });

    let weak = ui.as_weak();
    let rows4 = rows.clone();
    let tx4 = cmd_tx.clone();
    let state4 = state.clone();
    ui.on_resplit(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &state4) {
            ui.set_status_text("任务进行中：暂不能重新切句".into());
            return;
        }
        let text = ui.get_script_text();
        rebuild(&ui, &rows4, &text);
        invalidate_worker_project(&tx4, &state4);
        clear_eval_scores(&ui, &rows4, &state4);
        reset_bgm(&ui, &state4);
        let n = rows4.row_count();
        ui.set_status_text(format!("已重新切句：{n} 句").into());
        toast(&ui, &format!("已重新切句：{n} 句"));
    });
}

/// 任务中心：跳转到任务所属 Tab、清除已完成。停止 / 重试仍在各 Tab 自己做
/// （那里已经知道该重试什么参数，任务中心不复制这套状态）。
/// 截图/演示用：`AW_UI_STATE=tasks` 时灌三条示例任务并打开任务中心
/// （仅 debug 构建存在，release 被编译掉）。
/// `AW_UI_STATE=bgm-done` 时给结果区塞一份**真实存在的** wav 当三轨：
/// 示例态下点「试听」「导出」也能真跑，而不只是渲染核对。
/// （找不到样例文件就什么都不做，界面仍显示标签，只是点了会提示"还没有成品"。）
#[cfg(debug_assertions)]
fn seed_shot_bgm_artifacts(ui: &MainWindow, state: &Rc<UiState>) {
    if std::env::var("AW_UI_STATE").as_deref() != Ok("bgm-done") {
        return;
    }
    let candidates = [
        PathBuf::from("/tmp/aw-sep-src.wav"),
        project_dir(&file_stem(&ui.get_project_name())).join("out/final.wav"),
    ];
    let Some(sample) = candidates.into_iter().find(|p| p.is_file()) else {
        return;
    };
    *state.bgm_artifacts.borrow_mut() = Some(artifacts_like(&sample));
}

#[cfg(not(debug_assertions))]
#[allow(dead_code)]
fn seed_shot_bgm_artifacts(_ui: &MainWindow, _state: &Rc<UiState>) {}

#[cfg(debug_assertions)]
fn artifacts_like(sample: &Path) -> BgmArtifacts {
    BgmArtifacts {
        voice: Some(sample.to_path_buf()),
        bgm: sample.to_path_buf(),
        mixed: Some(sample.to_path_buf()),
        srt: Some(sample.with_extension("srt")),
        duration: 4.27,
        segments: 5,
    }
}

/// 截图/演示：`AW_UI_STATE=batch` 时给批量面板灌三行示例稿子并展开面板。
///
/// 与 `AW_UI_STATE=tasks` 同理：这些行**没有**对应的 worker 命令，所以不登记任务、
/// 也不置 `batch_running`——否则启动守卫会把真实的"开始批量"挡下来。
#[cfg(debug_assertions)]
fn seed_shot_batch(ui: &MainWindow, state: &Rc<UiState>) {
    if std::env::var("AW_UI_STATE").as_deref() != Ok("batch") {
        return;
    }
    {
        let mut rows = state.batch_rows.borrow_mut();
        for (name, sentences, st, detail) in [
            (
                "第一集 · 开场",
                42,
                batch::ItemState::Done,
                "已出片 · 复用 8 句",
            ),
            (
                "第二集 · 正片",
                57,
                batch::ItemState::Running,
                "第 12/57 句",
            ),
            ("第三集 · 彩蛋", 0, batch::ItemState::Waiting, "排队中"),
        ] {
            rows.push(BatchRowState {
                name: name.to_string(),
                script: String::new(),
                sentences,
                task_id: None,
                state: st,
                detail: detail.to_string(),
                out: None,
            });
        }
    }
    ui.set_batch_open(true);
    refresh_batch_rows(ui, state);
    refresh_batch_summary(ui, state, "示例数据：批量队列一次导入 N 篇稿子");
    ui.set_status_text("批量队列（示例数据）：每篇 = 一个工程，逐条成片".into());
}

#[cfg(debug_assertions)]
fn seed_shot_tasks(ui: &MainWindow, state: &Rc<UiState>) {
    if std::env::var("AW_UI_STATE").as_deref() != Ok("tasks") {
        return;
    }
    // 演示任务不入调度：`tasks_in_flight` 会因为这个标记直接返回 false
    state.demo_tasks.set(true);
    let running = state
        .tasks
        .borrow_mut()
        .start(tasks::TaskKind::Dub, "配音 · 34 句");
    state
        .tasks
        .borrow_mut()
        .progress(running, 0.35, "第 12/34 句已完成");

    let failed = state
        .tasks
        .borrow_mut()
        .start(tasks::TaskKind::Bgm, "BGM · 生成并混音");
    state.tasks.borrow_mut().finish(
        failed,
        tasks::TaskState::Failed,
        "磁盘不足：目标卷剩余 0.2GB",
    );
    let done = state
        .tasks
        .borrow_mut()
        .start(tasks::TaskKind::Song, "音乐制作 · 生成歌曲");
    state
        .tasks
        .borrow_mut()
        .finish(done, tasks::TaskState::Done, "成品 168.0s");
    // 排队中的条目：截图/演示也要能看到「排队中 #N」这一档
    // （**不**占真实槽位：演示任务不是真在飞，占了会让启动守卫把它当成真任务）
    state
        .tasks
        .borrow_mut()
        .enqueue(tasks::TaskKind::Separation, "人声分离 · 频道口播");
    ui.set_task_center_open(true);
    refresh_tasks(ui, state);
    ui.set_status_text("任务中心：跨 Tab 任务台账（示例数据，切 Tab 不会取消任务）".into());
}

/// 停止配音合成（协作式：当前句跑完停）。Tab 按钮与任务中心共用这一份。
fn stop_dub_run(ui: &MainWindow, stop: &Arc<AtomicBool>) {
    if !ui.get_running() {
        return;
    }
    stop.store(true, Ordering::Relaxed);
    ui.set_status_text("正在停止：当前句合成完就停".into());
}

/// 停止 BGM（协作式：当前分段跑完停；已完成分段留在目录里可复用）。
fn stop_bgm_run(ui: &MainWindow, stop: &Arc<AtomicBool>) {
    if !ui.get_busy() && !ui.get_running() {
        return;
    }
    stop.store(true, Ordering::Relaxed);
    ui.set_bgm_status_text("停止中：当前分段跑完才停…".into());
    ui.set_status_text("BGM 停止中…".into());
}

/// 停止人声分离：排队中 = 立刻出队（登记取消，worker 取到就不执行）；
/// 运行中 = 协作停止（上游没有取消 API，整轮跑完再丢弃结果，耗时照算）。
fn stop_separation(ui: &MainWindow, state: &Rc<UiState>, sep_stop: &Arc<AtomicBool>) {
    if !ui.get_sep_busy() {
        return;
    }
    // 还排在队列里 → 立刻终态：登记取消，worker 轮到时不会执行它
    // （采纳自 Xmusic-splitter 的 per-job registry：取消按 task_id 定位）
    let queued = state
        .sep_task
        .get()
        .and_then(|id| state.tasks.borrow().queue_position(id).map(|_| id));
    if let Some(id) = queued {
        state.cancel.cancel(id);
        ui.set_sep_busy(false);
        ui.set_sep_progress(0.0);
        ui.set_sep_has_result(false);
        ui.set_sep_result_available(false);
        state.sep_tracks.borrow_mut().take();
        let note = "已从队列中移除（还没开始跑，没有消耗算力）".to_string();
        ui.set_sep_status_text(note.clone().into());
        ui.set_status_text(note.clone().into());
        finish_task(ui, state, &state.sep_task, tasks::TaskState::Stopped, note);
        return;
    }
    sep_stop.store(true, Ordering::Relaxed);
    // 如实说：上游没有取消 API，should_stop 只在整轮分离返回后被查（aw-core
    // separate_tracks 的实现），所以这里只是"跑完丢弃、不落盘"，耗时照算
    ui.set_sep_status_text("停止中：本轮分离跑完才会丢弃结果（上游没有取消接口）…".into());
}

/// 合成消息到达时，该不该作废这一句的质检分数。
///
/// 语义 = **开始重做就作废**（aw-core 在同一时刻清并把"已作废"落盘，保证磁盘上不会出现
/// "新音频 + 旧分数"）。所以 running / done / error 都要清，只有 pending 例外。
/// 抽成函数是为了让这条语义有单测钉住（复核抓过"两边说法不一"）。
fn sentence_message_invalidates_score(status: &str) -> bool {
    matches!(status, "running" | "done") || status.starts_with("error")
}

/// `error: oom: 内存不足…` → `oom: 内存不足…`（只去掉协议前缀，保留可执行文案）。
fn sentence_error_detail(status: &str) -> &str {
    status
        .strip_prefix("error")
        .unwrap_or(status)
        .trim_start_matches(':')
        .trim()
}

/// 一轮配音结束后给用户看的唯一状态文案。
///
/// 失败时把第一条错误原文（可能是 OOM 的模型/内存/三个动作）带出来，并明确
/// 下一次「继续合成」只重跑失败句——不能只显示一个失败计数让用户自己猜。
fn run_finished_note(
    failed: usize,
    stopped: bool,
    reused: usize,
    done: i32,
    total: i32,
    error: Option<&str>,
) -> String {
    let reused_note = if reused > 0 {
        format!("（复用 {reused} 句）")
    } else {
        String::new()
    };
    if stopped {
        format!("已停止：完成 {done}/{total} 句{reused_note}，可随时继续")
    } else if failed > 0 {
        let detail = error
            .map(sentence_error_detail)
            .filter(|d| !d.is_empty())
            .map(|d| format!("：{d}"))
            .unwrap_or_default();
        format!(
            "本轮完成 {done}/{total} 句{reused_note}（{failed} 句失败{detail}；点「继续合成」只重跑失败句）"
        )
    } else {
        format!("合成完成 {done}/{total} 句{reused_note}：可试听、可导出 WAV / SRT")
    }
}

/// 本轮**没评上、但旧分仍在工程里**的那些句子 → 补进本次结果。
///
/// 关键在来源：这些分是**当年那次质检**测的，来源必须用 `sen.eval_model` **原样带回**，
/// 不能盖成本轮这个 `model`。盖错的形态是"冒充匹配"——部分 ASR 失败 + 用户换过回读模型时，
/// 界面会对那几句说「现有的分数就是 <本轮模型> 测的」，而这正是 `eval_model` 要消灭的假结论。
///
/// 抽成纯函数是为了给它测试隔离：原先这段内联在 worker 的评测循环里，
/// 把它改成 `Some(model.clone())` 整仓用例**一条都不红**（复核实测的零覆盖）。
fn carried_over_scores(
    project: &Project,
    already_scored: &[(usize, f64, Option<String>)],
) -> Vec<(usize, f64, Option<String>)> {
    let mut out = Vec::new();
    for sen in &project.sentences {
        let Some(percent) = sen.eval_percent else {
            continue;
        };
        // 与 `eval_ledger_from_project` 同一条口径：只认 done，失败/待合成的旧分不贴
        if sen.status != "done" {
            continue;
        }
        if already_scored.iter().any(|(i, _, _)| *i == sen.index) {
            continue;
        }
        out.push((sen.index, percent, sen.eval_model.clone()));
    }
    out
}

/// 从工程里取质检台账：**只接受状态是「已合成」的句子**。
///
/// 失败/待合成的句子即使文件里还留着旧分数也不贴出来——那种分数描述的不是当前这句
/// 可用的音频（复核建议：回灌要按状态过滤）。
///
/// 每项带 `Option<String>` 的来源模型。**旧工程缺这个字段时是 `None`，要保持 `None`**：
/// 那是"来源未知"，不是"没测过"（没测过由 `eval_percent == None` 表达，根本不在这里）。
fn eval_ledger_from_project(project: &Project) -> Vec<(usize, f64, Option<String>)> {
    project
        .sentences
        .iter()
        .filter(|s| s.status == "done")
        .filter_map(|s| s.eval_percent.map(|p| (s.index, p, s.eval_model.clone())))
        .collect()
}

/// 把一次成功的回读结果写进工程：**分数与来源模型同进同出**。
///
/// 抽成函数有两个理由：
/// · 报告念的模型（`qa_report_markdown(.., &model, ..)`）与工程里记的来源必须是
///   **同一个入参**；两处各写一份 `model.clone()` 就会漂移成"报告写 A、工程记 B"；
/// · app 侧写 `eval_percent` 只允许在这里一处（源码守卫钉住）。
fn record_eval_score(project: &mut Project, index: usize, percent: f64, model: &str) {
    if let Some(sen) = project.sentences.iter_mut().find(|s| s.index == index) {
        sen.eval_percent = Some(percent);
        sen.eval_model = Some(model.to_string());
    }
}

/// 质检台账（分数 + 来源）的**唯一整体替换入口**。
///
/// 两份 map 分开存，是因为 `eval_scores` 的消费者（排序 / 最差句 / 行标签）只关心分数，
/// 不值得为来源模型改它们的签名；代价就是这条约束：**只能从这里写**。
fn replace_eval_ledger(state: &UiState, entries: &[(usize, f64, Option<String>)]) {
    let mut scores = state.eval_scores.borrow_mut();
    let mut models = state.eval_models.borrow_mut();
    scores.clear();
    models.clear();
    for (index, percent, model) in entries {
        scores.insert(*index, *percent);
        models.insert(*index, model.clone());
    }
}

/// 台账整体清空（分数与来源一起清）。
fn clear_eval_ledger(state: &UiState) {
    state.eval_scores.borrow_mut().clear();
    state.eval_models.borrow_mut().clear();
}

/// 摘掉一句的台账（返回"原来有没有分数"）。分数与来源一起摘。
fn drop_eval_ledger(state: &UiState, index: usize) -> bool {
    let had = state.eval_scores.borrow_mut().remove(&index).is_some();
    state.eval_models.borrow_mut().remove(&index);
    had
}

/// 质检分数在句子行上的标签。低于阈值加 ⚠ 前缀提醒看一眼。
///
/// 95% 是**启发式**（不是质量门槛）：CHARTER 记的基线是 98.5~100%，留一段余量；
/// 触发后用户可以直接在展开行点「重录」。
fn eval_label(percent: f64) -> String {
    if percent < 95.0 {
        format!("⚠ 可懂度 {percent:.1}%")
    } else {
        format!("可懂度 {percent:.1}%")
    }
}

/// 把质检分数写进句子行（没有分数的行清空标签）。
fn apply_eval_labels(rows: &Rc<VecModel<Sentence>>, scores: &HashMap<usize, f64>) {
    for i in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(i) else {
            continue;
        };
        // 行位置可能已经被质检排序改变，分数必须按工程 index 找，不能按 i 找。
        let label = usize::try_from(row.index)
            .ok()
            .and_then(|index| scores.get(&index))
            .map(|p| eval_label(*p))
            .unwrap_or_default();
        if row.eval_label.as_str() == label {
            continue;
        }
        row.eval_label = SharedString::from(label);
        rows.set_row_data(i, row);
    }
}

/// 质检报告（Markdown）：逐句对照表 + 汇总。抽成纯函数便于单测。
///
/// 为什么要有报告：分数留在工程里是为了跨会话查看，但**逐句的"参考 vs 回读"**只有当场才拿得到；
/// 写成文件后用户能存档、对比两次改稿、或者贴到笔记里，不用一边听一边记。
fn qa_report_markdown(
    project_name: &str,
    model: &str,
    rows: &[EvalRow],
    percent: f64,
    scored: usize,
    asr_failed: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# 质检报告 · {project_name}\n\n"));
    out.push_str(&format!("- 回读模型：{model}\n"));
    // 总句数 = 本次评分的 + 转写失败的（rows 只含评上分的，用 rows.len() 会把失败句漏掉）
    out.push_str(&format!(
        "- 句数：{}（评分 {scored}，转写失败 {asr_failed}）\n",
        rows.len() + asr_failed
    ));
    if scored > 0 {
        out.push_str(&format!("- 平均可懂度：{percent:.1}%\n"));
    } else {
        out.push_str("- 平均可懂度：未能评分（本次没有一句转写成功）\n");
    }
    out.push_str("\n| 句 | 可懂度 | 参考 | 回读 | 首个差异 |\n");
    out.push_str("|---|---|---|---|---|\n");
    for r in rows {
        // 表格里换行/竖线会破版：转义掉
        let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
        out.push_str(&format!(
            "| {} | {:.1}% | {} | {} | {} |\n",
            r.index + 1,
            r.percent,
            cell(&r.reference),
            cell(&r.hypothesis),
            cell(&r.snippet)
        ));
    }
    out
}

/// 质检完成后的摘要文案（抽成纯函数：全失败 / 全一致 / 有最差句三种要分开说，
/// 否则"0 句评上分"会被说成"平均 0%"甚至"全部一致"——复核抓到过）。
fn eval_summary_note(summary: &EvalSummary) -> String {
    // 先出"这次质检的结论"……
    let mut note = if summary.scored == 0 {
        format!(
            "质检未能评分：{} 句 ASR 转写都失败了（检查 ASR 模型/服务）",
            summary.asr_failed
        )
    } else {
        let mut n = format!(
            "质检完成：平均可懂度 {:.1}%（{} 句）",
            summary.percent, summary.scored
        );
        if summary.asr_failed > 0 {
            n.push_str(&format!("·{} 句转写失败", summary.asr_failed));
        }
        match summary.worst.first() {
            Some(worst) if worst.snippet.is_empty() => n.push_str(&format!(
                "·最差 第 {} 句 {:.1}%",
                worst.index + 1,
                worst.percent
            )),
            Some(worst) => n.push_str(&format!(
                "·最差 第 {} 句 {:.1}%：{}",
                worst.index + 1,
                worst.percent,
                worst.snippet
            )),
            None => n.push_str("·全部一致"),
        }
        n
    };
    if let Some(err) = &summary.asr_error {
        note.push_str(&format!("·转写服务说明：{err}"));
    }
    // ……再追加"东西有没有落盘"。**两种分支都要走到这里**：一句都没评上分时报告照样写了，
    // 写失败/分数没落盘也得让用户看见（复核指出旧写法在 scored==0 时提前 return，把这些吞了）。
    if let Some(warn) = &summary.persist_warning {
        note.push_str(&format!("·（{warn}）"));
    }
    if let Some(path) = &summary.report_path {
        note.push_str(&format!("·报告 {}", file_label(path)));
    }
    // 回读模型念出来：报告的"回读模型"与状态行这一点必须是同一个值，否则用户
    // 换过模型之后两边说法会不一致（而且报告是要存档的，事后更没法核对）。
    note.push_str(&format!("·回读 {}", summary.model));
    note
}

/// 停止质检：排队中 = 立刻出队；运行中 = 协作停止（当前句转写完就停，ASR 调用中断不了）。
fn stop_eval(ui: &MainWindow, state: &Rc<UiState>, eval_stop: &Arc<AtomicBool>) {
    let queued = state
        .eval_task
        .get()
        .and_then(|id| state.tasks.borrow().queue_position(id).map(|_| id));
    if let Some(id) = queued {
        state.cancel.cancel(id);
        let note = "已从队列中移除（还没开始跑，没有消耗算力）".to_string();
        ui.set_status_text(note.clone().into());
        finish_task(ui, state, &state.eval_task, tasks::TaskState::Stopped, note);
        return;
    }
    eval_stop.store(true, Ordering::Relaxed);
    ui.set_status_text("质检停止中：当前句转写完就停（ASR 调用没法中断）…".into());
}

/// 取消排队中的歌曲（运行中的歌曲请求发出去就中断不了，这里只处理排队期）。
fn stop_song(ui: &MainWindow, state: &Rc<UiState>) {
    let queued = state
        .song_task
        .get()
        .and_then(|id| state.tasks.borrow().queue_position(id).map(|_| id));
    let Some(id) = queued else {
        ui.set_status_text("歌曲请求已发出，本轮无法中断（服务端不支持取消）".into());
        return;
    };
    state.cancel.cancel(id);
    ui.set_song_busy(false);
    ui.set_song_queued(false);
    ui.set_song_has_result(false);
    let note = "已从队列中移除（还没开始跑，没有消耗算力）".to_string();
    ui.set_song_status_text(note.clone().into());
    ui.set_status_text(note.clone().into());
    finish_task(ui, state, &state.song_task, tasks::TaskState::Stopped, note);
}

/// 任务中心点「停止」：按种类分派到上面那几个共享函数。
///
/// 每个 Tab 同时最多一条在飞，所以"这条任务的 id 是否等于该 Tab 槽位里的 id"
/// 就足以确认点的是同一条；对不上（例如列表里更早的那条）就什么都不做。
/// 任务中心「停止」要停哪一类（纯判定，便于单测"错误的 task_id 不会停当前任务"）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StopTarget {
    Dub,
    Bgm,
    Separation,
    Song,
    Eval,
    /// 批量队列里的**排队中**那一条：硬取消（worker 轮到它时不会执行）
    Batch,
}

/// 判定 (task_id, kind) 对应哪个停止位。**必须同时匹配槽位**——列表里更早的那条
/// 任务 id 不等于该 Tab 槽位里的 id，那种点击什么都不该发生。
fn stop_target(task_id: u32, kind: tasks::TaskKind, slots: &TaskSlots) -> Option<StopTarget> {
    match kind {
        // 批量条目也是 Dub 类，先按批量表认领（它不在 dub 槽位里）
        tasks::TaskKind::Dub if slots.batch.contains(&task_id) => Some(StopTarget::Batch),
        tasks::TaskKind::Dub if slots.dub == Some(task_id) => Some(StopTarget::Dub),
        tasks::TaskKind::Bgm if slots.bgm == Some(task_id) => Some(StopTarget::Bgm),
        tasks::TaskKind::Separation if slots.sep == Some(task_id) => Some(StopTarget::Separation),
        tasks::TaskKind::Song if slots.song == Some(task_id) => Some(StopTarget::Song),
        tasks::TaskKind::Eval if slots.eval == Some(task_id) => Some(StopTarget::Eval),
        _ => None,
    }
}

/// 取消批量里**排队中**的一条：登记取消（worker 轮到它时直接跳过、不消耗算力），
/// 列表行与台账同时收尾——两处都要动，否则界面说已取消、任务中心还挂着"排队中"。
fn stop_batch_item(ui: &MainWindow, state: &Rc<UiState>, task_id: u32) {
    state.cancel.cancel(task_id);
    if let Some(i) = batch_row_index_for_task(state, task_id) {
        let mut rows = state.batch_rows.borrow_mut();
        rows[i].state = batch::ItemState::Skipped;
        rows[i].detail = "排队中被取消，没有跑".into();
    }
    let note = "已从队列中移除（还没开始跑，没有消耗算力）".to_string();
    ui.set_status_text(note.clone().into());
    finish_task_by_id(ui, state, task_id, tasks::TaskState::Stopped, &note);
    refresh_batch_rows(ui, state);
    refresh_batch_summary(ui, state, &note);
}

fn stop_task_from_center(
    ui: &MainWindow,
    state: &Rc<UiState>,
    stop: &Arc<AtomicBool>,
    sep_stop: &Arc<AtomicBool>,
    eval_stop: &Arc<AtomicBool>,
    task_id: u32,
) {
    let slots = TaskSlots::from_state(state);
    let kind = state
        .tasks
        .borrow()
        .tasks_newest_first()
        .find(|t| t.id == task_id)
        .map(|t| t.kind);
    match kind.and_then(|k| stop_target(task_id, k, &slots)) {
        Some(StopTarget::Dub) => stop_dub_run(ui, stop),
        Some(StopTarget::Bgm) => stop_bgm_run(ui, stop),
        Some(StopTarget::Separation) => stop_separation(ui, state, sep_stop),
        Some(StopTarget::Song) => stop_song(ui, state),
        Some(StopTarget::Eval) => stop_eval(ui, state, eval_stop),
        Some(StopTarget::Batch) => stop_batch_item(ui, state, task_id),
        None => {}
    }
}

/// 批量队列（M4-P1）：导入 → 提交 → 停止 → 清空。
///
/// 提交走的是 `Cmd::RunBatch`（与单篇同一条 worker 链路），每条在任务台账里
/// 各登记一条配音任务——所以任务中心能看到 N 条、排队中的那些也能单独取消。
fn wire_batch(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    msg_tx: &Sender<WorkerMsg>,
    state: &Rc<UiState>,
    stop: &Arc<AtomicBool>,
) {
    // ① 导入稿件（系统多选文件框，后台线程回消息）
    let weak = ui.as_weak();
    let msg = msg_tx.clone();
    let st = state.clone();
    ui.on_batch_import(move || {
        let Some(ui) = weak.upgrade() else { return };
        if st.batch_running.get() {
            ui.set_status_text("批量正在跑：先停止或等它跑完，再换稿件".into());
            return;
        }
        ui.set_batch_summary("正在打开文件选择框（可多选）…".into());
        spawn_scripts_pick(msg.clone());
    });

    // ② 开始批量
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    let stop1 = Arc::clone(stop);
    ui.on_batch_start(move || {
        let Some(ui) = weak.upgrade() else { return };
        if batch_in_flight(&st) {
            ui.set_status_text("批量已经在跑了".into());
            return;
        }
        // 与单篇同一套提交守卫：正在跑别的长任务时不提交，否则用户此刻看的进度
        // 会与批量里那一条串在一起
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&st) {
            ui.set_status_text("有任务正在进行：等它结束或先停止，再开始批量".into());
            return;
        }
        let idx = ui.get_voice_index();
        let Some(v) = (idx >= 0)
            .then(|| ui.get_voices().row_data(idx as usize))
            .flatten()
        else {
            ui.set_batch_summary("没有可用引擎：先在本机 audio.cpp 服务里配置 tts 模型".into());
            return;
        };
        let model = v.name.to_string();
        let (voice_ref, voice_ref_text) = voice_input_from_ui(&ui);
        // 与单篇同一条前置拦截：批量里 10 篇 × N 句全红是最坏的失败形态。
        // 按**当前引擎**判断：只有它要求参考文本（如 audio8-tts）且填了参考音
        // 且文本为空才拦；可选引擎（index-tts2）放行。
        if reference_text_missing(v.requires_reference_text, &voice_ref, &voice_ref_text) {
            ui.set_batch_summary(reference_text_required_note(&model).into());
            return;
        }
        // 唯一入口 prepare：批量每篇都会带同一份 voice_ref 发克隆请求，超 15s
        // 自动取前 15 秒（拦一次顶 N 次引擎崩溃），裁剪失败才红字拦截。
        let mut trim_note: Option<String> = None;
        let voice_ref = match voice_ref {
            Some(path) => match prepare_reference_for_clone(&path, voice_ref_text.as_deref()) {
                Ok(prepared) => {
                    trim_note = prepared.note;
                    Some(prepared.path.to_string_lossy().into_owned())
                }
                Err(note) => {
                    ui.set_batch_summary(note.into());
                    return;
                }
            },
            None => None,
        };

        // 每条先登记成一条配音任务（排队中），worker 轮到它时用 TaskStarted 抬成运行中
        let mut items = Vec::new();
        {
            let mut rows = st.batch_rows.borrow_mut();
            let mut q = st.tasks.borrow_mut();
            for row in rows.iter_mut() {
                let id = q.enqueue(tasks::TaskKind::Dub, format!("配音 · {}（批量）", row.name));
                row.task_id = Some(id);
                row.sentences = 0;
                row.state = batch::ItemState::Waiting;
                row.detail = "排队中".into();
                items.push(BatchCmdItem {
                    task_id: id,
                    name: row.name.clone(),
                    script: row.script.clone(),
                });
            }
        }
        if items.is_empty() {
            return;
        }
        let total = items.len();
        stop1.store(false, Ordering::Relaxed);
        st.batch_running.set(true);
        ui.set_batch_running(true);
        ui.set_status_text(
            match trim_note {
                // 信息提示留在状态行上（随后的"批量已提交"会盖掉它），拼在前面。
                Some(trimmed) => {
                    format!("{trimmed} · 批量已提交：{total} 篇按顺序跑（任务中心能看到每一条）")
                }
                None => format!("批量已提交：{total} 篇按顺序跑（任务中心能看到每一条）"),
            }
            .into(),
        );
        refresh_tasks(&ui, &st);
        refresh_batch_rows(&ui, &st);
        refresh_batch_summary(&ui, &st, "已提交，按顺序跑");
        if tx
            .send(Cmd::RunBatch {
                revision: st.project_revision.get(),
                model,
                voice_ref,
                voice_ref_text,
                gap_ms: gap_ms_from_ui(&ui),
                auto_normalize: ui.get_auto_normalize(),
                dict: st.active_dict.borrow().clone(),
                items,
            })
            .is_err()
        {
            st.batch_running.set(false);
            ui.set_batch_running(false);
            // 台账里那几条已经 enqueue 过了：必须逐条收成失败，否则它们会永远挂在
            // Pending，`tasks_in_flight` 一直为真，之后连单篇都提交不了
            let orphan_ids: Vec<u32> = {
                let mut rows = st.batch_rows.borrow_mut();
                rows.iter_mut()
                    .filter(|r| r.state == batch::ItemState::Waiting)
                    .filter_map(|r| {
                        r.state = batch::ItemState::Failed;
                        r.detail = "工作线程不可用，没有提交".into();
                        r.task_id
                    })
                    .collect()
            };
            for id in orphan_ids {
                st.tasks.borrow_mut().finish(
                    id,
                    tasks::TaskState::Failed,
                    "工作线程不可用，没有提交",
                );
            }
            refresh_tasks(&ui, &st);
            refresh_batch_rows(&ui, &st);
            refresh_batch_summary(&ui, &st, "工作线程不可用：一篇都没提交，请重启应用");
            ui.set_status_text("工作线程不可用：批量未提交，请重启应用".into());
        }
    });

    // ③ 停止整批：当前句合成完就停，后面的不再开
    let weak = ui.as_weak();
    let st = state.clone();
    let stop2 = Arc::clone(stop);
    ui.on_batch_stop(move || {
        let Some(ui) = weak.upgrade() else { return };
        if !batch_in_flight(&st) {
            return;
        }
        stop2.store(true, Ordering::Relaxed);
        ui.set_status_text("正在停止批量：当前句合成完就停，后面的篇目不再开".into());
        refresh_batch_summary(&ui, &st, "停止中：当前句合成完就停");
    });

    // ④ 清空列表（跑批中不允许：那会把进度写到已经不在的行上）
    let weak = ui.as_weak();
    let st = state.clone();
    ui.on_batch_clear(move || {
        let Some(ui) = weak.upgrade() else { return };
        if batch_in_flight(&st) {
            ui.set_status_text("批量正在跑：先停止再清空".into());
            return;
        }
        st.batch_rows.borrow_mut().clear();
        st.batch_skipped_notes.borrow_mut().clear();
        refresh_batch_rows(&ui, &st);
        refresh_batch_summary(&ui, &st, "");
    });
}

fn wire_task_center(
    ui: &MainWindow,
    state: &Rc<UiState>,
    stop: &Arc<AtomicBool>,
    sep_stop: &Arc<AtomicBool>,
    eval_stop: &Arc<AtomicBool>,
) {
    // 任务中心里点「停止」→ 走与各 Tab 按钮完全相同的停止函数（避免两套逻辑漂移）
    let weak_stop = ui.as_weak();
    let st_stop = state.clone();
    let stop_c = Arc::clone(stop);
    let sep_stop_c = Arc::clone(sep_stop);
    let eval_stop_c = Arc::clone(eval_stop);
    ui.on_task_stop(move |task_id| {
        let Some(ui) = weak_stop.upgrade() else {
            return;
        };
        if task_id < 0 {
            return;
        }
        stop_task_from_center(
            &ui,
            &st_stop,
            &stop_c,
            &sep_stop_c,
            &eval_stop_c,
            task_id as u32,
        );
    });

    let weak = ui.as_weak();
    ui.on_task_jump(move |tab, state| {
        let Some(ui) = weak.upgrade() else { return };
        let tab = tab.clamp(0, ui.get_scenes().row_count() as i32 - 1);
        ui.set_scene(tab);
        ui.set_task_center_open(false);
        let name = ui
            .get_scenes()
            .row_data(tab as usize)
            .map(|s| s.to_string())
            .unwrap_or_default();
        // 说对话：失败/已停止的任务不会"在后台继续跑"（复核抓到无条件文案与按钮不符）
        let note = if state == "失败" {
            format!("已切到「{name}」：在那里点开始/重录重试")
        } else if state == "运行中" || state.starts_with("排队中") {
            format!("已切到「{name}」：任务在后台继续跑")
        } else {
            format!("已切到「{name}」")
        };
        ui.set_status_text(note.into());
    });

    let weak = ui.as_weak();
    let st = state.clone();
    ui.on_task_clear_finished(move || {
        let Some(ui) = weak.upgrade() else { return };
        let n = st.tasks.borrow_mut().clear_finished();
        refresh_tasks(&ui, &st);
        if n > 0 {
            ui.set_status_text(format!("已从任务中心清除 {n} 条已结束任务").into());
        }
    });
}

/// 当前界面上的输入 → 一份工程（版本留档与对比都以"界面看到的"为准）。
///
/// 与 worker 造新工程共用 `new_project_from_inputs`，所以留档下来的东西就是"点开始合成
/// 会用的那一份"，不是另建一个近似对象。
fn project_from_ui(ui: &MainWindow, dict: &std::collections::BTreeMap<String, String>) -> Project {
    let (voice_ref, voice_ref_text) = voice_input_from_ui(ui);
    let mut project = new_project_from_inputs(
        &ui.get_script_text(),
        &current_model_name(ui).unwrap_or_default(),
        voice_ref.clone(),
        voice_ref_text,
        gap_ms_from_ui(ui),
        ui.get_auto_normalize(),
        dict,
    );
    // 留档也要忠实：把参考音的内容哈希一起记下来（算不出来就留 None = 下次保守重录）
    if let Some(path) = voice_ref.as_deref() {
        project.voice_ref_hash = sha256_file(Path::new(path)).ok();
    }
    project
}

/// 回滚（不碰界面）：读版本 → 用**当前磁盘工程**按文本继承音频与状态 → 落盘。
///
/// 关键契约：当前 `project.json` **存在但读不出来**（损坏/权限）时，直接中止并返回
/// Err——绝不 commit。`Project::load_if_present` 的契约就是"损坏工程不自动重建、不覆盖"，
/// 回滚如果把 `Err` 当成"没有当前工程"继续写盘，就会把唯一可人工恢复的现场抹掉。
fn rollback_with_inheritance(
    dir: &Path,
    id: &str,
    dict: &std::collections::BTreeMap<String, String>,
) -> Result<(Project, usize), String> {
    let inherited = match Project::load_if_present(dir) {
        Ok(p) => p,
        Err(e) => return Err(format!("当前工程读不出来，回滚已中止（不会覆盖现场）：{e}")),
    };
    let snapshot = versions::load_for_rollback(dir, id)?;
    let mut restored = project_from_version(&snapshot, dict);
    // 与 `load_resumable` 同一条判据：模型 / 兜底开关 / 参考音不一致时**不许复用音频**，
    // 否则回滚会把"另一个模型/音色合成的声音"标成这份版本的已合成。
    let reused = match inherited.as_ref() {
        Some(saved)
            if settings_allow_reuse(
                saved,
                &restored.model,
                &restored.voice_ref,
                &restored.voice_ref_hash,
                &restored.voice_ref_text,
                restored.auto_normalize,
                &effective_dict_hash(&restored),
            ) =>
        {
            reuse_done_sentences(&mut restored, saved, dir)?
        }
        _ => 0,
    };
    versions::commit_rollback(dir, &restored)?;
    Ok((restored, reused))
}

/// 版本快照 → 可继续合成的工程。
///
/// 快照里的句子状态是留档那一刻的（全 pending），这里**按稿件与设置重建**，
/// 句级状态交给调用方用"按文本继承"补——保持"状态由合成决定"这条不变式。
fn project_from_version(
    snapshot: &Project,
    dict: &std::collections::BTreeMap<String, String>,
) -> Project {
    let script: String = snapshot.sentences.iter().map(|s| s.text.as_str()).collect();
    // 词典用**当前启用的那套**：版本存的是稿件与设置，词典是库里的资产、不随版本走
    // （`new_project_from_inputs` 会把当前词典的指纹写进工程，复用判据因此仍然自洽）。
    let mut project = new_project_from_inputs(
        &script,
        &snapshot.model,
        snapshot.voice_ref.clone(),
        snapshot.voice_ref_text.clone(),
        snapshot.gap_ms,
        snapshot.auto_normalize,
        dict,
    );
    project.voice_ref_hash = snapshot.voice_ref_hash.clone();
    project
}

/// 时间戳 → 人话（版本列表用；不引日期库，按"多久以前"说）。
fn relative_time(now_ms: u64, then_ms: u64) -> String {
    if now_ms < then_ms {
        return "刚刚".to_string();
    }
    let secs = (now_ms - then_ms) / 1000;
    match secs {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", secs / 60),
        3600..=86_399 => format!("{} 小时前", secs / 3600),
        _ => format!("{} 天前", secs / 86_400),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 版本列表 → 界面（含坏文件计数：坏文件不能被当成"没有版本"）。
fn refresh_versions(ui: &MainWindow, state: &Rc<UiState>) {
    let Some(dir) = state.project_dir.borrow().clone() else {
        ui.set_version_rows(ModelRc::from(Rc::new(VecModel::from(Vec::new()))));
        ui.set_version_status("还没有工程：先开始一次合成再留档".into());
        return;
    };
    let (rows, broken) = versions::list(&dir);
    let now = now_ms();
    let ui_rows: Vec<VersionRow> = rows
        .iter()
        .map(|r| VersionRow {
            id: r.id.clone().into(),
            label: r.label.clone().into(),
            created: relative_time(now, r.created_at).into(),
            sentences: r.sentences as i32,
        })
        .collect();
    ui.set_version_rows(ModelRc::from(Rc::new(VecModel::from(ui_rows))));
    let mut status = if rows.is_empty() {
        "还没有留档".to_string()
    } else {
        format!("{} 份留档", rows.len())
    };
    if broken > 0 {
        status.push_str(&format!("（{broken} 个文件读不出来，已跳过）"));
    }
    ui.set_version_status(status.into());
}

/// 差异 → 面板里的多行文本。只列"变了的"：几十句稿子把没变的也铺出来没法读。
fn format_diff(label: &str, d: &versions::ProjectDiff, lines: &[versions::LineOp]) -> String {
    let mut out = String::new();
    out.push_str(&format!("与「{label}」对比：{}\n", d.summary()));
    if !d.settings.is_empty() {
        out.push_str("\n设置：\n");
        for c in &d.settings {
            out.push_str(&format!("  {}：{} → {}\n", c.field, c.before, c.after));
        }
    }
    let changed: Vec<&versions::LineOp> = lines
        .iter()
        .filter(|l| !matches!(l, versions::LineOp::Keep(_)))
        .collect();
    if changed.is_empty() {
        out.push_str("\n句子：没有变化\n");
    } else {
        out.push_str("\n句子（只列变化）：\n");
        for l in &changed {
            match l {
                versions::LineOp::Add(t) => out.push_str(&format!("  + {t}\n")),
                versions::LineOp::Remove(t) => out.push_str(&format!("- {t}\n")),
                versions::LineOp::Keep(_) => {}
            }
        }
        out.push_str(&format!(
            "\n（其余 {} 句未变）\n",
            d.lines.len() - changed.len()
        ));
    }
    out
}

/// 工程版本（P3）：留档 / 对比 / 回滚。
fn wire_versions(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
) {
    // 留档
    let weak = ui.as_weak();
    let st = state.clone();
    ui.on_version_save(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &st) || batch_in_flight(&st) {
            ui.set_status_text("任务进行中：版本等这轮跑完再留档".into());
            return;
        }
        let Some(dir) = st.project_dir.borrow().clone() else {
            ui.set_status_text("还没有工程：先开始一次合成再留档".into());
            return;
        };
        let project = project_from_ui(&ui, &st.active_dict.borrow());
        match versions::save(&dir, &ui.get_version_label_text(), &project, now_ms()) {
            Ok(_) => {
                ui.set_version_label_text("".into());
                refresh_versions(&ui, &st);
                ui.set_status_text("已留档（不快照音频；文本相同的句子下次合成仍会复用）".into());
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    // 对比（与该版本比"当前界面上的工程"）
    let weak = ui.as_weak();
    let st_diff = state.clone();
    ui.on_version_diff(move |id| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(dir) = st_diff.project_dir.borrow().clone() else {
            ui.set_status_text("还没有工程".into());
            return;
        };
        let id = id.to_string();
        match versions::load(&dir, &id) {
            Ok(v) => {
                let current = project_from_ui(&ui, &st_diff.active_dict.borrow());
                let d = versions::diff(&v.project, &current);
                let text = format_diff(&v.label, &d, &d.lines);
                ui.set_version_diff_text(text.into());
                ui.set_version_open(true);
                ui.set_status_text(format!("版本「{}」{}", v.label, d.summary()).into());
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    // 回滚：写回 project.json（音频不动，交给下次合成按文本继承）
    let weak = ui.as_weak();
    let st = state.clone();
    let tx = cmd_tx.clone();
    let rows = rows.clone();
    ui.on_version_rollback(move |id| {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &st) || batch_in_flight(&st) {
            ui.set_status_text("任务进行中：版本等这轮跑完再回滚".into());
            return;
        }
        let Some(dir) = st.project_dir.borrow().clone() else {
            ui.set_status_text("还没有工程".into());
            return;
        };
        let id = id.to_string();
        // 两步回滚（复核抓到的关键点）：版本快照里的句子全是 pending，直接写回再跑时
        // `load_resumable` 走快路径会原样返回它 —— "文本相同的句子复用"就落空；
        // 而当前工程损坏时必须中止（不能覆盖现场）。两件事都在下面这个函数里定义清楚。
        let dict = st.active_dict.borrow().clone();
        match rollback_with_inheritance(&dir, &id, &dict) {
            Ok((project, reused)) => {
                // 界面回灌：稿件 + 引擎/音色/停顿/兜底，全部走与手工编辑同一条路径
                let script: String = project.sentences.iter().map(|s| s.text.as_str()).collect();
                ui.set_script_text(script.clone().into());
                // 句子行重算（与改稿同一条路径）：这里只有 ui，行模型由 wire_versions 传入
                rebuild(&ui, &rows, &script);
                set_voice_ref(&ui, &project.voice_ref.clone().unwrap_or_default());
                ui.set_voice_ref_text(project.voice_ref_text.clone().unwrap_or_default().into());
                if !restore_voice_index(&ui, &project.model) {
                    ui.set_voice_index(-1);
                }
                ui.set_gap_ms_text(project.gap_ms.to_string().into());
                ui.set_auto_normalize(project.auto_normalize);
                st.auto_normalize_seen.set(project.auto_normalize);
                refresh_voice_labels(&ui);
                // 稿件/设置都换了 → 与改稿同一套作废（成品、BGM、质检分数）
                invalidate_worker_project(&tx, &st);
                reset_bgm(&ui, &st);
                st.assembled.borrow_mut().take();
                clear_eval_scores(&ui, &rows, &st);
                ui.set_has_result(false);
                refresh_versions(&ui, &st);
                ui.set_version_diff_text("".into());
                let todo = project
                    .sentences
                    .iter()
                    .filter(|s| s.status != "done")
                    .count();
                ui.set_status_text(
                    format!(
                        "已回滚到该版本：复用 {reused} 句已合成的音频，还要合成 {todo} 句；点「开始合成」继续"
                    )
                    .into(),
                );
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });
}

/// 模板（P2）：应用 / 存为 / 删除。
///
/// 应用是这里唯一有"后果"的动作：按 `templates::apply_effect` 判定要不要作废工程
/// （重录 > 重新导出 > 只影响试听），并把同一句话写到状态行——界面文案与判定同源。
fn wire_templates(ui: &MainWindow, cmd_tx: &Sender<Cmd>, state: &Rc<UiState>) {
    // 应用
    let weak = ui.as_weak();
    let st = state.clone();
    let tx = cmd_tx.clone();
    ui.on_template_apply(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 批量在飞时也算"任务进行中"：应用模板会改输入，而批量用的是提交那一刻的参数
        if project_editing_blocked(&ui, &st) || batch_in_flight(&st) {
            ui.set_status_text("任务进行中：模板等这轮跑完再应用".into());
            return;
        }
        let Ok(set) = read_templates() else {
            ui.set_status_text("模板文件坏了：先修好或删掉 templates.json，再加新模板".into());
            return;
        };
        let Some(name) = selected_template_name(&ui) else {
            ui.set_status_text("先选一个模板".into());
            return;
        };
        let Some(t) = set.get(&name).cloned() else {
            ui.set_status_text(format!("找不到模板「{name}」：刷新一下再试").into());
            return;
        };
        let effect = templates::apply_effect(&project_inputs_from_ui(&ui), &t);

        // 先套用输入（模型索引按名字找回，找不到就把索引清 -1，别静默换成别的引擎）
        set_voice_ref(&ui, &t.voice_ref.clone().unwrap_or_default());
        ui.set_voice_ref_text(t.voice_ref_text.clone().unwrap_or_default().into());
        if !restore_voice_index(&ui, &t.model) {
            ui.set_voice_index(-1);
        }
        ui.set_speed(t.speed);
        ui.set_speed_label(format!("{:.2}x", t.speed).into());
        // 手写的模板可能填了超上限的 gap：应用时归一，界面与执行保持一致
        ui.set_gap_ms_text(normalize_gap_ms(&t.gap_ms.to_string()).to_string().into());
        ui.set_auto_normalize(t.auto_normalize);
        st.auto_normalize_seen.set(t.auto_normalize);
        refresh_voice_labels(&ui);

        match effect {
            templates::ApplyEffect::Resynthesize => {
                invalidate_worker_project(&tx, &st);
                reset_bgm(&ui, &st);
                st.assembled.borrow_mut().take();
                ui.set_has_result(false);
            }
            templates::ApplyEffect::ReassembleOnly => {
                // 停顿只影响拼装：不清 has_result（导出按钮要能点），导出时会带上新停顿
                st.bgm_artifacts.borrow_mut().take();
                ui.set_bgm_has_result(false);
            }
            templates::ApplyEffect::AuditionOnly => {}
        }
        ui.set_template_index(
            ui.get_template_names()
                .iter()
                .position(|n| n == name)
                .map(|i| i as i32)
                .unwrap_or(-1),
        );
        ui.set_status_text(format!("已应用模板「{name}」：{}", effect.note()).into());
    });

    // 存为（同名覆盖）
    let weak = ui.as_weak();
    let st_save = state.clone();
    ui.on_template_save(move || {
        let Some(ui) = weak.upgrade() else { return };
        if project_editing_blocked(&ui, &st_save) || batch_in_flight(&st_save) {
            ui.set_status_text("任务进行中：模板等这轮跑完再存".into());
            return;
        }
        let model = current_model_name(&ui).unwrap_or_else(|| {
            ui.set_status_text("先选一个引擎，再存模板".into());
            String::new()
        });
        if model.is_empty() {
            return;
        }
        let t = match template_from_inputs(
            &ui.get_template_name_text(),
            &model,
            non_empty(ui.get_voice_ref_path().to_string()),
            non_empty(ui.get_voice_ref_text().to_string()),
            ui.get_speed(),
            gap_ms_from_ui(&ui),
            ui.get_auto_normalize(),
        ) {
            Ok(t) => t,
            Err(note) => {
                ui.set_status_text(note.into());
                return;
            }
        };
        let path = templates_path();
        let mut set = match read_templates() {
            Ok(set) => set,
            Err(e) => {
                // 坏文件绝不覆盖：先让用户处理，不然他已有的模板就没了
                ui.set_status_text(format!("{e}；模板没有保存").into());
                return;
            }
        };
        set.upsert(t.clone());
        if let Err(e) = templates::save(&path, &set) {
            ui.set_status_text(e.into());
            return;
        }
        refresh_template_names(&ui, Some(&t.name));
        ui.set_template_name_text("".into());
        ui.set_status_text(format!("已保存模板「{}」（同名会覆盖）", t.name).into());
    });

    // 删除
    let weak = ui.as_weak();
    ui.on_template_delete(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(name) = selected_template_name(&ui) else {
            ui.set_status_text("先选一个模板".into());
            return;
        };
        let mut set = match read_templates() {
            Ok(set) => set,
            Err(e) => {
                ui.set_status_text(format!("{e}；模板没有删除").into());
                return;
            }
        };
        if !set.remove(&name) {
            ui.set_status_text(format!("找不到模板「{name}」").into());
            return;
        }
        if let Err(e) = templates::save(&templates_path(), &set) {
            ui.set_status_text(e.into());
            return;
        }
        refresh_template_names(&ui, None);
        ui.set_status_text(format!("已删除模板「{name}」").into());
    });
}

/// 模板下拉当前选中的名字（索引非法返回 None）。
fn selected_template_name(ui: &MainWindow) -> Option<String> {
    let idx = ui.get_template_index();
    if idx < 0 {
        return None;
    }
    ui.get_template_names()
        .row_data(idx as usize)
        .map(|n| n.to_string())
}

fn wire_engine_changes(ui: &MainWindow, cmd_tx: &Sender<Cmd>, state: &Rc<UiState>) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state1 = state.clone();
    ui.on_model_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：音色暂不可改".into());
            return;
        }
        invalidate_worker_project(&tx, &state1);
        reset_bgm(&ui, &state1);
        ui.set_has_result(false);
        refresh_voice_labels(&ui);
        ui.set_status_text("音色已变更：请重新开始合成，旧工程音频暂不可导出".into());
    });

    let weak = ui.as_weak();
    let tx2 = cmd_tx.clone();
    let state2 = state.clone();
    ui.on_voice_ref_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        apply_voice_ref_change(&ui, &tx2, &state2);
    });
}

/// 换参考音频后的统一作废语义：手改路径（`on_voice_ref_changed`）与参考音频行的
/// 「选择…」文件框两条路共用这一份正文——同一语义两份实现必然漂移。
///
/// 运行中/忙时与手工编辑一样拒绝（「任务进行中：参考音暂不可改」）；
/// 放行则：清参考文本 + 作废工程/BGM/成品 + 刷新音色标签 + 状态行。
fn apply_voice_ref_change(ui: &MainWindow, tx: &Sender<Cmd>, state: &Rc<UiState>) {
    if ui.get_running() || ui.get_busy() {
        ui.set_status_text("任务进行中：参考音暂不可改".into());
        return;
    }
    // 路径字段是 in-out 绑定，回调触发时 Slint 已经把它改成新值了 ——
    // 拿不到旧值做比较，所以**任何编辑都让文本作废**：
    // 文本属于原来那段音频，留着就会静默拿它去条件新音频（见 `keeps_reference_text`）。
    clear_reference_text(ui);
    invalidate_worker_project(tx, state);
    reset_bgm(ui, state);
    ui.set_has_result(false);
    refresh_voice_labels(ui);
    ui.set_status_text("参考音已变更：请重新开始合成，旧工程音频暂不可导出".into());
}

/// 配音页内「音色」区：试听当前音色、清除参考音（切回内置音色）。
///
/// 概念区分（用户纠偏）：**换音色 ≠ 换模型**。
/// 音色只有两种来源——模型内置默认音色 / 参考音频克隆出来的音色；
/// 模型（audio8-tts / index-tts2 / 0.1b / stream…）是**引擎参数**，走 `model-changed`。
/// 单一事实来源：`voice-ref-path` 为空 = 内置默认音色，非空 = 克隆音色（不设第二个 mode 状态）。
fn wire_voice_panel(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    msg_tx: &Sender<WorkerMsg>,
    state: &Rc<UiState>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    ui.on_preview_voice(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 试听的语义是"马上听到"：排到长任务后面就是没试听，所以明确拒绝 + 说清原因
        // （此前这里是静默 return，用户点了没反应；分离在跑时还会被排到它后面）。
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&st) {
            ui.set_status_text(
                "有任务正在进行：试听要等它结束（试听是立刻返回的短操作，不排队）".into(),
            );
            return;
        }
        let idx = ui.get_voice_index();
        let Some(v) = (idx >= 0)
            .then(|| ui.get_voices().row_data(idx as usize))
            .flatten()
        else {
            ui.set_status_text("没有可用引擎：先在本机 audio.cpp 服务里配置 tts 模型".into());
            return;
        };
        let model = v.name.to_string();
        let (voice_ref, voice_ref_text) = voice_input_from_ui(&ui);
        // 与合成同一条按引擎守卫：audio8-tts 克隆缺文本在**发请求前**拦（试听也是
        // 一次真实请求，不拦就会拿服务端那句看不懂的 500 当试听结果）。
        if reference_text_missing(v.requires_reference_text, &voice_ref, &voice_ref_text) {
            ui.set_status_text(reference_text_required_note(&model).into());
            return;
        }
        // 克隆态试听同样是一次真实克隆请求：唯一入口 prepare 同一条——超 15s
        // 自动取前 15 秒，裁剪失败才红字拦截（不发起）。
        let mut trim_note: Option<String> = None;
        let voice_ref = match voice_ref {
            Some(path) => match prepare_reference_for_clone(&path, voice_ref_text.as_deref()) {
                Ok(prepared) => {
                    trim_note = prepared.note;
                    Some(prepared.path.to_string_lossy().into_owned())
                }
                Err(note) => {
                    ui.set_status_text(note.into());
                    return;
                }
            },
            None => None,
        };
        let what = if voice_ref.is_some() {
            "克隆音色"
        } else {
            "内置默认音色"
        };
        // 试听本身也是一条 worker 命令：置 busy 让"提交即运行中"的互斥任务（配音/BGM）
        // 在试听期间也被挡住，否则它们会排在试听后面却显示成已在跑。
        ui.set_busy(true);
        ui.set_status_text(
            match trim_note {
                // 信息提示留在状态行上（随后的"正在合成试听"会盖掉它），拼在前面。
                Some(trimmed) => format!("{trimmed} · 正在合成试听（{what} · {model}）…"),
                None => format!("正在合成试听（{what} · {model}）…"),
            }
            .into(),
        );
        if tx
            .send(Cmd::PreviewVoice {
                revision: st.project_revision.get(),
                model,
                voice_ref,
                voice_ref_text,
                text: VOICE_PREVIEW_TEXT.to_string(),
            })
            .is_err()
        {
            // 发不出去就必须把 busy 放掉，否则试听按钮/编辑守卫会永远卡住
            ui.set_busy(false);
            ui.set_status_text("工作线程不可用：试听未发出，请重启应用".into());
        }
    });

    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    ui.on_clear_voice_reference(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() || ui.get_voice_ref_path().is_empty() {
            return;
        }
        // 清空路径 ⇒ helper 判定"不是同一段音频" ⇒ 文本与状态行一起清
        set_voice_ref(&ui, "");
        invalidate_worker_project(&tx, &st);
        reset_bgm(&ui, &st);
        ui.set_has_result(false);
        refresh_voice_labels(&ui);
        ui.set_status_text("已切回内置默认音色：请重新开始合成".into());
    });

    // ── 参考音频「选择…」：文件框单选。结果与手改路径**同一条**作废语义 ──
    let weak = ui.as_weak();
    let msg_pick = msg_tx.clone();
    ui.on_reference_pick(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：参考音暂不可改".into());
            return;
        }
        ui.set_status_text(
            format!(
                "正在打开文件选择框（选 {} 干净人声）…",
                aw_core::reference_range_label()
            )
            .into(),
        );
        let msg_pick = msg_pick.clone();
        std::thread::spawn(move || {
            let pick = picker::pick_file(
                &format!(
                    "选择参考音频（{} 干净人声）",
                    aw_core::reference_range_label()
                ),
                "音频",
                &["*.wav", "*.mp3", "*.flac", "*.m4a", "*.ogg"],
            );
            let _ = msg_pick.send(WorkerMsg {
                revision: 0,
                msg: Msg::ReferenceAudioPicked { pick },
            });
        });
    });

    // ── 参考音频的文本：与参考音一样是音色的一部分（服务端拿它做条件）──
    //    手改 ⇒ 与"换参考音"同一条作废路径；清掉状态行是因为用户已经自己拍板了。
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    ui.on_voice_ref_text_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_voice_ref_text_status("".into());
        refresh_voice_labels(&ui);
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：参考音频的文本暂不可改".into());
            return;
        }
        invalidate_worker_project(&tx, &st);
        reset_bgm(&ui, &st);
        ui.set_has_result(false);
        ui.set_status_text("参考音频的文本已改：请重新开始合成".into());
    });

    // ── 自动转写：**只帮用户填，不替用户决定**。结果回填到输入框 + 状态行要求核对。──
    let weak = ui.as_weak();
    let msg = msg_tx.clone();
    let st = state.clone();
    ui.on_transcribe_reference(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_voice_ref_text_busy() {
            return;
        }
        // 转写期间不能有合成在跑：文本一变就要作废旧工程，跑到一半改输入会让
        // "界面上是什么"和"这次合成用的是哪份文本"对不上。
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&st) {
            ui.set_voice_ref_text_status(
                "有任务正在进行：自动转写要等它结束（转写会作废旧工程音频）".into(),
            );
            return;
        }
        let Some(path) = non_empty(ui.get_voice_ref_path().to_string()) else {
            ui.set_voice_ref_text_status("先填参考音频路径，再点「自动转写」".into());
            return;
        };
        if !Path::new(&path).is_file() {
            ui.set_voice_ref_text_status("参考音频不存在或不可读：先修正路径".into());
            return;
        }
        let model = aw_core::DEFAULT_ASR_MODEL.to_string();
        ui.set_voice_ref_text_busy(true);
        ui.set_voice_ref_text_status(format!("正在用 {model} 转写参考音频…").into());
        let msg = msg.clone();
        std::thread::spawn(move || {
            let result = match make_client() {
                Ok(client) => client.asr(Path::new(&path)).map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            let _ = msg.send(WorkerMsg {
                revision: 0,
                msg: Msg::ReferenceTranscribed { model, result },
            });
        });
    });

    // ── 文本描述生成音色：生成 / 用作配音音色 ──
    //    生成是短操作，不排队：有任务在飞时明确拒绝（与"试听"同一条守则）。
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    ui.on_design_generate(move || {
        let Some(ui) = weak.upgrade() else { return };
        let model = effective_design_model(&settings_snapshot());
        let text = ui.get_design_text().to_string();
        let description = ui.get_design_description().to_string();
        let any_busy = ui.get_running() || ui.get_busy() || tasks_in_flight(&st);
        // 点击回调与按钮 enabled 用**同一份判据**（改动见 design_generate_refusal 注释）
        if let Some(refusal) = design_generate_refusal(
            model.as_deref(),
            &text,
            &description,
            ui.get_design_busy(),
            any_busy,
        ) {
            ui.set_design_status(refusal.into());
            return;
        }
        let Some(model) = model else {
            ui.set_design_status("本机没有可用的音色设计模型".into());
            return;
        };
        ui.set_design_busy(true);
        ui.set_design_status(format!("正在生成音色（{model}）…").into());
        if tx
            .send(Cmd::DesignVoice {
                revision: 0,
                model,
                text,
                description,
            })
            .is_err()
        {
            // 发不出去就必须把 busy 放掉，否则按钮永远卡在"生成中"
            ui.set_design_busy(false);
            ui.set_design_status("工作线程不可用：生成未发出，请重启应用".into());
        }
    });

    let weak = ui.as_weak();
    let tx_use = cmd_tx.clone();
    let st = state.clone();
    ui.on_design_use_in_dubbing(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some((path, text)) = st.design_result.borrow().clone() else {
            ui.set_status_text("还没有可用的生成结果：先描述音色并生成".into());
            return;
        };
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&st) {
            ui.set_status_text("有任务正在进行：音色暂不可改，等它结束再使用".into());
            return;
        }
        set_voice_ref(&ui, &path.to_string_lossy());
        ui.set_voice_ref_text(text.into());
        ui.set_voice_ref_text_status("".into());
        // 与手改参考音同一条作废路径：旧工程音频不能再导出
        invalidate_worker_project(&tx_use, &st);
        reset_bgm(&ui, &st);
        ui.set_has_result(false);
        refresh_voice_labels(&ui);
        ui.set_scene(0);
        ui.set_dub_voice(true);
        ui.set_status_text("已用作配音音色：去配音页开始合成（克隆路径会用它做条件）".into());
    });
}

fn wire_sentence_actions(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    cmd_tx: &Sender<Cmd>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
) {
    let weak = ui.as_weak();
    let model1 = rows.clone();
    let state1 = state.clone();
    ui.on_select_sentence(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        if i == QA_SORT_COMMAND {
            toggle_qa_sort(&ui, &model1, &state1);
            return;
        }
        if i == QA_JUMP_WORST_COMMAND {
            jump_to_worst(&ui, &model1, &state1);
            return;
        }
        if i < 0 {
            return;
        }
        ui.set_selected(i);
        if let Some(row) = model1.row_data(i as usize) {
            ui.set_status_text(
                format!(
                    "已选中第 {} 句 · 起始 {} · 时长 {}",
                    row.no, row.start_label, row.duration_label
                )
                .into(),
            );
        }
    });

    let weak = ui.as_weak();
    let model2 = rows.clone();
    let player2 = player.clone();
    let state2 = state.clone();
    ui.on_preview_one(move |i| {
        if i < 0 {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        let Some(row) = model2.row_data(i as usize) else {
            return;
        };
        ui.set_selected(i);
        // i 是当前可见行位置；play_sentence 内部用 row.index 找工程 wav。
        play_sentence(&ui, &model2, &player2, &state2, i as usize, row.duration);
    });

    let weak = ui.as_weak();
    let model3 = rows.clone();
    let tx3 = cmd_tx.clone();
    let state3 = state.clone();
    ui.on_redo_one(move |i| {
        if i < 0 {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        let display_index = i as usize;
        let Some(row) = model3.row_data(display_index) else {
            return;
        };
        let project_index = match usize::try_from(row.index) {
            Ok(index) => index,
            Err(_) => return,
        };
        // 重录是"刚听完这句就要重录"的短操作：排在 8 分钟歌曲后面等于让用户执行一个
        // 他已经不想要的旧动作，所以**不入队**，而是明确拒绝。判据统一取台账 + busy
        // （重录自己也会置 busy，挡住随后的配音/BGM/导出）。
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&state3) {
            ui.set_status_text(
                "有任务正在进行：重录要等它结束（重录是立刻执行的短操作，不排队）".into(),
            );
            return;
        }
        if !state3.project_ready.get() {
            ui.set_status_text("工程已变更：先开始合成，再重录单句".into());
            return;
        }
        // 重录会让该句分数失效；先回到工程原序，之后所有消息/试听都用 index 映射。
        state3.qa_sorted.set(false);
        apply_row_order(&model3, restore_project_order(rows_as_vec(&model3)));
        let display_index = row_position(&model3, project_index).unwrap_or(0);
        ui.set_selected(display_index as i32);
        ui.set_busy(true);
        start_task(
            &ui,
            &state3,
            &state3.redo_task,
            tasks::TaskKind::Dub,
            format!("重录第 {} 句", row.no),
        );
        if tx3
            .send(Cmd::Redo {
                revision: state3.project_revision.get(),
                index: project_index,
            })
            .is_err()
        {
            ui.set_busy(false);
            let note = "工作线程不可用：重录未发出，请重启应用";
            // 收尾台账：命令没发出去，任务不能永远停在"运行中"
            finish_task(
                &ui,
                &state3,
                &state3.redo_task,
                tasks::TaskState::Failed,
                note,
            );
            ui.set_status_text(note.into());
            return;
        }
        ui.set_status_text(format!("单句重录中：第 {} 句（换 seed 重跑）", row.no).into());
    });

    // 时间轴点击 = 从那句话开始听（M1 不做拖动定位，逐句跳转即定位）
}

#[allow(clippy::too_many_arguments)]
fn wire_run(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    cmd_tx: &Sender<Cmd>,
    player: &Rc<player::Player>,
    stop: &Arc<AtomicBool>,
    state: &Rc<UiState>,
) {
    let weak = ui.as_weak();
    let model4 = rows.clone();
    let tx = cmd_tx.clone();
    let stop1 = Arc::clone(stop);
    let player1 = player.clone();
    let state1 = state.clone();
    ui.on_start_run(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 配音会改写 worker 持有的工程：台账里还有任何未跑完的任务（含排队的分离/
        // 歌曲）都不开始，避免两个 Run 同时改同一份工程
        if ui.get_busy() || ui.get_sep_busy() || tasks_in_flight(&state1) {
            ui.set_status_text("有任务正在进行：等当前任务结束再开始配音".into());
            return;
        }
        let n = model4.row_count();
        if n == 0 {
            ui.set_status_text("稿件为空：先粘稿子或点「载入示例稿」".into());
            return;
        }
        // 重新合成会让逐句音频/分数进入重算流程；分数排序视角立即回原序。
        state1.qa_sorted.set(false);
        apply_row_order(&model4, restore_project_order(rows_as_vec(&model4)));
        ui.set_selected(-1);
        let model_name = selected_model(&ui);
        if model_name.is_empty() {
            ui.set_status_text("没有可用音色：检查 server.json / audiocpp_server".into());
            return;
        }
        // 引擎能力与模型名取同一行（避免"文案看 A、能力看 B"）
        let requires_text = selected_voice(&ui)
            .as_ref()
            .map(|v| v.requires_reference_text)
            .unwrap_or(false);
        // 续作语义：已合成句保持原样（worker 跳过 done 句），其余重跑。
        // 若界面上已经有失败句，这一次明确走「只重跑失败句」，不把 pending
        // 或 done 混进去。
        let failed_count = (0..n)
            .filter(|&i| {
                model4
                    .row_data(i)
                    .map(|r| r.status.starts_with("失败"))
                    .unwrap_or(false)
            })
            .count();
        let retry_failed = failed_count > 0;
        let resume = ui.get_done_count() > 0;
        if !resume {
            for i in 0..n {
                set_status(&model4, i, "待合成");
            }
        }
        let (voice_ref, voice_ref_text) = voice_input_from_ui(&ui);
        // 唯一入口 prepare：超 15s 自动取前 15 秒（原文件不动，副本进 voice-trimmed），
        // 裁剪失败才红字拦截——**发请求前**收敛到 ≤15s 的路径（不拦会把引擎进程打死，
        // 见 prepare_reference_for_clone 的注释），不排任务、不发任何请求。
        let mut trim_note: Option<String> = None;
        let voice_ref = match voice_ref {
            Some(path) => {
                if !Path::new(&path).is_file() {
                    ui.set_status_text(
                        format!("参考音频不存在或不可读：{path}（修正后再开始合成）").into(),
                    );
                    return;
                }
                match prepare_reference_for_clone(&path, voice_ref_text.as_deref()) {
                    Ok(prepared) => {
                        trim_note = prepared.note;
                        Some(prepared.path.to_string_lossy().into_owned())
                    }
                    Err(note) => {
                        ui.set_status_text(note.into());
                        return;
                    }
                }
            }
            None => None,
        };
        // 克隆音色缺参考文本：**发起前**拦住。发出去的话每一句都会撞同一个 500
        // （服务端原文用户看不懂，而且 N 句 = N 条一模一样的失败）。
        // 按**当前引擎**判断（audio8-tts 要、index-tts2 不要），文案与界面提示
        // 同一份 `reference_text_required_note`。
        if reference_text_missing(requires_text, &voice_ref, &voice_ref_text) {
            ui.set_status_text(reference_text_required_note(&model_name).into());
            return;
        }
        reset_bgm(&ui, &state1);
        stop1.store(false, Ordering::Relaxed);
        let task_id = start_task(
            &ui,
            &state1,
            &state1.dub_task,
            tasks::TaskKind::Dub,
            format!("配音 · {n} 句"),
        );
        ui.set_running(true);
        ui.set_has_result(false);
        ui.set_progress(ui.get_done_count() as f32 / n.max(1) as f32);
        ui.set_playing(false);
        player1.stop();
        state1.assembled.borrow_mut().take();
        state1.project_ready.set(false);
        let stem = file_stem(&ui.get_project_name());
        *state1.project_dir.borrow_mut() = Some(project_dir(&stem));
        refresh_versions(&ui, &state1);
        if tx
            .send(Cmd::Run {
                revision: state1.project_revision.get(),
                task_id,
                script: ui.get_script_text().to_string(),
                model: model_name.clone(),
                voice_ref,
                voice_ref_text,
                project_name: stem,
                gap_ms: gap_ms_from_ui(&ui),
                auto_normalize: ui.get_auto_normalize(),
                dict: state1.active_dict.borrow().clone(),
                retry_failed,
            })
            .is_err()
        {
            ui.set_running(false);
            state1.project_ready.set(false);
            let note = "工作线程不可用：合成未发出，请重启应用";
            finish_task(
                &ui,
                &state1,
                &state1.dub_task,
                tasks::TaskState::Failed,
                note,
            );
            ui.set_status_text(note.into());
            return;
        }
        let mut note = if retry_failed {
            format!(
                "继续合成 · {model_name} · 只重跑 {failed_count} 句失败句（不重跑已完成的句子）"
            )
        } else if resume {
            format!(
                "继续合成 · {model_name} · 已完成的 {} 句自动跳过",
                ui.get_done_count()
            )
        } else {
            format!("合成中 · {model_name}")
        };
        // 边合成边校听（streaming-preview §3 变更点 1）：运行中已完成句随时可点行内「试听」
        note.push_str("（已完成的句子可随时点「试听」）");
        // 超长参考音被自动裁剪的信息提示要留在状态行上（随后那句"合成中"会盖掉它）——
        // 拼在前面，状态栏 elide 截断时丢的是尾巴。
        if let Some(trimmed) = trim_note {
            note = format!("{trimmed} · {note}");
        }
        ui.set_status_text(note.into());
    });

    let weak = ui.as_weak();
    let stop2 = Arc::clone(stop);
    ui.on_stop_run(move || {
        let Some(ui) = weak.upgrade() else { return };
        stop_dub_run(&ui, &stop2);
    });

    // 手动释放模型内存：只在用户点 OOM 句子上的按钮时调用。任何任务在飞时
    // 都拒绝，避免把别的任务正在用的模型卸掉；结果由 worker 回包如实展示。
    let weak = ui.as_weak();
    let tx_unload = cmd_tx.clone();
    let st_unload = state.clone();
    ui.on_unload_models(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&st_unload) {
            ui.set_status_text("有任务正在进行：等任务结束后再释放模型内存".into());
            return;
        }
        ui.set_busy(true);
        ui.set_status_text("正在请求服务端卸载已加载模型…".into());
        if tx_unload.send(Cmd::UnloadModels).is_err() {
            ui.set_busy(false);
            ui.set_status_text("工作线程不可用：释放模型内存未发出，请重启应用".into());
        }
    });

    let weak = ui.as_weak();
    let player3 = player.clone();
    ui.on_stop_preview(move || {
        let Some(ui) = weak.upgrade() else { return };
        player3.stop();
        ui.set_playing(false);
        ui.set_status_text("已停止试听".into());
    });

    let weak = ui.as_weak();
    let model5 = rows.clone();
    let player5 = player.clone();
    let state5 = state.clone();
    ui.on_preview_all(move || {
        let Some(ui) = weak.upgrade() else { return };
        play_all(&ui, &model5, &player5, &state5);
    });

    let weak = ui.as_weak();
    let player6 = player.clone();
    ui.on_speed_changed(move |v| {
        let Some(ui) = weak.upgrade() else { return };
        let label = format!("{v:.2}x");
        ui.set_speed_label(label.clone().into());
        player6.set_speed(v);
        ui.set_status_text(format!("试听倍速 {label}（回放即时生效，不动合成参数）").into());
    });
}

fn wire_export(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    msg_tx: &Sender<WorkerMsg>,
    state: &Rc<UiState>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state = state.clone();
    // 批量导出的闭包也要一份（下面这个 clone 必须在 state 被移进 on_export_requested 之前）
    let state_batch = state.clone();
    let state_stems = state.clone();
    ui.on_export_requested(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 批量导出正在跑时拒绝：两边都会往导出目录写 `<工程名>.wav`，当前工程也在
        // 那批里的话就是同一个目标文件（复核指出并发写同一目标的风险）
        if state.batch_export_running.get() {
            ui.set_status_text("批量导出还在进行：等它结束再导出当前工程".into());
            return;
        }
        // 拼装只有亚秒级，同样不该排队：有任务在飞时直接拒绝并说清
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&state) {
            ui.set_status_text(
                "有任务正在进行：导出要等它结束（拼装是立刻执行的短操作，不排队）".into(),
            );
            return;
        }
        if !state.project_ready.get() {
            ui.set_status_text("工程已变更：先开始合成，再导出".into());
            return;
        }
        ui.set_busy(true);
        if tx
            .send(Cmd::Assemble {
                revision: state.project_revision.get(),
                gap_ms: gap_ms_from_ui(&ui),
            })
            .is_err()
        {
            ui.set_busy(false);
            ui.set_status_text("工作线程不可用：导出未发出，请重启应用".into());
            return;
        }
        ui.set_status_text("拼装成品中（完成后按导出开关复制）…".into());
    });

    // 批量导出：projects/ 下所有有成品（out/final.wav）的工程 → 导出目录
    let weak = ui.as_weak();
    let st = state_batch;
    let msg = msg_tx.clone();
    ui.on_batch_export(move || {
        let Some(ui) = weak.upgrade() else { return };
        if st.batch_export_running.get() {
            ui.set_status_text("批量导出还在进行…".into());
            return;
        }
        if let Some(refusal) = export_refusal(ui.get_busy(), batch_in_flight(&st)) {
            ui.set_status_text(refusal.into());
            return;
        }
        let wav_on = ui.get_export_wav_on();
        let srt_on = ui.get_export_srt_on();
        if !wav_on && !srt_on {
            // 与单篇同一条口径：没勾格式就说清楚，别假装导了
            ui.set_status_text("未选择导出格式：先选「整段 WAV」或「逐句 SRT」".into());
            return;
        }
        st.batch_export_running.set(true);
        ui.set_status_text("批量导出中：正在把有成品工程的 WAV/SRT 复制到导出目录…".into());
        spawn_batch_export(
            msg.clone(),
            projects_root(),
            PathBuf::from(ui.get_export_dir().to_string()),
            wav_on,
            srt_on,
        );
    });

    // 分轨导出（P6 的另一半）：人声 + BGM 两轨 → 导出目录，文件名沿用 _voice / _bgm
    let weak = ui.as_weak();
    let st = state_stems;
    ui.on_export_stems(move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Some(refusal) = export_refusal(ui.get_busy(), batch_in_flight(&st)) {
            ui.set_status_text(refusal.into());
            return;
        }
        let name = file_stem(&ui.get_project_name());
        let dir = PathBuf::from(ui.get_export_dir().to_string());
        // 判据：UI 认为这套 BGM 结果仍是当前结果（改描述会置 stale、从磁盘恢复的也是 stale）。
        // 描述改没改只有 UI 知道，磁盘那层只看"配没配上这份配音成品"。
        let outcome = export::export_stems(&name, &project_dir(&name), &dir, &bgm_context(&ui));
        let text = match &outcome {
            export::StemExportOutcome::Done(s) => {
                if let Some(first) = s.written.first() {
                    toast(&ui, &format!("已导出 {}", file_label(first)));
                }
                export::stem_summary_text(s, &dir)
            }
            export::StemExportOutcome::NothingToExport => {
                "分轨导出：这个工程还没有成品（先合成 / 混音）".to_string()
            }
            export::StemExportOutcome::Stale(reasons) => format!(
                "分轨导出：磁盘上的混音过期了（{}）；在 BGM 页重新生成并混音后再导",
                reasons.join("；")
            ),
            export::StemExportOutcome::Failed(e) => format!("分轨导出失败：{e}"),
        };
        ui.set_status_text(text.into());
    });
}

fn wire_bgm(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
    player: &Rc<player::Player>,
    stop: &Arc<AtomicBool>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state1 = state.clone();
    let stop1 = Arc::clone(stop);
    ui.on_bgm_generate(move || {
        let Some(ui) = weak.upgrade() else { return };
        // BGM 会读"配音是否已有成品"来决定混音还是独立生成，且写同一个工程目录：
        // 同样要求台账里没有别的任务在飞
        if ui.get_running() || ui.get_busy() || tasks_in_flight(&state1) {
            ui.set_status_text("任务进行中：等当前任务结束再生成 BGM".into());
            return;
        }
        // 注意：这里**不要求** project_ready —— 没有配音成品时可以独立生成 BGM
        // （worker 会用命令带来的工程目录当落点，且只出 BGM 一轨）。
        let prompt = ui.get_bgm_prompt().to_string();
        if prompt.trim().is_empty() {
            ui.set_status_text("先写一段 BGM 描述".into());
            return;
        }
        // 档位（duck / 独立时长）没有回调，生成时一起把 BGM 输入落盘
        save_bgm_settings(&ui);
        reset_bgm(&ui, &state1);
        ui.set_busy(true);
        // 清停止位：否则上一轮遗留的 stop 会让新任务在第一次段间检查时立刻停掉
        stop1.store(false, Ordering::Relaxed);
        // 新的一轮开始：旧产物不再是"当前结果"（has_result 由 reset_bgm 清掉），stale 也一并复位
        ui.set_bgm_stale(false);
        let task_id = start_task(
            &ui,
            &state1,
            &state1.bgm_task,
            tasks::TaskKind::Bgm,
            "BGM · 生成并混音",
        );
        ui.set_bgm_status_text("正在生成 BGM 分段…".into());
        if tx
            .send(Cmd::RunBgm {
                revision: state1.project_revision.get(),
                task_id,
                prompt,
                duck_gain: duck_gain_for(ui.get_bgm_duck_index()),
                standalone_seconds: Some(bgm_standalone_seconds(ui.get_bgm_standalone_index())),
                dir: project_dir(&file_stem(&ui.get_project_name())),
            })
            .is_err()
        {
            ui.set_busy(false);
            let note = "工作线程不可用：BGM 未发出，请重启应用";
            finish_task(
                &ui,
                &state1,
                &state1.bgm_task,
                tasks::TaskState::Failed,
                note,
            );
            ui.set_bgm_status_text(note.into());
            ui.set_status_text(note.into());
        }
    });

    // 停止：BGM 与配音走同一台 worker，共用同一个停止位（顺序队列）
    let weak = ui.as_weak();
    let stop_bgm = Arc::clone(stop);
    ui.on_bgm_stop(move || {
        let Some(ui) = weak.upgrade() else { return };
        stop_bgm_run(&ui, &stop_bgm);
    });

    // 改 prompt：旧产物标为"旧版本"（仍可试听；导出以当前结果为准）
    let weak = ui.as_weak();
    ui.on_bgm_prompt_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_busy() || ui.get_running() {
            return;
        }
        if ui.get_bgm_has_result() && !ui.get_bgm_stale() {
            ui.set_bgm_stale(true);
            ui.set_bgm_status_text(
                "prompt 已改：当前显示的是旧版本，重新生成后才是当前结果".into(),
            );
        }
        // 描述是用户写的内容，不能重启就丢：每次真变了就写回 settings.json
        save_bgm_settings(&ui);
    });

    let weak = ui.as_weak();
    let state2 = state.clone();
    let player2 = player.clone();
    ui.on_bgm_preview_track(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(artifacts) = state2.bgm_artifacts.borrow().clone() else {
            ui.set_status_text("还没有 BGM 成品：先生成并混音".into());
            return;
        };
        let Some(path) = bgm_track_path(&artifacts, i) else {
            ui.set_status_text("这一轨不存在：本次是独立生成的 BGM（只有 BGM 轨）".into());
            return;
        };
        let what = match i {
            0 => "人声",
            1 => "BGM",
            _ => "混音",
        };
        match player2.play_wav(&path) {
            Ok(()) => {
                state2.playing_total.set(artifacts.duration as f32);
                ui.set_playing(true);
                ui.set_status_text(format!("试听{what}轨：{}", file_label(&path)).into());
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    // 旧的两个入口（整包试听/整包导出）已随结果区改造下线：试听/导出都按轨走。

    // 单独导出某一轨（结果区每行一个导出）
    let weak = ui.as_weak();
    let st_export = state.clone();
    ui.on_bgm_export_track(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        // 注意：这里**不**再看 `bgm_artifacts` 在不在内存里——分轨导出的来源是磁盘
        // （重开应用打开旧工程时内存里没有 artifacts，但 bgm/bgm.wav 一直在）。
        if let Some(refusal) = export_refusal(ui.get_busy(), batch_in_flight(&st_export)) {
            ui.set_status_text(refusal.into());
            return;
        }
        // 与「分轨导出」共用同一份实现：**来源与命名只有一处**（<工程名>_voice/_bgm/_mixed.wav）
        let dir = PathBuf::from(ui.get_export_dir().to_string());
        let name = file_stem(&ui.get_project_name());
        let project = project_dir(&name);
        match export::export_stem(&name, &project, &dir, stem_for_track(i), &bgm_context(&ui)) {
            export::StemExportOutcome::Done(s) => {
                let Some(path) = s.written.first() else {
                    ui.set_status_text("这一轨没有导出".into());
                    return;
                };
                ui.set_status_text(format!("已导出：{}", path.display()).into());
                toast(&ui, &format!("已导出 {}", file_label(path)));
            }
            export::StemExportOutcome::NothingToExport => {
                ui.set_status_text("这一轨不存在：本次是独立生成的 BGM（只有 BGM 轨）".into());
            }
            export::StemExportOutcome::Stale(reasons) => ui.set_status_text(
                format!(
                    "这一轨是旧混音，与当前配音成品对不上（{}）；先重新生成并混音",
                    reasons.join("；")
                )
                .into(),
            ),
            export::StemExportOutcome::Failed(e) => ui.set_status_text(e.into()),
        }
    });
}

fn export_song(ui: &MainWindow, state: &Rc<UiState>) {
    let Some((source, _)) = state.song_artifact.borrow().clone() else {
        ui.set_status_text("还没有歌曲成品：先生成歌曲".into());
        return;
    };
    let dir = PathBuf::from(ui.get_export_dir().to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        ui.set_status_text(aw_core::dub::write_failure_note(&dir, 0, &e).into());
        return;
    }
    let dst = dir.join(format!("{}_song.wav", stem_of(ui)));
    if let Err(e) = aw_core::dub::copy_atomic(&source, &dst) {
        ui.set_status_text(aw_core::dub::write_failure_note(&dst, 0, &e).into());
        return;
    }
    toast(ui, &format!("已导出 {}", dst.display()));
    ui.set_status_text(format!("歌曲已导出到 {}", dst.display()).into());
}

// ===========================================================================
// 音乐制作引擎：清单里的 gen 模型 → 可选引擎
//
// 改前是两处硬编码：UI 的 `options: ["yue2（歌词成歌）", "ACE-Step（文生音乐）"]`
// 与提交时的 `if index == 1 { "ace-step" } else { "yue2" }`，worker 再 `_ => Yue2`
// 兜底 —— 清单里换个 gen 模型时，用户选的根本不是跑的那个，而且全程无报错。
// 现在选项、下标 → id、id → 实现都从同一份清单派生（见
// `LESSON_同一语义两处实现必然漂移`：决定走哪个分支的那个判断也该一起抽）。
// ===========================================================================

/// App 内置支持的音乐制作引擎（清单缺失时的兜底；也是 id ↔ 展示名的唯一表）。
const BUILTIN_SONG_ENGINES: [(&str, &str); 2] = [
    ("yue2", "yue2（歌词成歌）"),
    ("ace-step", "ACE-Step（文生音乐）"),
];

/// 一个可选的歌曲引擎：稳定 id + 展示名。
#[derive(Debug, Clone, PartialEq)]
struct SongEngineOption {
    id: String,
    label: String,
}

/// 内置引擎的展示名；不在内置表里的 id 用 id 本身当展示名（照样可选，但过不了
/// `song_model_for_id` —— 会显式报"还没接入"，不静默换引擎）。
fn builtin_song_label(id: &str) -> Option<&'static str> {
    BUILTIN_SONG_ENGINES
        .iter()
        .find(|(bid, _)| *bid == id)
        .map(|(_, label)| *label)
}

/// 音乐制作可选引擎的**唯一清单**：清单里 `task == "gen"`、没被产品层排除、
/// 不是流式专用的模型。界面选项与"下标 → id"都从它派生。
///
/// 清单里一个 gen 模型都没有（服务没配 / 读不到 server.json）时退回
/// `BUILTIN_SONG_ENGINES`：这是显式的兜底清单，不是静默的引擎替换。
fn song_engine_options(models: &[ServerModel]) -> Vec<SongEngineOption> {
    let from_manifest: Vec<SongEngineOption> = models
        .iter()
        .filter(|m| m.task == "gen" && !m.caps.product_excluded && !m.caps.is_streaming_only())
        .map(|m| SongEngineOption {
            label: builtin_song_label(&m.id)
                .unwrap_or(m.id.as_str())
                .to_string(),
            id: m.id.clone(),
        })
        .collect();
    if from_manifest.is_empty() {
        BUILTIN_SONG_ENGINES
            .iter()
            .map(|(id, label)| SongEngineOption {
                id: (*id).to_string(),
                label: (*label).to_string(),
            })
            .collect()
    } else {
        from_manifest
    }
}

/// id → 引擎实现。**未知 id 是显式错误**，不再 `_ => Yue2` 静默兜底：
/// 清单里换个 gen 模型时，宁可报"这个引擎 App 还没接入"，也不要把请求发到错的
/// 引擎上 —— 用错引擎出的音频，用户从声音里分辨不出来。
fn song_model_for_id(id: &str) -> Result<SongModel, String> {
    match id {
        "yue2" => Ok(SongModel::Yue2),
        "ace-step" => Ok(SongModel::AceStep),
        other => Err(format!(
            "引擎 {other} 还没有接入音乐制作链路（App 支持：yue2 / ace-step）"
        )),
    }
}

/// 音乐制作 Tab 的模式（S2 翻唱入口）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SongMode {
    Text2Song,
    Cover,
}

impl SongMode {
    fn from_ui(mode_index: i32) -> Self {
        if mode_index == 1 {
            Self::Cover
        } else {
            Self::Text2Song
        }
    }
}

/// 翻唱源音频是否已就绪（路径非空）。
fn song_source_is_ready(source: Option<&str>) -> bool {
    source.is_some_and(|s| !s.trim().is_empty())
}

/// 生成/翻唱主按钮的可用性与原因（**唯一判据**，Slint 里不重拼）。
///
/// 与 `backup_refusal` / `update_refusal` 同一条约定：按钮 enabled 与点击回调
/// 都吃这一份，避免「按钮亮着却点不动」或「按钮灰着但判据说不忙」。
/// `song_queued` 时按钮是「取消排队」——可点，所以不产生 refusal。
fn song_generate_refusal(
    mode: SongMode,
    lyrics: &str,
    style: &str,
    cover_source: Option<&str>,
    song_busy: bool,
    song_queued: bool,
) -> Option<String> {
    if song_queued {
        return None;
    }
    if song_busy {
        return Some("歌曲生成中：等它完成（整段生成，没有中间进度）".into());
    }
    if lyrics.trim().is_empty() {
        return Some("先填歌词".into());
    }
    if style.trim().is_empty() {
        return Some("先填歌曲风格".into());
    }
    if mode == SongMode::Cover && !song_source_is_ready(cover_source) {
        return Some("翻唱需要先选择源音频（源音频 → sheetsage2 转谱 → yue2 唱新词）".into());
    }
    None
}

fn song_generate_blocked(
    mode: SongMode,
    lyrics: &str,
    style: &str,
    cover_source: Option<&str>,
    song_busy: bool,
    song_queued: bool,
) -> bool {
    song_generate_refusal(mode, lyrics, style, cover_source, song_busy, song_queued).is_some()
}

/// 把主按钮可用性/原因投影到 UI（每 tick 同步一次；值没变时 Slint 不会重绘）。
///
/// 可用性与原因都从 `song_generate_refusal` 一份判据派生：`song_generate_blocked`
/// 只是它的 bool 形态，二者不会漂移。
fn refresh_song_action(ui: &MainWindow, state: &Rc<UiState>) {
    let mode = SongMode::from_ui(ui.get_song_mode_index());
    let lyrics = ui.get_song_lyrics();
    let style = ui.get_song_style();
    let source = state.song_source.borrow();
    let busy = ui.get_song_busy();
    let queued = ui.get_song_queued();
    let blocked = song_generate_blocked(mode, &lyrics, &style, source.as_deref(), busy, queued);
    let reason = song_generate_refusal(mode, &lyrics, &style, source.as_deref(), busy, queued)
        .unwrap_or_default();
    ui.set_song_generate_blocked(blocked);
    ui.set_song_generate_reason(reason.into());
}

/// 音色设计「生成」的提交拦截判据。**单点**：按钮 enabled 与点击回调都用它，
/// 不在 Slint 里重拼条件。
fn design_generate_refusal(
    design_model: Option<&str>,
    text: &str,
    description: &str,
    design_busy: bool,
    any_busy: bool,
) -> Option<String> {
    if design_busy {
        return Some("音色生成中：等它完成".into());
    }
    if any_busy {
        return Some("有任务正在进行：生成音色要等它结束（生成是短操作，不排队）".into());
    }
    if design_model.is_none() {
        return Some("本机没有可用的 VoiceDesign 模型：需在服务清单里登记 task=vdes 的设计模型（BreezeTTS 2 / Qwen3-TTS 等）".into());
    }
    if text.trim().is_empty() {
        return Some("先填试听文本".into());
    }
    if description.trim().is_empty() {
        return Some("先填音色描述".into());
    }
    None
}

/// 音色设计生成按钮的 bool 形态：与 [`design_generate_refusal`] 同一份判据。
fn design_generate_blocked(
    design_model: Option<&str>,
    text: &str,
    description: &str,
    design_busy: bool,
    any_busy: bool,
) -> bool {
    design_generate_refusal(design_model, text, description, design_busy, any_busy).is_some()
}

/// 把音色设计按钮的可用性/原因投影到 UI（每 tick 同步）。
fn refresh_design_action(ui: &MainWindow, state: &Rc<UiState>) {
    let model = effective_design_model(&settings_snapshot());
    let text = ui.get_design_text();
    let description = ui.get_design_description();
    let design_busy = ui.get_design_busy();
    ui.set_design_model_ready(model.is_some());
    let any_busy = ui.get_running() || ui.get_busy() || tasks_in_flight(state);
    let blocked =
        design_generate_blocked(model.as_deref(), &text, &description, design_busy, any_busy);
    let reason =
        design_generate_refusal(model.as_deref(), &text, &description, design_busy, any_busy)
            .unwrap_or_default();
    ui.set_design_blocked(blocked);
    ui.set_design_reason(reason.into());
    // 试听文本/描述一变，旧结果虽还能试听，但「用作配音音色」的语义仍是那份旧产物；
    // ready 只由「有没有结果」决定，这里不改它。
}

/// 音色设计合成的结果 → Msg。**失败文案必须经 `ClientError::to_string()`**：那里是
/// OOM 可执行文案的唯一入口（`memory_shortfall_note` 给出「释放模型内存」下一步）。
/// 谁在这里改成裸 `format!("{code}")`，OOM 提示就会从音色设计这条路上消失。
fn design_voice_msg(model: String, text: String, outcome: Result<Vec<u8>, ClientError>) -> Msg {
    match outcome {
        Ok(wav) => Msg::DesignVoiceDone { wav, text, model },
        Err(e) => Msg::DesignVoiceFailed {
            error: e.to_string(),
            model,
        },
    }
}

/// 写入翻唱源音频（唯一写入点）：路径是真相，UI 的 source-path / source-summary /
/// source-picked 都是它的投影。源音频变了就作废旧歌曲产物（试听/导出别指向旧输入）。
fn set_song_source(ui: &MainWindow, state: &Rc<UiState>, path: String) {
    let same = state.song_source.borrow().as_deref() == Some(path.as_str());
    ui.set_song_cover_source_path(path.clone().into());
    ui.set_song_cover_source_summary(format!("源音频：{}", file_label(Path::new(&path))).into());
    *state.song_source.borrow_mut() = Some(path);
    ui.set_song_cover_source_picked(true);
    if !same {
        ui.set_song_has_result(false);
        *state.song_artifact.borrow_mut() = None;
        ui.set_song_status_text("源音频已就绪，可生成翻唱（yue2，约 8 分钟/首）".into());
    }
    refresh_song_action(ui, state);
}

/// 把清单里的歌曲引擎投影到界面（选项文案 + 收窄当前下标）。
fn refresh_song_engine_options(ui: &MainWindow) {
    let models = read_server_config().map(|c| c.models).unwrap_or_default();
    let options = song_engine_options(&models);
    if options.is_empty() {
        return;
    }
    let labels: Vec<SharedString> = options.iter().map(|o| o.label.clone().into()).collect();
    ui.set_song_model_options(ModelRc::from(Rc::new(VecModel::from(labels))));
    let keep = ui.get_song_model_index().max(0) as usize;
    ui.set_song_model_index(keep.min(options.len() - 1) as i32);
}

fn wire_song(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    msg_tx: &Sender<WorkerMsg>,
    state: &Rc<UiState>,
    player: &Rc<player::Player>,
) {
    // 模式切换：文生歌 ↔ 翻唱。切换即作废旧产物（旧产物不对应新模式的输入）。
    let weak = ui.as_weak();
    let state_mode = state.clone();
    ui.on_song_mode_selected(move |cover_index| {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_song_mode_index(if cover_index == 1 { 1 } else { 0 });
        ui.set_song_has_result(false);
        *state_mode.song_artifact.borrow_mut() = None;
        if cover_index == 1 {
            ui.set_song_status_text(
                "翻唱：选源音频 → sheetsage2 转谱 → yue2 唱新词（约 8 分钟/首）".into(),
            );
        } else {
            ui.set_song_status_text(
                "写歌 / 文生音乐（yue2 约 8 分钟/首；ACE-Step 120s 约 6.4 分钟）".into(),
            );
        }
        refresh_song_action(&ui, &state_mode);
    });

    // 选择翻唱源音频（系统文件框，后台线程 + 消息回传；三态语义见 picker）
    let weak = ui.as_weak();
    let msg = msg_tx.clone();
    ui.on_song_cover_pick_file(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_song_busy() {
            return;
        }
        ui.set_song_status_text("正在打开系统文件选择框…".into());
        spawn_song_source_pick(msg.clone());
    });

    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state1 = state.clone();
    ui.on_song_generate(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 歌曲不依赖配音工程的可变状态（只复用工程目录写 song/），所以运行中也能
        // 提交：排进同一条队列。本 Tab 只有一个结果槽位，同一条歌曲还没结束时不接第二条。
        if state1.song_task.get().is_some() {
            ui.set_song_status_text("已有一首歌在队列里：等它结束，或去任务中心看进度".into());
            return;
        }
        let mode_index = ui.get_song_mode_index();
        let cover_mode = mode_index == 1;
        let mode = SongMode::from_ui(mode_index);
        let lyrics = ui.get_song_lyrics().to_string();
        let style = ui.get_song_style().to_string();
        let cover_source = state1.song_source.borrow().clone();
        // 按钮的 enabled 与这里必须用**同一份判据**（改动见 song_generate_refusal 的注释）
        if let Some(refusal) = song_generate_refusal(
            mode,
            &lyrics,
            &style,
            cover_source.as_deref(),
            ui.get_song_busy(),
            ui.get_song_queued(),
        ) {
            ui.set_song_generate_reason(refusal.clone().into());
            ui.set_song_status_text(refusal.into());
            return;
        }
        ui.set_song_busy(true);
        let task_label = if cover_mode {
            "音乐制作 · 翻唱"
        } else {
            "音乐制作 · 生成歌曲"
        };
        let task_id = enqueue_task(
            &ui,
            &state1,
            &state1.song_task,
            tasks::TaskKind::Song,
            task_label,
        );
        ui.set_song_has_result(false);
        let song_note = queue_note(&state1, task_id);
        // 真的排在别人后面才给"取消排队"：空闲提交时 worker 立刻接手，没有可取消的窗口
        ui.set_song_queued(song_note.is_some());
        let running_note = if cover_mode {
            "翻唱生成中（yue2，约 8 分钟/首；无中间进度）…".to_string()
        } else {
            "歌曲生成中（yue2 可能约 8 分钟，ACE-Step 120s 约 6.4 分钟）…".to_string()
        };
        ui.set_song_status_text(
            match song_note {
                Some(note) => format!("{note} · 轮到它时自动开始"),
                None => running_note,
            }
            .into(),
        );
        if cover_mode {
            let Some(source_audio) = cover_source.filter(|p| !p.trim().is_empty()) else {
                ui.set_song_busy(false);
                ui.set_song_queued(false);
                let note = "翻唱需要先选择源音频";
                finish_task(
                    &ui,
                    &state1,
                    &state1.song_task,
                    tasks::TaskState::Failed,
                    note,
                );
                ui.set_song_status_text(note.into());
                ui.set_status_text(note.into());
                return;
            };
            if tx
                .send(Cmd::RunCover {
                    revision: state1.project_revision.get(),
                    task_id,
                    project_name: file_stem(&ui.get_project_name()),
                    lyrics,
                    style,
                    source_audio: PathBuf::from(source_audio),
                })
                .is_err()
            {
                ui.set_song_busy(false);
                ui.set_song_queued(false);
                let note = "工作线程不可用：翻唱未发出，请重启应用";
                finish_task(
                    &ui,
                    &state1,
                    &state1.song_task,
                    tasks::TaskState::Failed,
                    note,
                );
                ui.set_song_status_text(note.into());
                ui.set_status_text(note.into());
            }
            return;
        }

        let song_engines =
            song_engine_options(&read_server_config().map(|c| c.models).unwrap_or_default());
        let model = usize::try_from(ui.get_song_model_index().max(0))
            .ok()
            .and_then(|i| song_engines.get(i))
            .map(|o| o.id.clone());
        let Some(model) = model else {
            ui.set_song_busy(false);
            ui.set_song_queued(false);
            let note = "没有可用的音乐制作引擎：先在服务清单里配一个 gen 模型";
            finish_task(
                &ui,
                &state1,
                &state1.song_task,
                tasks::TaskState::Failed,
                note,
            );
            ui.set_song_status_text(note.into());
            ui.set_status_text(note.into());
            return;
        };
        if tx
            .send(Cmd::RunSong {
                revision: state1.project_revision.get(),
                task_id,
                project_name: file_stem(&ui.get_project_name()),
                model,
                lyrics,
                style,
            })
            .is_err()
        {
            ui.set_song_busy(false);
            ui.set_song_queued(false);
            let note = "工作线程不可用：歌曲未发出，请重启应用";
            finish_task(
                &ui,
                &state1,
                &state1.song_task,
                tasks::TaskState::Failed,
                note,
            );
            ui.set_song_status_text(note.into());
            ui.set_status_text(note.into());
        }
    });

    // 取消排队：还没轮到跑的歌曲可以硬取消（立刻出队，worker 取到取消标记就不执行）。
    // 已经在跑的那条不给假停止——歌曲请求一旦发出就是一次阻塞调用，服务端没有取消接口，
    // 界面这时显示的是"生成中…"而不是"停止"。
    let weak = ui.as_weak();
    let state_stop = state.clone();
    ui.on_song_stop(move || {
        let Some(ui) = weak.upgrade() else { return };
        stop_song(&ui, &state_stop);
    });

    let weak = ui.as_weak();
    let state2 = state.clone();
    let player2 = player.clone();
    ui.on_song_preview(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some((path, duration)) = state2.song_artifact.borrow().clone() else {
            ui.set_status_text("还没有歌曲成品：先生成歌曲".into());
            return;
        };
        match player2.play_wav(&path) {
            Ok(()) => {
                state2.playing_total.set(duration as f32);
                ui.set_playing(true);
                ui.set_status_text("试听歌曲".into());
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    let weak = ui.as_weak();
    let state3 = state.clone();
    ui.on_song_export_track(move || {
        let Some(ui) = weak.upgrade() else { return };
        export_song(&ui, &state3);
    });
}

fn wire_keys(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
) {
    let weak = ui.as_weak();
    let model = rows.clone();
    ui.on_key_prev(move || {
        let Some(ui) = weak.upgrade() else { return };
        let cur = ui.get_selected();
        let next = if cur < 0 { 0 } else { (cur - 1).max(0) };
        if (next as usize) < model.row_count() {
            ui.set_selected(next);
            if let Some(row) = model.row_data(next as usize) {
                ui.set_status_text(format!("已选中第 {} 句", row.no).into());
            }
        }
    });
    let weak = ui.as_weak();
    let model = rows.clone();
    ui.on_key_next(move || {
        let Some(ui) = weak.upgrade() else { return };
        let cur = ui.get_selected();
        let next = if cur < 0 {
            0
        } else {
            (cur + 1).min(model.row_count() as i32 - 1)
        };
        if next >= 0 && (next as usize) < model.row_count() {
            ui.set_selected(next);
            if let Some(row) = model.row_data(next as usize) {
                ui.set_status_text(format!("已选中第 {} 句", row.no).into());
            }
        }
    });
    let weak = ui.as_weak();
    let model = rows.clone();
    let player = player.clone();
    let state = state.clone();
    ui.on_key_toggle(move || {
        let Some(ui) = weak.upgrade() else { return };
        if player.is_playing() {
            player.stop();
            ui.set_playing(false);
            ui.set_status_text("已停止试听".into());
            return;
        }
        let sel = ui.get_selected();
        if sel < 0 {
            ui.set_status_text("先选中一句（↑/↓ 或点击）".into());
            return;
        }
        if let Some(row) = model.row_data(sel as usize) {
            play_sentence(&ui, &model, &player, &state, sel as usize, row.duration);
        }
    });
}

// ===========================================================================
// 主循环节拍：消息泵 + 播放头
// ===========================================================================

/// 哪些消息**不该**按工程版本（`project_revision`）过滤。
///
/// 判据不是"哪条命令发的"，而是"这条消息自己有没有别的身份"：
///   · 带 `task_id` 的（任务中心那套）：队列会让它跨过若干次改稿才回来；
///   · 来自文件/目录对话框的结果：用户点的是"选一个文件"，与工程版本无关。
///
/// 反面教材见本批复核抓到的第一版：批量那五条消息不在名单里，于是用户只要先编辑
/// 一次稿件（revision+1），导入结果与整批进度就全被静默丢掉——界面停在"合成本"、
/// 台账停在运行中。抽成函数是为了让单测能钉住**每一条**批量消息都在名单里。
/// 这条 worker 消息现在该不该处理：不在名单里的必须 revision 对得上。
/// 生产（`tick`）与单测共用这一个判定，避免"测试过了但泵里还是老逻辑"。
fn should_handle_message(worker_msg: &WorkerMsg, current_revision: u64) -> bool {
    message_ignores_revision(&worker_msg.msg)
        || worker_message_is_current(worker_msg, current_revision)
}

/// 放掉「一键备份…」的在飞标志。
///
/// 它同时是按钮的禁用条件，**取消 / 选择器不可用 / 终态三条路都必须放**：
/// 忘了放就等于把按钮永久锁死。三处调用共用这一份，别再手写第二遍。
fn release_backup_running(ui: &MainWindow, state: &Rc<UiState>) {
    state.backup_running.set(false);
    ui.set_backup_running(false);
}

fn message_ignores_revision(msg: &Msg) -> bool {
    matches!(
        msg,
        Msg::TaskStarted { .. }
        | Msg::TaskStage { .. }
        | Msg::ServerHealth { .. }
        | Msg::EngineSelfHeal { .. }
        | Msg::ModelsUnloaded { .. }
        | Msg::ModelDirPicked { .. }
        // 备份：目录选择结果来自对话框，终态来自长 IO，两者都与工程版本无关；
        // 不在名单里就会出现"备份好了但状态行还停在正在备份"（期间改一次稿就永远停住）
        | Msg::BackupDirPicked { .. }
        | Msg::BackupDone { .. }
        | Msg::VoiceImportDirPicked { .. }
        // 音色库「添加音频…」：来自系统文件框（后台线程、revision 0），
        // 与工程版本无关——被过滤掉就是「点完没反应」
        | Msg::VoiceFilesPicked { .. }
        // 参考音频「选择…」同理：文件框结果，与工程版本无关
        | Msg::ReferenceAudioPicked { .. }
        // 参考音转写：来自后台线程，与稿件版本无关（转的是参考音，不是稿子）
        | Msg::ReferenceTranscribed { .. }
        // 文本描述生成音色：来自 worker，但只合成一句试听文本、不碰当前工程，
        // 与稿件版本无关；用 revision 0 发回，否则期间改稿会把终态丢掉、界面停在"生成中…"
        | Msg::DesignVoiceDone { .. }
        | Msg::DesignVoiceFailed { .. }
        | Msg::DictFilePicked { .. }
        | Msg::SeparationInputPicked { .. }
        | Msg::SongSourcePicked { .. }
        | Msg::SeparationProgress { .. }
        | Msg::SeparationDone { .. }
        | Msg::SeparationStopped { .. }
        | Msg::SeparationFailed { .. }
        // 歌曲带 task_id 自证身份：它可能排在别的任务后面，期间改稿不该
        // 让终态消息被 revision 过滤掉（否则任务永远停在"运行中"）
        | Msg::SongDone { .. }
        | Msg::SongStopped { .. }
        | Msg::SongFailed { .. }
        // 质检同理：它可能排在别的任务后面，期间改稿不该把终态丢掉
        | Msg::EvalProgress { .. }
        | Msg::EvalDone { .. }
        | Msg::EvalStopped { .. }
        | Msg::EvalFailed { .. }
        // 批量：导入结果来自文件对话框（与工程版本无关）；逐篇消息都带
        // task_id 自证身份，而且批量跑的时候用户**可以**继续改当前稿件
        // （批量写的是别的工程目录）——按 revision 过滤会让整批消息在
        // 第一次改稿后全部消失，界面停在"合成本"、台账停在运行中
        | Msg::BatchScriptsPicked { .. }
        | Msg::BatchItemStarted { .. }
        | Msg::BatchItemProgress { .. }
        | Msg::BatchItemDone { .. }
        | Msg::BatchDone { .. }
        // 批量导出同理：它来自后台线程，用户点按钮那一刻与工程版本无关；
        // 用 revision 0 发回来，按版本过滤就会「点完没反应」
        | Msg::BatchExportDone { .. }
        // 模型下载同理：后台队列线程发回，与工程版本无关（下载权重和稿子无关）
        | Msg::DownloadUpdate(_)
        // 检查更新：后台线程的结果，用户点按钮那一刻与工程版本无关；
        // 用 revision 0 发回来，按版本过滤就会「点完永远停在检查中…」
        | Msg::UpdateCheckDone { .. }
    )
}

fn tick(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    msg_rx: &Rc<RefCell<Receiver<WorkerMsg>>>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
    cmd_tx: &Sender<Cmd>,
    msg_tx: &Sender<WorkerMsg>,
) {
    // ── 工作线程消息 ──
    let mut run_finished: Option<(usize, bool, usize, Option<String>)> = None;
    loop {
        let worker_msg = msg_rx.borrow_mut().try_recv();
        let Ok(worker_msg) = worker_msg else { break };
        // 服务健康检查与工程版本无关：不过滤，否则刚改完设置的结果会被静默丢掉
        // 与工程版本无关的后台结果（服务健康、目录选择）不过滤：过滤会让"刚选好的目录"
        // 因为期间编过稿件（revision+1）被静默丢掉，状态永远停在"正在打开系统目录选择框…"。
        if !should_handle_message(&worker_msg, state.project_revision.get()) {
            continue;
        }
        match worker_msg.msg {
            Msg::TaskStage { task_id, stage } => {
                if state.tasks.borrow_mut().note(task_id, stage) {
                    refresh_tasks(ui, state);
                }
            }
            Msg::TaskStarted { task_id } => {
                // 只有台账里确实还排着的条目才会被提升（排队中点停止的不会被复活）。
                // 先把借用收掉再动界面：set_task_running_text / refresh_tasks 都要再借一次
                // 这张台账，临时借用跨块会让 RefCell 直接 panic。
                let promoted = state.tasks.borrow_mut().promote(task_id);
                if promoted {
                    set_task_running_text(ui, state, task_id);
                }
                refresh_tasks(ui, state);
            }
            Msg::ProjectLoaded { project, reused } => {
                // worker 载入的工程才是执行事实：把开关/停顿同步成它记着的值，
                // 免得界面上显示 A、实际按 B 合成
                ui.set_auto_normalize(project.auto_normalize);
                state.auto_normalize_seen.set(project.auto_normalize);
                ui.set_gap_ms_text(project.gap_ms.to_string().into());
                // 载入/续作后先回到工程顺序；分数可以保留，但“按分数看”的视图不跨轮次沿用。
                state.qa_sorted.set(false);
                apply_row_order(rows, restore_project_order(rows_as_vec(rows)));
                // 工程里的质检分数回灌（跨会话留存：重开应用不用重跑 ASR）。
                // 来源模型一起灌，否则"谁测的"会随每次重开丢失（旧工程仍是 None=未知）。
                replace_eval_ledger(state, &eval_ledger_from_project(&project));
                apply_project_to_rows(ui, rows, &project);
                apply_eval_labels(rows, &state.eval_scores.borrow());
                sync_qa_actions(ui, rows, state);
                state.project_ready.set(true);
                if reused > 0 {
                    ui.set_status_text(
                        format!("工程已恢复：按文本复用 {reused} 句，未变句无需重录").into(),
                    );
                }
            }
            Msg::Fatal(t) => {
                mark_running_rows_failed(rows);
                ui.set_running(false);
                ui.set_busy(false);
                finish_task(
                    ui,
                    state,
                    &state.dub_task,
                    tasks::TaskState::Failed,
                    t.clone(),
                );
                ui.set_status_text(t.into());
            }
            Msg::Sentence {
                index,
                status,
                duration,
            } => {
                // 与磁盘同一个触发点：aw-core 在**开始重做**时就把旧分作废并落盘，
                // 所以 UI 在 running（以及随后的 done/error）都清——两边不会说法不一。
                if sentence_message_invalidates_score(&status) {
                    // 分数与来源一起摘（唯一写入点）；只摘分数会留下"有来源、没分"的孤儿
                    let had = drop_eval_ledger(state, index);
                    // 这句刚被重做，旧质检结论不再能代表当前音频；排序视角也回原序。
                    state.qa_sorted.set(false);
                    apply_row_order(rows, restore_project_order(rows_as_vec(rows)));
                    if let Some(i) = row_position(rows, index) {
                        ui.set_selected(i as i32);
                    }
                    if had {
                        apply_eval_labels(rows, &state.eval_scores.borrow());
                    }
                    sync_qa_actions(ui, rows, state);
                }
                // 走 review-leftovers 抽出的可测入口（按工程 index 回填）；
                // error 判定用 starts_with：OOM 句的状态是 `error: oom: ...`，不是裸 "error"
                apply_sentence_msg(rows, index, &status, duration);
                if status == "done" || status.starts_with("error") {
                    let done = (0..rows.row_count())
                        .filter(|&i| {
                            rows.row_data(i)
                                .map(|r| r.status.as_str() == "已合成")
                                .unwrap_or(false)
                        })
                        .count();
                    ui.set_done_count(done as i32);
                    let p = done as f32 / rows.row_count().max(1) as f32;
                    ui.set_progress(p);
                    progress_task(
                        ui,
                        state,
                        &state.dub_task,
                        p,
                        format!("{done}/{} 句已完成", rows.row_count()),
                    );
                }
            }
            Msg::RunDone {
                failed,
                stopped,
                reused,
                error,
            } => {
                ui.set_busy(false);
                // 任务台账：停下来 = 已停止；有失败句 = 失败；否则完成。
                // 失败时把第一条完整错误带给任务中心，OOM 的三个动作不能只活在
                // 句子行里；状态栏随后还会明确告诉用户「继续合成」只重跑失败句。
                let (task_state, detail) = if stopped {
                    (tasks::TaskState::Stopped, "用户停止".to_string())
                } else if failed > 0 {
                    let detail = error
                        .as_deref()
                        .map(sentence_error_detail)
                        .map(|d| format!("{failed} 句失败：{d}"))
                        .unwrap_or_else(|| format!("{failed} 句失败"));
                    (tasks::TaskState::Failed, detail)
                } else {
                    (
                        tasks::TaskState::Done,
                        if reused > 0 {
                            format!("完成（复用 {reused} 句）")
                        } else {
                            "完成".to_string()
                        },
                    )
                };
                finish_task(ui, state, &state.dub_task, task_state, detail);
                run_finished = Some((failed, stopped, reused, error));
            }
            Msg::ModelsUnloaded { ok, note } => {
                ui.set_busy(false);
                let text = if ok {
                    format!("释放模型内存：{note}")
                } else {
                    format!("释放模型内存失败：{note}")
                };
                ui.set_status_text(text.into());
            }
            Msg::RedoDone { index, error } => {
                ui.set_busy(false);
                match error {
                    Some(error) => {
                        set_status_by_project_index(rows, index, "error");
                        finish_task(
                            ui,
                            state,
                            &state.redo_task,
                            tasks::TaskState::Failed,
                            error.clone(),
                        );
                        ui.set_status_text(error.into());
                    }
                    None => {
                        finish_task(
                            ui,
                            state,
                            &state.redo_task,
                            tasks::TaskState::Done,
                            format!("第 {} 句已重录", index + 1),
                        );
                        ui.set_status_text(format!("第 {} 句重录完成", index + 1).into());
                    }
                }
            }
            Msg::AssembleFailed(error) => {
                ui.set_busy(false);
                ui.set_status_text(error.into());
            }
            Msg::ModelDirPicked { pick } => match pick {
                picker::Outcome::Picked(p) => {
                    ui.set_model_dir(p.clone().into());
                    ui.set_status_text(
                        format!("已选中模型目录：{p}（点「应用并重连」生效）").into(),
                    );
                }
                picker::Outcome::Cancelled => {
                    ui.set_status_text("取消了选择模型目录".into());
                }
                picker::Outcome::Unavailable(trouble) => {
                    ui.set_status_text(format!("没能选择模型目录：{}", trouble.note()).into());
                }
            },
            Msg::BackupDirPicked { pick } => match pick {
                picker::Outcome::Picked(p) => ui.set_backup_info(
                    format!("正在备份到 {p} …（工程大时要一会儿，中途别关窗口）").into(),
                ),
                // 取消 / 选择器不可用：后台线程这两条路都不会再发 `BackupDone`，
                // 在飞标志必须在这里放掉，否则「一键备份…」被永久锁死。
                picker::Outcome::Cancelled => {
                    release_backup_running(ui, state);
                    ui.set_backup_info("已取消备份".into());
                }
                picker::Outcome::Unavailable(trouble) => {
                    release_backup_running(ui, state);
                    ui.set_backup_info(format!("备份没开始：{}", trouble.note()).into());
                }
            },
            Msg::BackupDone { result } => {
                release_backup_running(ui, state);
                match result {
                    Ok(note) => ui.set_backup_info(note.into()),
                    Err(error) => ui.set_backup_info(format!("备份失败：{error}").into()),
                }
            }
            Msg::UpdateCheckDone { result } => {
                // 终态一定要把在飞标志放掉：它同时是按钮的禁用条件与"检查中…"文案，
                // 忘了放就等于把「检查更新」永久锁死
                state.update_running.set(false);
                ui.set_update_running(false);
                // 「显示什么 + 留不留发布页」由 update::outcome_view 一处决定（有单测钉住
                // 三种结果；尤其 UpToDate/Err 必须交出 None，否则按钮亮着点开是旧版本）。
                // 这里只做赋值，不再自己写第二份判断。
                let (info, release) = update::outcome_view(update::CURRENT_VERSION, result);
                ui.set_update_info(info.into());
                // 发布页按钮的可用性 = 这个 Option 在不在（tick 里投影给 UI）
                *state.update_release.borrow_mut() = release;
            }
            Msg::ServerHealth { ok, detail } => {
                ui.set_server_status(detail.into());
                ui.set_server_ok(ok);
                // 服务刚被改地址 / 重启过时，后端可能从 metal 变 cuda，标签要跟着走
                refresh_backend_label(ui);
                // 地址判据也可能刚变（测试连接前用户改了 host/port）：自愈开关跟着刷新
                state.engine_explicit.set(server_base_for_engine().1);
            }
            Msg::EngineSelfHeal { note } => {
                // 只有结论与上次不同才写状态行：引擎稳定时不刷屏；引擎恢复后清空记录，
                // 下一次故障才能再次提示。
                let mut last_note = state.last_engine_heal_note.borrow_mut();
                if note != *last_note {
                    if let Some(n) = &note {
                        ui.set_status_text(n.clone().into());
                    }
                    *last_note = note;
                }
            }
            Msg::VoicePreview { wav, label } => {
                ui.set_busy(false);
                // 试听只写临时文件，不落工程目录：不参与导出、不污染断点续作
                let path = std::env::temp_dir().join("audio-workshop-voice-preview.wav");
                match std::fs::write(&path, &wav) {
                    Ok(()) => match player.play_wav(&path) {
                        Ok(()) => {
                            ui.set_playing(true);
                            ui.set_status_text(format!("试听音色：{label}").into());
                        }
                        Err(e) => ui.set_status_text(format!("试听失败：{e}").into()),
                    },
                    Err(e) => {
                        ui.set_status_text(
                            format!(
                                "试听临时文件写入失败：{}",
                                aw_core::dub::write_failure_note(&path, wav.len(), &e)
                            )
                            .into(),
                        );
                    }
                }
            }
            Msg::DesignVoiceDone { wav, text, model } => {
                ui.set_design_busy(false);
                match save_design_voice(&wav) {
                    Ok(path) => {
                        *state.design_result.borrow_mut() = Some((path.clone(), text.clone()));
                        ui.set_design_ready(true);
                        ui.set_design_status(
                            format!("已生成（{model}）：可试听或用作配音音色").into(),
                        );
                        // 生成即试听，用户不用再点一次"播放"才知道设计出来的是什么声音
                        match player.play_wav(&path) {
                            Ok(()) => {
                                ui.set_playing(true);
                                ui.set_status_text(format!("音色设计完成：{model}").into());
                            }
                            Err(e) => {
                                ui.set_status_text(format!("音色已生成，但试听失败：{e}").into())
                            }
                        }
                    }
                    Err(e) => {
                        ui.set_design_status(format!("音色已生成，但保存失败：{e}").into());
                        ui.set_status_text(format!("音色设计保存失败：{e}").into());
                    }
                }
            }
            Msg::DesignVoiceFailed { error, model } => {
                ui.set_design_busy(false);
                ui.set_design_status(format!("生成失败（{model}）：{error}").into());
                ui.set_status_text(format!("音色设计失败（{model}）：{error}").into());
            }
            Msg::ReferenceTranscribed { model, result } => {
                ui.set_voice_ref_text_busy(false);
                match result {
                    Ok(text) => {
                        // **填进输入框**而不是替用户拍板：用户能在同一处直接改错字。
                        // 状态行一直留着提醒，直到他改文本或开始合成（见 on_start_run）。
                        ui.set_voice_ref_text(text.clone().into());
                        ui.set_voice_ref_text_status(
                            format!(
                                "已用 {model} 转写并填入（{} 字）：**请核对**，转写错字会让克隆音色跑偏。",
                                text.trim().chars().count()
                            )
                            .into(),
                        );
                        // 文本变了 ⇒ 旧成品/旧工程不可复用（与手改参考文本同一条路）。
                        // 三件事与 `on_voice_ref_text_changed` **逐条对齐**：少了 BGM 与
                        // has_result，"试听全篇/导出"会亮着、点了才说"工程已变更"（复核指出）。
                        // 必须在这里作废 worker 那份内存工程：否则下一次「重新合成某句」
                        // 会拿旧文本去合成，用户改了文本却听不出变化。
                        invalidate_worker_project(cmd_tx, state);
                        reset_bgm(ui, state);
                        ui.set_has_result(false);
                        refresh_voice_labels(ui);
                        ui.set_status_text("参考音频已转写：核对文本后开始合成".into());
                    }
                    Err(e) => {
                        ui.set_voice_ref_text_status(
                            format!("自动转写失败（{model}）：{e} —— 可以手填这段音频实际念的内容")
                                .into(),
                        );
                        ui.set_status_text("自动转写失败：手动填写参考音频的文本即可继续".into());
                    }
                }
            }
            Msg::ReferenceAudioPicked { pick } => {
                let path = match pick {
                    picker::Outcome::Picked(p) => p,
                    picker::Outcome::Cancelled => {
                        ui.set_status_text("取消了选择参考音频".into());
                        return;
                    }
                    picker::Outcome::Unavailable(trouble) => {
                        ui.set_status_text(format!("没能选择参考音频：{}", trouble.note()).into());
                        return;
                    }
                };
                // 选完那一刻可能有任务跑起来了：与手工编辑同一条拒绝语义，
                // **先拒后改**——不能先把路径改掉再拒（会留下改了音色却不作废的残局）
                if ui.get_running() || ui.get_busy() {
                    ui.set_status_text("任务进行中：参考音暂不可改".into());
                    return;
                }
                // 与手改路径同一语义：写路径（换音频时连带清参考文本）+ 作废工程/BGM/成品
                set_voice_ref(ui, &path);
                apply_voice_ref_change(ui, cmd_tx, state);
            }
            Msg::VoicePreviewFailed { label, error } => {
                ui.set_busy(false);
                ui.set_status_text(format!("试听失败（{label}）：{error}").into());
            }
            Msg::BgmProgress { done, total } => {
                let progress = done as f32 / total.max(1) as f32;
                ui.set_bgm_progress(progress);
                let note = format!("BGM 生成中：{done}/{total} 段");
                progress_task(ui, state, &state.bgm_task, progress, note.clone());
                ui.set_bgm_status_text(note.clone().into());
                ui.set_status_text(note.into());
            }
            Msg::BgmDone {
                artifacts,
                segments,
                mixed,
            } => {
                ui.set_busy(false);
                apply_bgm_artifacts(ui, state, &artifacts);
                let note = if mixed {
                    format!(
                        "BGM 完成：{segments} 段 · 成品 {:.1}s · 已生成 voice/bgm/mixed",
                        artifacts.duration
                    )
                } else {
                    format!(
                        "BGM 完成（独立生成）：{segments} 段 · 成品 {:.1}s · 只生成 BGM 轨",
                        artifacts.duration
                    )
                };
                finish_task(
                    ui,
                    state,
                    &state.bgm_task,
                    tasks::TaskState::Done,
                    format!("{segments} 段 · 成品 {:.1}s", artifacts.duration),
                );
                ui.set_bgm_status_text(note.clone().into());
                ui.set_status_text(note.into());
                // 产物清单：记下"这套 BGM 是哪个参数、配哪份配音成品做出来的"。
                // 之后改稿/改描述/跨会话恢复都靠它判断还能不能导分轨
                // （见 src/export.rs::bgm_result_is_current）。
                let manifest_dir = artifacts
                    .mixed
                    .as_ref()
                    .or(Some(&artifacts.bgm))
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf());
                if let Some(dir) = manifest_dir {
                    if let Err(e) = export::write_result_manifest(&dir, &bgm_digest_now(ui, mixed))
                    {
                        ui.set_status_text(
                            format!("BGM 完成，但产物清单没写上（{e}）：下次导出会提示先重新生成")
                                .into(),
                        );
                    }
                }
                *state.bgm_artifacts.borrow_mut() = Some(artifacts);
            }
            Msg::BgmStopped { done } => {
                ui.set_busy(false);
                ui.set_bgm_has_result(false);
                ui.set_bgm_progress(0.0);
                let note = format!("已停止：完成了 {done} 段（未混音；下次同描述可复用已完成段）");
                ui.set_bgm_status_text(note.clone().into());
                ui.set_status_text(note.clone().into());
                finish_task(ui, state, &state.bgm_task, tasks::TaskState::Stopped, note);
            }
            Msg::BgmFailed(error) => {
                ui.set_busy(false);
                finish_task(
                    ui,
                    state,
                    &state.bgm_task,
                    tasks::TaskState::Failed,
                    error.clone(),
                );
                ui.set_bgm_has_result(false);
                ui.set_bgm_progress(0.0);
                ui.set_bgm_status_text(error.clone().into());
                ui.set_status_text(error.into());
            }
            Msg::SeparationInputPicked { pick } => match pick {
                picker::Outcome::Picked(p) => {
                    set_separation_input(ui, state, p);
                }
                picker::Outcome::Cancelled => {
                    ui.set_status_text("取消了选择音频".into());
                }
                picker::Outcome::Unavailable(trouble) => {
                    ui.set_status_text(format!("没能选择音频：{}", trouble.note()).into());
                }
            },
            Msg::SongSourcePicked { pick } => match pick {
                picker::Outcome::Picked(p) => {
                    set_song_source(ui, state, p);
                    ui.set_status_text("翻唱源音频已选择".into());
                }
                picker::Outcome::Cancelled => {
                    ui.set_song_status_text("取消了选择源音频".into());
                    ui.set_status_text("取消了选择源音频".into());
                }
                picker::Outcome::Unavailable(trouble) => {
                    let note = format!("没能选择源音频：{}", trouble.note());
                    ui.set_song_status_text(note.clone().into());
                    ui.set_status_text(note.into());
                }
            },
            Msg::SeparationProgress {
                task_id,
                percent,
                note,
            } => {
                if state.sep_task.get() == Some(task_id) {
                    ui.set_sep_progress(percent);
                    ui.set_sep_status_text(note.clone().into());
                    progress_task(ui, state, &state.sep_task, percent, note);
                }
            }
            Msg::SeparationDone {
                task_id,
                input,
                out_dir,
                vocals,
                accompaniment,
            } => {
                if state.sep_task.get() == Some(task_id) {
                    // 先落盘历史，再提交 UI 结果：写失败时如实报错，但分离本身的两轨
                    // 已经成功，仍可试听/导出；不把“历史写失败”伪装成“历史已写入”。
                    let history_note = match sep_history::record_success(
                        &out_dir,
                        now_ms(),
                        &input,
                        &vocals,
                        &accompaniment,
                    ) {
                        Ok(_) => "两轨已生成 · 已写入分离历史 · 可分别试听和导出".to_string(),
                        Err(e) => format!("两轨已生成，但分离历史写入失败：{e}"),
                    };
                    ui.set_sep_busy(false);
                    ui.set_sep_progress(1.0);
                    ui.set_sep_has_result(true);
                    ui.set_sep_result_available(true);
                    ui.set_sep_vocals_label(format!("人声 · {}", file_label(&vocals)).into());
                    ui.set_sep_accompaniment_label(
                        format!("伴奏 · {}", file_label(&accompaniment)).into(),
                    );
                    ui.set_sep_status_text(history_note.clone().into());
                    ui.set_status_text(history_note.clone().into());
                    finish_task(
                        ui,
                        state,
                        &state.sep_task,
                        tasks::TaskState::Done,
                        history_note,
                    );
                    *state.sep_tracks.borrow_mut() = Some((vocals, accompaniment));
                    refresh_separation_history(ui, state);
                }
            }
            Msg::SeparationStopped { task_id } => {
                if state.sep_task.get() == Some(task_id) {
                    ui.set_sep_busy(false);
                    ui.set_sep_progress(0.0);
                    ui.set_sep_has_result(false);
                    ui.set_sep_result_available(false);
                    // 上游没有取消 API：已经跑掉的算力收不回，这里如实说
                    let note = "已停止（本轮结果已丢弃，没有落盘；分离跑完前无法中断，耗时照算）"
                        .to_string();
                    ui.set_sep_status_text(note.clone().into());
                    ui.set_status_text(note.clone().into());
                    finish_task(ui, state, &state.sep_task, tasks::TaskState::Stopped, note);
                }
            }
            Msg::SeparationFailed { task_id, error } => {
                if state.sep_task.get() == Some(task_id) {
                    ui.set_sep_busy(false);
                    ui.set_sep_progress(0.0);
                    ui.set_sep_has_result(false);
                    ui.set_sep_result_available(false);
                    ui.set_sep_status_text(error.clone().into());
                    ui.set_status_text(format!("人声分离失败：{error}").into());
                    finish_task(ui, state, &state.sep_task, tasks::TaskState::Failed, error);
                }
            }
            Msg::EvalProgress {
                task_id,
                done,
                total,
                model,
            } => {
                if state.eval_task.get() == Some(task_id) {
                    let note = format!("质检中：第 {done}/{total} 句 · 回读 {model}");
                    ui.set_status_text(note.clone().into());
                    progress_task(
                        ui,
                        state,
                        &state.eval_task,
                        done as f32 / total.max(1) as f32,
                        note,
                    );
                }
            }
            Msg::EvalDone { task_id, summary } => {
                if state.eval_task.get() != Some(task_id) {
                    continue;
                }
                // summary.scores 是**这次跑完之后工程里的完整分数集**（本次评上的 +
                // ASR 失败但保留的旧分），每项自带来源模型，所以这里是整体替换，与磁盘一致。
                replace_eval_ledger(state, &summary.scores);
                // 新的一轮质检结束后先回工程原序；排序要不要开由用户点按钮决定。
                state.qa_sorted.set(false);
                apply_row_order(rows, restore_project_order(rows_as_vec(rows)));
                apply_eval_labels(rows, &state.eval_scores.borrow());
                let mut note = eval_summary_note(&summary);
                // 质检是用户主动发起的"找问题"动作：跑完先把最差那句选中，
                // 用户也可以随时用「跳到最差句」重新定位并滚动。
                //
                // 同源：自动选中和「跳到最差句」共用 `eval_done_selection`（也就是
                // `lowest_scored_index`）的推导，不再各写一份。
                // 借用分两句写：临时借用活到语句末尾，后续要改这块时不容易踩 RefCell。
                let (auto_index, auto_note) =
                    eval_done_selection(&rows_as_vec(rows), &state.eval_scores.borrow());
                if let Some(index) = auto_index {
                    if let Some(i) = row_position(rows, index) {
                        ui.set_selected(i as i32);
                    }
                } else {
                    ui.set_selected(-1);
                }
                note.push_str(&auto_note);
                // 一句都没评上分 = 这次质检没得出结论，不能标成绿色的"完成"
                let outcome = if summary.scored == 0 {
                    tasks::TaskState::Failed
                } else {
                    tasks::TaskState::Done
                };
                finish_task(ui, state, &state.eval_task, outcome, note.clone());
                sync_qa_actions(ui, rows, state);
                ui.set_status_text(note.into());
            }
            Msg::EvalStopped { task_id } => {
                if state.eval_task.get() != Some(task_id) {
                    continue;
                }
                let note = "质检已停止（已回读的句子不影响工程）".to_string();
                finish_task(
                    ui,
                    state,
                    &state.eval_task,
                    tasks::TaskState::Stopped,
                    note.clone(),
                );
                ui.set_status_text(note.into());
            }
            Msg::EvalFailed { task_id, error } => {
                if state.eval_task.get() != Some(task_id) {
                    continue;
                }
                finish_task(
                    ui,
                    state,
                    &state.eval_task,
                    tasks::TaskState::Failed,
                    error.clone(),
                );
                ui.set_status_text(format!("质检失败：{error}").into());
            }
            Msg::SongDone {
                task_id,
                path,
                duration,
            } => {
                if state.song_task.get() != Some(task_id) {
                    continue;
                }
                ui.set_song_busy(false);
                ui.set_song_queued(false);
                finish_task(
                    ui,
                    state,
                    &state.song_task,
                    tasks::TaskState::Done,
                    format!("成品 {duration:.1}s"),
                );
                ui.set_song_has_result(true);
                let note = format!("歌曲完成：{duration:.1}s · {}", path.display());
                ui.set_song_status_text(note.clone().into());
                ui.set_status_text(note.into());
                *state.song_artifact.borrow_mut() = Some((path, duration));
            }
            Msg::SongStopped { task_id } => {
                if state.song_task.get() != Some(task_id) {
                    continue;
                }
                ui.set_song_busy(false);
                ui.set_song_queued(false);
                ui.set_song_has_result(false);
                let note = "歌曲已从队列中移除（还没开始跑）".to_string();
                ui.set_song_status_text(note.clone().into());
                ui.set_status_text(note.clone().into());
                finish_task(ui, state, &state.song_task, tasks::TaskState::Stopped, note);
            }
            Msg::SongFailed { task_id, error } => {
                if state.song_task.get() != Some(task_id) {
                    continue;
                }
                ui.set_song_busy(false);
                ui.set_song_queued(false);
                finish_task(
                    ui,
                    state,
                    &state.song_task,
                    tasks::TaskState::Failed,
                    error.clone(),
                );
                ui.set_song_has_result(false);
                ui.set_song_status_text(error.clone().into());
                ui.set_status_text(error.into());
            }
            Msg::DownloadUpdate(snap) => {
                // 队列线程推来的快照：同一模型只保留最新那条任务的，重建列表。
                let label = snap.label.clone();
                let id = snap.id;
                let terminal = snap.state.is_terminal();
                // 先记下"这条终态是不是成功"：`snap` 马上就要被 merge 吃掉
                let finished_ok = matches!(snap.state, download::State::Done);
                {
                    let mut list = state.downloads.borrow_mut();
                    merge_download_snapshot(&mut list, snap);
                }
                if terminal {
                    // 终态才摘登记：中途摘掉会让「取消」按钮又变成「下载」再排一条。
                    // 而且要**认领自己的 id**——旧任务的迟到终态不能把新任务摘掉（复核 B1）。
                    release_download_id(&mut state.download_ids.borrow_mut(), &label, id);
                    if finished_ok {
                        // 下完第一个模型，随包引擎才第一次有可能起来（它没有模型会拒绝启动）。
                        // 只记日志：失败原因已经能通过状态栏的后端标签与 /health 看到，
                        // 在这里抢状态行会把"下载成功"的提示顶掉。
                        if let engine_supervisor::StartOutcome::Failed(why) =
                            ensure_engine_serving()
                        {
                            eprintln!("模型下载完成，但随包引擎没能起来：{why}");
                        }
                    }
                }
                refresh_download_rows(ui, state);
            }
            Msg::BatchExportDone { dir, outcome } => {
                state.batch_export_running.set(false);
                let text = match outcome {
                    export::BatchExportOutcome::Done(summary) => {
                        export::summary_text(&summary, &dir)
                    }
                    export::BatchExportOutcome::NoneSelected => {
                        "未选择导出格式：先选「整段 WAV」或「逐句 SRT」".to_string()
                    }
                    export::BatchExportOutcome::Failed(e) => format!("批量导出失败：{e}"),
                };
                ui.set_status_text(text.into());
            }
            Msg::DictFilePicked { pick } => {
                let path = match pick {
                    picker::Outcome::Picked(p) => p,
                    picker::Outcome::Cancelled => {
                        ui.set_status_text("取消了导入词条".into());
                        return;
                    }
                    picker::Outcome::Unavailable(trouble) => {
                        ui.set_status_text(format!("没能导入词条：{}", trouble.note()).into());
                        return;
                    }
                };
                // 文件框是异步的：打开期间可能已经起了**任何**任务（含歌曲/分离这类不设
                // 全局 running/busy 的），所以这里用与点击时同一个忙判据再查一次。
                let busy = dictionary_controls_busy(ui, state);
                let active_file = state.active_dict_file.borrow().clone();
                match import_entries_into_library(
                    &dictionaries_root(),
                    Path::new(&path),
                    active_file.as_deref(),
                    busy,
                    now_ms(),
                ) {
                    Ok(applied) => {
                        let skipped = if applied.skipped.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "，跳过 {} 条（{}）",
                                applied.skipped.len(),
                                applied.skipped[0]
                            )
                        };
                        activate_dictionary(
                            ui,
                            rows,
                            state,
                            cmd_tx,
                            Some(applied.entry.file.clone()),
                        );
                        ui.set_status_text(
                            format!(
                                "已导入 {} 条词条到「{}」{}",
                                applied.imported, applied.entry.name, skipped
                            )
                            .into(),
                        );
                    }
                    Err(e) => ui.set_status_text(e.into()),
                }
            }
            Msg::VoiceImportDirPicked { pick } => {
                let dir = match pick {
                    picker::Outcome::Picked(p) => p,
                    picker::Outcome::Cancelled => {
                        ui.set_status_text("取消了导入音色".into());
                        return;
                    }
                    picker::Outcome::Unavailable(trouble) => {
                        ui.set_status_text(format!("没能导入音色：{}", trouble.note()).into());
                        return;
                    }
                };
                match voices::import_from(&voices_root(), Path::new(&dir), now_ms(), file_stem) {
                    Ok(entry) => {
                        refresh_voice_library(ui);
                        ui.set_status_text(
                            format!("已导入音色「{}」（自包含：音频已复制进库）", entry.name)
                                .into(),
                        );
                    }
                    Err(e) => ui.set_status_text(e.into()),
                }
            }
            Msg::VoiceFilesPicked { pick } => {
                let paths = match pick {
                    picker::Outcome::Picked(paths) => paths,
                    picker::Outcome::Cancelled => {
                        ui.set_status_text("取消了添加音频".into());
                        return;
                    }
                    picker::Outcome::Unavailable(trouble) => {
                        ui.set_status_text(format!("没能添加音频：{}", trouble.note()).into());
                        return;
                    }
                };
                if paths.is_empty() {
                    ui.set_status_text("没有选到音频文件".into());
                    return;
                }
                // 逐个按文件名入库：单文件失败不中断整批（失败原因在 report 里）
                let report =
                    voices::import_audio_files(&voices_root(), &paths, now_ms(), file_stem);
                refresh_voice_library(ui);
                let mut note = format!("已加入 {} 个音色（同名会覆盖）", report.imported);
                if report.failed > 0 {
                    match &report.first_error {
                        Some(e) => note.push_str(&format!("；失败 {} 个：{e}", report.failed)),
                        None => note.push_str(&format!("；失败 {} 个", report.failed)),
                    }
                }
                ui.set_status_text(note.into());
            }
            Msg::BatchScriptsPicked { pick } => {
                let paths = match pick {
                    picker::Outcome::Picked(paths) => paths,
                    // 用户取消：no-op，不能把已经导入好的那份列表清掉——
                    // "取消"不该有破坏性副作用（复核提的 UX 残留）。
                    picker::Outcome::Cancelled => {
                        let note = if state.batch_rows.borrow().is_empty() {
                            "没有选稿件".to_string()
                        } else {
                            "已取消选择，列表不变".to_string()
                        };
                        refresh_batch_summary(ui, state, &note);
                        ui.set_status_text(note.into());
                        continue;
                    }
                    picker::Outcome::Unavailable(trouble) => {
                        let note = format!("没能选择稿件：{}", trouble.note());
                        refresh_batch_summary(ui, state, &note);
                        ui.set_status_text(note.into());
                        continue;
                    }
                };
                // 走到这里说明对话框确实给了结果；空表只可能是解析异常（取消已经在
                // 上面分流），仍然按 no-op 处理，不拿它去清用户的列表。
                if paths.is_empty() {
                    let note = if state.batch_rows.borrow().is_empty() {
                        "没有选稿件".to_string()
                    } else {
                        "已取消选择，列表不变".to_string()
                    };
                    refresh_batch_summary(ui, state, &note);
                    ui.set_status_text(note.into());
                    continue;
                }
                // 一次导入 N 篇：读文件、跳过有问题的、按顺序建行（纯逻辑在 batch.rs）
                let outcome = batch::import_scripts(&paths, file_stem);
                {
                    let mut rows = state.batch_rows.borrow_mut();
                    rows.clear();
                    for item in &outcome.items {
                        rows.push(BatchRowState {
                            name: item.name.clone(),
                            script: item.script.clone(),
                            sentences: 0,
                            task_id: None,
                            state: batch::ItemState::Waiting,
                            detail: "待跑".into(),
                            out: None,
                        });
                    }
                }
                *state.batch_skipped_notes.borrow_mut() = outcome.skipped.clone();
                refresh_batch_rows(ui, state);
                let tail = if outcome.items.is_empty() {
                    if outcome.skipped.is_empty() {
                        "没有选文件".to_string()
                    } else {
                        "一篇都没能导入：看上面的原因".to_string()
                    }
                } else {
                    format!("已导入 {} 篇，点「开始批量」按顺序跑", outcome.items.len())
                };
                refresh_batch_summary(ui, state, &tail);
                ui.set_status_text(tail.into());
            }
            Msg::BatchItemStarted {
                task_id,
                index,
                total,
                name,
            } => {
                if let Some(i) = batch_row_index_for_task(state, task_id) {
                    let mut rows = state.batch_rows.borrow_mut();
                    rows[i].state = batch::ItemState::Running;
                    rows[i].detail = format!("第 {}/{} 篇 · 正在合成", index + 1, total);
                }
                ui.set_status_text(
                    format!("批量：正在合成第 {}/{} 篇「{name}」", index + 1, total).into(),
                );
                refresh_batch_rows(ui, state);
            }
            Msg::BatchItemProgress {
                task_id,
                index,
                done,
                total,
            } => {
                let detail = format!("第 {done}/{total} 句");
                if let Some(i) = batch_row_index_for_task(state, task_id) {
                    let mut rows = state.batch_rows.borrow_mut();
                    rows[i].sentences = total;
                    rows[i].detail = detail.clone();
                }
                state.tasks.borrow_mut().progress(
                    task_id,
                    done as f32 / total.max(1) as f32,
                    detail,
                );
                refresh_batch_rows(ui, state);
                refresh_tasks(ui, state);
                ui.set_status_text(
                    format!("批量：第 {} 篇 · {}", index + 1, "正在逐句合成").into(),
                );
            }
            Msg::BatchItemDone {
                task_id,
                index,
                name,
                wav,
                srt,
                failed,
                reused,
                skipped,
                error,
                note,
            } => {
                let (item_state, detail, task_state) = if skipped {
                    (
                        batch::ItemState::Skipped,
                        note.unwrap_or_else(|| "排队中被取消，没有跑".to_string()),
                        tasks::TaskState::Stopped,
                    )
                } else if let Some(e) = error {
                    (
                        batch::ItemState::Failed,
                        e.clone(),
                        tasks::TaskState::Failed,
                    )
                } else {
                    // 续跑复用是这条能力的卖点之一：说清楚这次跑实际合成了几句
                    let reuse_note = if reused > 0 {
                        format!(" · 复用 {reused} 句")
                    } else {
                        String::new()
                    };
                    let failed_note = if failed > 0 {
                        format!(" · {failed} 句失败")
                    } else {
                        String::new()
                    };
                    (
                        batch::ItemState::Done,
                        format!("已出片{failed_note}{reuse_note}"),
                        tasks::TaskState::Done,
                    )
                };
                if let Some(i) = batch_row_index_for_task(state, task_id) {
                    let mut rows = state.batch_rows.borrow_mut();
                    rows[i].state = item_state;
                    rows[i].detail = detail.clone();
                    if let (Some(wav), Some(srt)) = (wav, srt) {
                        rows[i].out = Some((wav, srt));
                    }
                }
                finish_task_by_id(ui, state, task_id, task_state, detail.clone());
                refresh_batch_rows(ui, state);
                ui.set_status_text(format!("批量：第 {} 篇「{name}」{}", index + 1, detail).into());
            }
            Msg::BatchDone {
                done,
                failed,
                skipped,
                stopped,
            } => {
                state.batch_running.set(false);
                ui.set_batch_running(false);
                let summary = batch::summary_text(done, failed, skipped, stopped);
                // 出片的目录要说出来：批量跑完最实际的问题是"我的文件在哪"
                let tail = match state
                    .batch_rows
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|r| r.out.as_ref())
                    .and_then(|(wav, _)| wav.parent())
                {
                    Some(dir) => format!("{summary} · 成品目录 {}", dir.display()),
                    None => summary.clone(),
                };
                refresh_batch_summary(ui, state, &tail);
                refresh_batch_rows(ui, state);
                ui.set_status_text(format!("批量结束：{summary}").into());
            }
            Msg::Assembled {
                wav,
                srt,
                duration,
                done,
                skipped,
            } => {
                ui.set_busy(false);
                *state.assembled.borrow_mut() = Some(AssembledInfo {
                    wav: wav.clone(),
                    duration,
                });
                ui.set_has_result(true);
                let skipped_note = if skipped > 0 {
                    format!(" · 跳过 {skipped} 句失败句")
                } else {
                    String::new()
                };
                // 单篇导出与批量导出共用同一份实现（src/export.rs）：
                // 复制语义、覆盖语义、失败文案只有一处
                let export_outcome = export::export_one(
                    &stem_of(ui),
                    &PathBuf::from(ui.get_export_dir().to_string()),
                    &wav,
                    Some(&srt),
                    ui.get_export_wav_on(),
                    ui.get_export_srt_on(),
                );
                let export_note = match export_outcome {
                    export::ExportOutcome::Exported(path) => {
                        toast(ui, &format!("已导出 {}", path.display()));
                        format!(" · 已导出 {}", path.display())
                    }
                    export::ExportOutcome::NoneSelected => " · 未选择导出格式，仅更新成品".into(),
                    export::ExportOutcome::Failed(e) => format!(" · 导出失败: {e}"),
                };
                ui.set_status_text(
                    format!("成品 {duration:.1}s（{done} 句{skipped_note}）{export_note}").into(),
                );
            }
        }
    }
    if let Some((failed, stopped, reused, error)) = run_finished {
        ui.set_running(false);
        let n = rows.row_count() as i32;
        let done = ui.get_done_count();
        ui.set_progress(done as f32 / n.max(1) as f32);
        ui.set_status_text(
            run_finished_note(failed, stopped, reused, done, n, error.as_deref()).into(),
        );
        if done > 0 {
            ui.set_has_result(true);
        }
    }

    // ── 配音成品就绪 → UI（BGM 的混音前置条件）──
    // 单一真相是 state.assembled（拼装成功时置、作废工程时清），每 tick 同步一次；
    // 值没变时 Slint 不会重绘。
    ui.set_dub_product_ready(state.assembled.borrow().is_some());

    // ── 一键备份能不能点：与点击回调同一份判据（改动见 backup_blocked 的注释）──
    refresh_backup_availability(ui, state);

    // ── 检查更新/打开发布页能不能点：同上，只做投影 ──
    refresh_update_availability(ui, state);

    // ── 音乐制作主按钮能不能点 + 原因：同一处投影，不在 Slint 重拼 ──
    refresh_song_action(ui, state);

    // ── 音色设计「生成」能不能点 + 原因：同上，单点投影 ──
    refresh_design_action(ui, state);

    // ── 托管引擎周期自愈（每约 30s 问一次；显式地址 = 用户自己的服务，恒跳过）──
    // 判据是 engine_supervisor::should_periodic_heal 的纯函数（显式跳过 / 节流内
    // 不重试 / 间隔外重试）。**重拉本身会阻塞（最坏等引擎 30s）**，所以只在 tick 里
    // 决定“该问了”，真正的探测/拉起放在后台线程，结论经 Msg::EngineSelfHeal 回传，
    // 绝不冻界面。192.9s 参考音把引擎打死之后，靠这里把服务拉回来。
    let heal_now = Instant::now();
    if engine_supervisor::should_periodic_heal(
        state.engine_explicit.get(),
        state.last_engine_heal.get(),
        heal_now,
    ) {
        state.last_engine_heal.set(Some(heal_now));
        let tx = msg_tx.clone();
        std::thread::spawn(move || {
            let note = match ensure_engine_serving_throttled() {
                Some(engine_supervisor::StartOutcome::Started) => {
                    Some("随包引擎已重新拉起，服务已恢复".to_string())
                }
                Some(engine_supervisor::StartOutcome::Failed(why)) => {
                    Some(format!("随包引擎自愈失败：{why}"))
                }
                // Reused（外部服务在响应）/ EngineMissing / NotConfigured 都是稳定态，
                // 不写状态行（不刷屏）
                Some(_) => None,
                // 健康在响应 / 节流挡住：无事发生
                None => None,
            };
            let _ = tx.send(WorkerMsg {
                revision: 0,
                msg: Msg::EngineSelfHeal { note },
            });
        });
    }

    // ── 试听结束：rodio 队列播空 → 复位 playing ──
    if ui.get_playing() && !player.is_playing() {
        ui.set_playing(false);
        ui.set_status_text("试听结束".into());
    }

    // ── 兜底规则开关（PixelSwitch 没有回调，只能比对上一次的值）──
    sync_auto_normalize_toggle(ui, state);

    // ── 质检按钮：enabled 是排序 / 跳转共用的唯一判据；busy/running 变化要跟上 ──
    sync_qa_actions(ui, rows, state);

    // ── 任务中心的"已排队 / 已运行 N"走字（按秒节流）──
    maybe_refresh_task_times(ui, state);
}

fn stem_of(ui: &MainWindow) -> String {
    file_stem(&ui.get_project_name())
}

// ===========================================================================
// 试听
// ===========================================================================

/// 「试听全篇」的播放计划（纯函数，`docs/streaming-preview.md` §3 Phase 1）。
///
/// 成品（final.wav 存在）优先；没有成品时按工程 index 排序，只取「已合成」的
/// **连续前缀**——遇到第一句未完成（待合成/合成中/失败）即停，不静默跳过空洞。
/// `duration` 与 `paths` 在同一趟遍历里产生（成品=其时长、前缀=各句时长之和），
/// 调用方直接喂 `playing_total`，不用再各自算一遍总时长（两份实现会漂移）。
struct ListenPlan {
    /// 按播放顺序排列的 wav：成品只有 1 个；前缀播放有 k 个；空前缀为空
    paths: Vec<PathBuf>,
    /// 状态行文案（成品 / 缺口 / 空前缀 / 全完成 各有固定措辞）
    note: String,
    /// 本次连播总时长（秒）
    duration: f32,
}

/// 成品（final.wav 存在）→ 只播成品；否则按工程 index 取已完成句的连续前缀。
///
/// `running` 只改文案不改计划：运行中点「试听全篇」同样只播已完成句，
/// 播到第一句未完成为止（计划里本来就没有空洞后面的句子）。
fn listen_plan(
    rows: &[Sentence],
    dir: &Path,
    assembled: Option<&AssembledInfo>,
    running: bool,
) -> ListenPlan {
    if let Some(info) = assembled {
        if info.wav.is_file() {
            return ListenPlan {
                paths: vec![info.wav.clone()],
                note: "试听全篇成品".to_string(),
                duration: info.duration as f32,
            };
        }
    }
    // 质检排序只影响屏幕顺序；试听计划必须回到工程时间顺序。
    let mut sorted: Vec<&Sentence> = rows.iter().collect();
    sorted.sort_by_key(|r| r.index);
    // 连续前缀：从第 1 句起逐句都要「已合成」；第一句未完成即停，
    // 空洞后面的完成句不算（不静默跳过——见单测 listen_plan_stops_at_first_hole）。
    let mut paths = Vec::new();
    let mut duration = 0.0_f32;
    for row in &sorted {
        if row.status.as_str() != "已合成" {
            break;
        }
        let Ok(index) = usize::try_from(row.index) else {
            break;
        };
        paths.push(dir.join(format!("sentences/{index:03}.wav")));
        duration += row.duration;
    }
    let k = paths.len();
    let note = if running {
        if k == 0 {
            "合成中：还没有已合成的句子，等第 1 句完成再试听".to_string()
        } else {
            match sorted.get(k) {
                // 下一句（第 k+1 句）还没合成：用它的展示句号（no 是 1 基「第 N 句」）
                Some(next) => format!("合成中：连播已完成的 {k} 句；第 {} 句还没合成", next.no),
                // 全部完成却仍在 running（RunDone 未到）：保持全完成的原文案
                None => format!("试听全篇（{k} 句连播，未拼间隙）"),
            }
        }
    } else if k == 0 {
        "还没有已合成的句子".to_string()
    } else if k < sorted.len() {
        format!(
            "试听全篇（已完成的 {k} 句连播；第 {} 句未合成）",
            sorted[k].no
        )
    } else {
        format!("试听全篇（{k} 句连播，未拼间隙）")
    };
    ListenPlan {
        paths,
        note,
        duration,
    }
}

/// 逐句试听：播 sentences/NNN.wav，播放头按该句时长推进。
fn play_sentence(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
    display_index: usize,
    duration: f32,
) {
    let Some(row) = rows.row_data(display_index) else {
        return;
    };
    if row.status.as_str() != "已合成" {
        ui.set_status_text("这句还没合成：先点「开始合成」".into());
        return;
    }
    let project_index = match usize::try_from(row.index) {
        Ok(index) => index,
        Err(_) => return,
    };
    let Some(dir) = state.project_dir.borrow().clone() else {
        ui.set_status_text("先跑一次合成".into());
        return;
    };
    let wav = dir.join(format!("sentences/{project_index:03}.wav"));
    if !wav.is_file() {
        ui.set_status_text("这句还没合成：先点「开始合成」".into());
        return;
    }
    match player.play_wav(&wav) {
        Ok(()) => {
            state.playing_total.set(duration.max(0.01));
            ui.set_playing(true);
            ui.set_status_text(format!("试听第 {} 句：{}", row.no, row.text).into());
        }
        Err(e) => ui.set_status_text(e.into()),
    }
}

/// 全篇试听：有成品播 final.wav，否则按工程顺序连播已完成句的连续前缀。
///
/// 成品优先 / 连续前缀 / 文案都在 `listen_plan` 纯函数里；这里只做界面交互：
/// 拿计划 → 空计划只说明不播 → `play_many` 连播 → 播放头总时长。
fn play_all(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
) {
    let assembled = state.assembled.borrow();
    let has_final = assembled.as_ref().is_some_and(|i| i.wav.is_file());
    if !has_final && state.project_dir.borrow().is_none() {
        ui.set_status_text("先跑一次合成".into());
        return;
    }
    // 成品分支不依赖工程目录（final.wav 是绝对路径）；没有成品时 dir 必为 Some（上面拦过）。
    let dir = state.project_dir.borrow().clone().unwrap_or_default();
    let row_vec: Vec<Sentence> = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .collect();
    let plan = listen_plan(&row_vec, &dir, assembled.as_ref(), ui.get_running());
    drop(assembled);
    if plan.paths.is_empty() {
        // 还没有可播的句子（运行中/空闲都适用）：只给说明，不启动播放器
        ui.set_status_text(plan.note.into());
        return;
    }
    match player.play_many(&plan.paths) {
        Ok(()) => {
            state.playing_total.set(plan.duration.max(0.01));
            ui.set_playing(true);
            ui.set_status_text(plan.note.into());
        }
        Err(e) => ui.set_status_text(e.into()),
    }
}

/// `AW_UI_STATE=download-source` 的两行对照（演示/核对用）。
///
/// 单独一个函数是为了让"改写 URL 只允许一处调用"那条源码守卫能一眼认出演示调用：
/// 守卫按**函数名**精确排除整个函数体（函数名改了就会失去豁免、守卫会红）。
/// 它只 `eprintln!`，不参与任何真实请求。
#[cfg(debug_assertions)]
fn download_source_demo_lines() -> Vec<String> {
    vec![
        format!(
            "AW_UI_STATE=download-source：改写示例 https://huggingface.co/a/b.gguf -> {}",
            download_mirror::rewrite_url(
                "https://huggingface.co/a/b.gguf",
                "https://hf-mirror.com"
            )
        ),
        format!(
            "AW_UI_STATE=download-source：非 HF 原样 https://example.com/x -> {}",
            download_mirror::rewrite_url("https://example.com/x", "https://hf-mirror.com")
        ),
    ]
}

/// 全局设置：服务地址 / 端口 / 模型清单路径。
///
/// 只改**本应用**连哪个服务、从哪份清单读模型；不改写 audio.cpp 自己的配置
/// （服务监听地址由 audio-service 的启动参数 / 清单决定）。
fn wire_global_settings(
    ui: &MainWindow,
    msg_tx: &Sender<WorkerMsg>,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    let msg = msg_tx.clone();
    ui.on_apply_server_settings(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 运行中不许换服务：换服务会让 in-flight 任务的终态消息按 revision 被过滤，
        // 台账就会永久停在"运行中"。目前 UI 靠 drawer 的 busy 绑定挡住，
        // 这里再加一道，避免以后把按钮移出抽屉时重新踩坑。
        if ui.get_running() || ui.get_busy() {
            ui.set_server_status("任务进行中：等当前任务结束再改服务设置".into());
            return;
        }
        let locked = ui.get_server_locked();
        let host = ui.get_server_host().trim().to_string();
        let port_raw = ui.get_server_port().trim().to_string();
        let dir = ui.get_model_dir().trim().to_string();
        // 下载源镜像 + 并发（P7 第二段）。镜像对**下一条入队**立即生效；
        // 并发数要改线程池，所以**下次启动**才生效（回显里写明）。
        let mirror_raw = ui.get_download_mirror().trim().to_string();
        let conc_raw = ui.get_download_concurrency().trim().to_string();

        let port = if port_raw.is_empty() {
            None
        } else {
            match port_raw.parse::<u16>() {
                Ok(p) => Some(p),
                Err(_) => {
                    ui.set_server_ok(false);
                    ui.set_server_status("端口要是 1–65535 的数字".into());
                    return;
                }
            }
        };
        // 目录不存在**不阻断保存**：默认目录（进程工作目录下的 models/）在开发与打包
        // 环境里常常还不存在，阻断会让"只想改端口"的保存连带失败。存在与否只做提示。
        let dir_missing = !dir.is_empty() && !Path::new(&dir).is_dir();

        // 并发数：空 = 用默认。填了但不在 1..=4 -> **如实报错**，不静默夹
        // （用户写 9 被悄悄改成 4，等于"设置没生效但界面说保存成功"）。
        let conc = if conc_raw.is_empty() {
            None
        } else {
            match conc_raw.parse::<u32>() {
                Ok(n) if (1..=download::MAX_CONCURRENCY).contains(&n) => Some(n),
                Ok(n) => {
                    ui.set_server_ok(false);
                    ui.set_server_status(
                        format!(
                            "并发数要在 1-{} 之间（你填的是 {n}）",
                            download::MAX_CONCURRENCY
                        )
                        .into(),
                    );
                    return;
                }
                Err(_) => {
                    ui.set_server_ok(false);
                    ui.set_server_status("并发数要是整数".into());
                    return;
                }
            }
        };
        // 镜像前缀：先校验（必须带 http(s)://，否则会拼成相对路径）、再归一
        // （去掉尾斜杠——不归一的话"当前生效的源"回显会带一串斜杠，像另一个地址）。
        let mirror = match download_mirror::validate_prefix(&mirror_raw) {
            Ok(m) => m,
            Err(e) => {
                ui.set_server_ok(false);
                ui.set_server_status(e.into());
                return;
            }
        };

        // 与默认目录相同时存 None（而不是把当时的绝对路径固化下来）：
        // 应用以后换位置/换工作目录时，默认值应该跟着走，不该被旧快照钉死。
        let is_default_dir = Path::new(dir.as_str()) == default_model_dir().as_path();
        // AW_SERVER 生效时输入是锁住的：不要把这时的显示值固化进配置，保留原有 host/port
        let prev = settings_snapshot();
        let next = AppSettings {
            host: if locked {
                prev.host
            } else {
                (!host.is_empty()).then_some(host)
            },
            port: if locked { prev.port } else { port },
            model_dir: (!dir.is_empty() && !is_default_dir).then_some(dir.clone()),
            // BGM 输入不在这里改（有自己的落盘点 save_bgm_settings），原样带上
            bgm: prev.bgm,
            // 词典选择也不在这里改（有自己的落盘点），原样带上
            dictionary: prev.dictionary,
            // 「更新」的清单地址也不在这里改（有自己的落盘点 save_update_url），原样带上
            update_url: prev.update_url,
            // 质检回读模型也不在这里改（自己的落盘点 persist_asr_model），原样带上
            asr_model: prev.asr_model,
            // 音色设计模型也不在这里改（自己的落盘点 persist_design_model），原样带上
            design_model: prev.design_model,
            // 下载源镜像 / 并发：本批自己的值，就在这里写
            download_mirror: mirror.clone(),
            download_concurrency: conc,
        };
        if let Err(e) = save_settings(&next) {
            ui.set_server_ok(false);
            ui.set_server_status(format!("设置保存失败：{e}").into());
            return;
        }
        if let Ok(mut g) = settings().lock() {
            *g = next;
        }
        // 服务地址改了，周期自愈的"显式地址恒跳过"判据输入要跟着刷新
        st.engine_explicit.set(server_base_for_engine().1);
        apply_engine_discovery(&ui, Some((&tx, &st)));
        refresh_download_rows(&ui, &st);
        // 回显是**投影**：从刚落盘的那份设置算，不在这里拼第二份判断
        // （`LESSON_同一语义两处实现必然漂移`）
        refresh_download_source_view(&ui);
        ui.set_server_status(
            if dir_missing {
                format!("设置已保存（模型目录还不存在：{dir}）· 正在测试连接…")
            } else {
                "设置已保存，正在测试连接…".into()
            }
            .into(),
        );
        spawn_server_check(msg.clone(), st.project_revision.get());
    });

    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    ui.on_rescan_models(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() {
            ui.set_server_status("任务进行中：等当前任务结束再重新扫描".into());
            return;
        }
        apply_engine_discovery(&ui, Some((&tx, &st)));
        refresh_download_rows(&ui, &st);
        refresh_download_source_view(&ui);
        ui.set_server_status("已重新扫描模型清单".into());
    });

    let weak = ui.as_weak();
    let st = state.clone();
    let msg = msg_tx.clone();
    ui.on_pick_model_dir(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_busy() || ui.get_running() {
            return;
        }
        ui.set_status_text("正在打开系统目录选择框…".into());
        spawn_folder_pick(msg.clone(), st.project_revision.get());
    });

    let weak = ui.as_weak();
    let st = state.clone();
    let msg = msg_tx.clone();
    ui.on_backup_all(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 按钮的 enabled 与这里必须用**同一份判据**：只让按钮变灰、回调不拦，
        // 快捷键/程序化触发照样能起备份；只拦回调、按钮不灰，用户会以为点了没反应。
        if let Some(refusal) = backup_refusal(
            ui.get_busy(),
            ui.get_running(),
            tasks_in_flight(&st),
            batch_in_flight(&st),
            st.backup_running.get(),
        ) {
            ui.set_backup_info(refusal.into());
            return;
        }
        // 在飞标志在**点下去那一刻**就要置位：目录选择框会把控制权交回事件循环，
        // 不置位的话连点两下就是两条后台线程（两条都去弹选择框、都往同一目标写）
        st.backup_running.set(true);
        ui.set_backup_running(true);
        ui.set_backup_info("正在打开系统目录选择框…".into());
        spawn_backup(msg.clone(), workshop_dir());
    });

    let weak = ui.as_weak();
    let st = state.clone();
    let msg = msg_tx.clone();
    ui.on_check_update(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 按钮的 enabled 与这里必须用**同一份判据**（改动见 update_refusal 的注释）
        if let Some(refusal) = update_refusal(st.update_running.get()) {
            ui.set_update_info(refusal.into());
            return;
        }
        // 在飞标志在**点下去那一刻**就要置位：网络请求是后台线程，不置位就能连点起一堆
        st.update_running.set(true);
        ui.set_update_running(true);
        // 上一次的发布页立刻作废：新检查没回来之前，那份已经不代表现在的判断
        *st.update_release.borrow_mut() = None;
        // 清单地址：界面上的值优先，留空回落官方地址；顺手存下来（内网镜像只填一次）
        let typed = ui.get_update_manifest_url().trim().to_string();
        let url = if typed.is_empty() {
            update::DEFAULT_MANIFEST_URL.to_string()
        } else {
            typed
        };
        save_update_url(&url);
        ui.set_update_info(format!("正在检查 {url} …").into());
        spawn_update_check(msg.clone(), url, update::CURRENT_VERSION.to_string());
    });

    let weak = ui.as_weak();
    let st = state.clone();
    ui.on_open_release_page(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 判据与按钮的 enabled 同源：手里没有待打开的发布页就什么都不做
        let url = st.update_release.borrow().as_ref().map(|r| r.url.clone());
        let Some(url) = url else {
            ui.set_update_info("还没有可打开的发布页：先点「检查更新」".into());
            return;
        };
        match open_external_url(&url) {
            Ok(()) => ui.set_update_info(format!("已在浏览器打开：{url}").into()),
            Err(error) => ui.set_update_info(error.into()),
        }
    });

    let weak = ui.as_weak();
    let st = state.clone();
    let msg = msg_tx.clone();
    ui.on_check_server(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_server_status("正在测试连接…".into());
        spawn_server_check(msg.clone(), st.project_revision.get());
    });
}

// ===========================================================================
// 行模型 / 数据构造
// ===========================================================================

/// 旁白块头：当前音色名 + 来源 + 阻断原因。只反映“能不能开始配音”这一件事，
/// 不把“稿件为空”等其它原因混进来（那由主按钮与空态承担）。
/// 当前引擎 / 参考音频能不能开工 —— **唯一判据**。
///
/// 配音主按钮的可用性、块头的阻断提示、内置音色行的可点性都从这一个结果派生，
/// 不在 Slint 里另拼一套条件（见 `LESSON_同一语义两处实现必然漂移` 的补充实例：
/// 控件禁用态与真实守卫各算各的，换个演示态就自相矛盾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceReadiness {
    /// 可以开工。
    Ready,
    /// 一个可选 TTS 引擎都没有（服务清单没配 / 读不到）。
    NoEngine,
    /// 填了参考音路径，但文件不在。
    ReferenceMissing,
    /// 该引擎**必须**提供参考音（index-tts2：不接 voice_ref 直接报错）。
    EngineNeedsReference,
    /// 给了参考音，但还差它的文本：**当前引擎**要求 `voice_ref` 与 `reference_text`
    /// 成对（audio8-tts 真机实测只给路径 → HTTP 500；index-tts2 不需要）。
    ReferenceTextMissing,
}

impl VoiceReadiness {
    /// 阻断原因（`Ready` 时为空串）。只留当前最重要的一条。
    fn note(self, engine: &str) -> String {
        match self {
            Self::Ready => String::new(),
            Self::NoEngine => "没有可用引擎：先在本机 audio.cpp 服务里配置 tts 模型".into(),
            Self::ReferenceMissing => "参考音频不存在或不可读：修正路径后再开始配音".into(),
            Self::EngineNeedsReference => {
                "这个引擎必须提供参考音频（不能只用内置音色）：在下面填一段参考 wav 再开始配音"
                    .into()
            }
            // 与提交处那次拦截**同一个函数**：界面提示与真正拦住用户的话逐字一致，
            // 不另写一句（见 LESSON_同一语义两处实现必然漂移）。
            Self::ReferenceTextMissing => reference_text_required_note(engine),
        }
    }
}

/// 判据入口：引擎行（可选）+ 参考音路径 / 是否可读 / 参考文本 → 能不能开工。
///
/// 四条规则合成**一个**结论，覆盖两组批次各自的语义：
/// ① 引擎硬要求参考音（audio-workshop 的 `requires.voice_ref`）；
/// ② 引擎硬要求参考文本（`requires.reference_text`，只约束填了参考音的情形）。
/// 文本那条**转调** `reference_text_missing`（与提交处同一份 trim 判断），
/// 不在这里另写一份 —— 两份判据迟早漂移。
fn voice_readiness(
    engine: Option<&Voice>,
    ref_path: &str,
    ref_exists: bool,
    ref_text: &str,
) -> VoiceReadiness {
    let Some(engine) = engine else {
        return VoiceReadiness::NoEngine;
    };
    let path = ref_path.trim();
    if !path.is_empty() && !ref_exists {
        return VoiceReadiness::ReferenceMissing;
    }
    if engine.requires_voice_ref && path.is_empty() {
        return VoiceReadiness::EngineNeedsReference;
    }
    let as_opt = |v: &str| non_empty(v.to_string());
    if reference_text_missing(
        engine.requires_reference_text,
        &as_opt(path),
        &as_opt(ref_text),
    ) {
        return VoiceReadiness::ReferenceTextMissing;
    }
    VoiceReadiness::Ready
}

/// 引擎行下面那行**只读**说明：硬要求 + 已知缺陷（都没有就是空串）。
///
/// `known_issues` 按 `config/models.schema.yaml` 的产品决策只做展示，
/// 不参与自动决策（不改产品决策，也不替用户做选择）。
fn engine_note(engine: Option<&Voice>) -> String {
    let Some(v) = engine else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    if v.requires_voice_ref {
        parts.push("该引擎必须提供参考音频（不接 voice_ref 会直接报错）".to_string());
    }
    if v.requires_reference_text {
        parts.push("克隆音色时参考文本必填（可点「自动转写」填上）".to_string());
    }
    if !v.known_issues.is_empty() {
        parts.push(format!("已知问题：{}", v.known_issues));
    }
    parts.join(" · ")
}

fn refresh_voice_labels(ui: &MainWindow) {
    let idx = ui.get_voice_index();
    // 选中的引擎行（`Voice` 带能力标记：是否硬要求参考音 + known_issues）。
    // 显示名与能力取同一行，避免"文案看 A、能力看 B"。
    let engine_row = (idx >= 0)
        .then(|| ui.get_voices().row_data(idx as usize))
        .flatten();
    ui.set_engine_label(
        engine_row
            .as_ref()
            .map(|v| v.name.clone())
            .unwrap_or_default(),
    );

    let ref_trimmed = ui.get_voice_ref_path().trim().to_string();
    let ref_text_trimmed = ui.get_voice_ref_text().trim().to_string();

    // 音色名：内置默认音色 / 克隆音色 · <参考音频文件名>（音色 ≠ 模型）
    if ref_trimmed.is_empty() {
        ui.set_voice_label("内置默认音色".into());
        ui.set_voice_source("内置".into());
    } else {
        let stem = Path::new(&ref_trimmed)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| ref_trimmed.clone());
        ui.set_voice_label(format!("克隆音色 · {stem}").into());
        ui.set_voice_source("克隆 · 参考音".into());
    }

    // 参考音可用性（决定试听 / 合成能不能开工）
    let exists = !ref_trimmed.is_empty() && Path::new(&ref_trimmed).is_file();
    ui.set_reference_exists(exists);

    // 参考音频时长回显：文本在这里算好下发，slint 只显示不算（判据只有
    // aw_core::ref_audio 一份，别在 UI 里再造）。超 15 秒不再红拦——使用时会自动
    // 取前 15 秒（prepare_reference_for_clone），这里如实回显将要发生什么。
    // 读不出时长 = fail-open（只显示"读不出"，不拦）。
    let duration_text = match non_empty(ref_trimmed.clone()) {
        Some(path) if Path::new(&path).is_file() => {
            match aw_core::reference_duration_seconds(Path::new(&path)) {
                Some(secs) if secs > aw_core::REFERENCE_MAX_SECONDS => format!(
                    "参考音频时长：{secs:.1} 秒 → 将使用前 {:.0} 秒（原文件未改动）",
                    aw_core::REFERENCE_MAX_SECONDS
                ),
                Some(secs) => format!("参考音频时长：{secs:.1} 秒"),
                None => "参考音频时长：读不出（不影响合成）".to_string(),
            }
        }
        _ => "".to_string(),
    };
    ui.set_reference_duration(duration_text.into());

    // 判据算一次，UI 只消费：主按钮可用性 / 内置音色行 / 阻断提示同一份结论。
    // 参考文本那条规则由 `voice_readiness` 内部转调 main 的 `reference_text_missing`
    // （与提交处同一份 trim 判断），这里不另写一份（见 LESSON_同一语义两处实现必然漂移）。
    let readiness = voice_readiness(engine_row.as_ref(), &ref_trimmed, exists, &ref_text_trimmed);
    let requires_ref = engine_row.as_ref().is_some_and(|v| v.requires_voice_ref);
    let requires_text = engine_row
        .as_ref()
        .is_some_and(|v| v.requires_reference_text);
    let engine_name = engine_row
        .as_ref()
        .map(|v| v.name.to_string())
        .unwrap_or_default();
    ui.set_engine_requires_voice_ref(requires_ref);
    ui.set_engine_requires_reference_text(requires_text);
    ui.set_engine_note(engine_note(engine_row.as_ref()).into());
    ui.set_dub_voice_ready(readiness == VoiceReadiness::Ready);
    ui.set_voice_hint(readiness.note(&engine_name).into());
}

fn selected_model(ui: &MainWindow) -> String {
    let index = ui.get_voice_index();
    if index < 0 {
        return String::new();
    }
    let idx = index as usize;
    ui.get_voices()
        .row_data(idx)
        .map(|v| v.name.to_string())
        .unwrap_or_default()
}

/// 选中引擎行：模型名与能力（是否要求参考文本等）都从**同一行**取，
/// 避免"文案看 A、能力看 B"（见 `LESSON_同一语义两处实现必然漂移`）。
fn selected_voice(ui: &MainWindow) -> Option<Voice> {
    let index = ui.get_voice_index();
    if index < 0 {
        return None;
    }
    ui.get_voices().row_data(index as usize)
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn set_row_duration(rows: &Rc<VecModel<Sentence>>, i: usize, duration: f32) {
    let Some(mut row) = rows.row_data(i) else {
        return;
    };
    row.duration = duration;
    row.duration_label = format!("{duration:.1}s").into();
    rows.set_row_data(i, row);
}

/// 切句 → 行模型（未合成句的时长按口播语速估算；合成后由真实时长覆盖）
fn build_rows(lines: &[String]) -> Vec<Sentence> {
    let mut start = 0.0_f32;
    let mut rows = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let duration = (line.chars().count() as f32 * SECS_PER_CHAR).max(0.6);
        rows.push(Sentence {
            index: i as i32,
            no: i as i32 + 1,
            text: line.as_str().into(),
            status: "待合成".into(),
            duration,
            start,
            duration_label: format!("{duration:.1}s").into(),
            start_label: clock_label(start).into(),
            eval_label: "".into(),
            error_detail: "".into(),
            oom: false,
            // 视图元数据只由第 0 行承载；先给所有行填默认值，排序后由 sync 回写。
            qa_enabled: false,
            qa_sorted: false,
            qa_scroll_y: 0.0,
        });
        start += duration;
    }
    rows
}

/// 切句（与 aw-core 同算法；UI 预览用，真实切句以 aw-core 为准）
fn split_for_preview(text: &str) -> Vec<String> {
    aw_core::split_sentences(text, DEFAULT_PUNCTUATION, MAX_CHARS)
}

/// 按工程真实 index 重排起始时间与总时长（合成拿到真实时长后调用）。
///
/// 质检排序会改变 `VecModel` 的显示顺序，时间轴绝不能跟着显示顺序重算，
/// 否则只是“看一下低分句”就会把每句的起始时间改错。
fn recompute_total(rows: &Rc<VecModel<Sentence>>) {
    let positions: HashMap<usize, usize> = (0..rows.row_count())
        .filter_map(|i| {
            rows.row_data(i)
                .and_then(|row| usize::try_from(row.index).ok())
                .map(|index| (index, i))
        })
        .collect();
    let mut items = rows_as_vec(rows);
    items.sort_by_key(|row| row.index);
    let mut start = 0.0_f32;
    for row in &mut items {
        row.start = start;
        row.start_label = clock_label(start).into();
        start += row.duration;
    }
    for row in items {
        if let Ok(index) = usize::try_from(row.index) {
            if let Some(&i) = positions.get(&index) {
                rows.set_row_data(i, row);
            }
        }
    }
}

const SAMPLE_SCRIPT: &str = "大家好，欢迎回到音频作坊。今天聊三件事。\
第一，声音是你自己的，素材不出机器，断网也能干活。\
第二，不按字收费，想生成多少就生成多少。\
第三，配音先行，BGM 和歌曲排在后面，成熟一个上一个。";

const SCENE_NOTES: [&str; 5] = [
    "配音：先选音色，再开始配音",
    "BGM：按描述生成，自动对齐配音时长并 ducking",
    "人声分离：后端未接入，占位",
    "音乐制作：写歌 / 文生音乐（yue2 · ace-step）",
    "音色设计：参考音频克隆 / 文本描述生成音色（task vdes / options.instruction）",
];

fn export_dir() -> String {
    documents_dir().join(WORKSHOP_DIR).display().to_string()
}

fn rebuild(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, text: &str) {
    let built = build_rows(&split_for_preview(text));

    rows.set_vec(built);
    recompute_total(rows);
    ui.set_selected(-1);
    ui.set_done_count(0);
    ui.set_has_result(false);
    ui.set_progress(0.0);
    ui.set_playing(false);
}

/// 状态标签映射：内部状态（aw-core）→ 界面文案。
///
/// `error: oom: …` 不在这里丢掉后半段：句子行要能直接看见 oom 与三个动作，
/// 不能只显示一个没有下一步信息的「失败」。详情仍然来自 aw-core 的同一份状态，
/// UI 不重新判断哪些错误算内存不足。
fn set_status(rows: &Rc<VecModel<Sentence>>, i: usize, status: &str) {
    let (label, error_detail) = if status == "done" {
        ("已合成", "")
    } else if status == "pending" {
        ("待合成", "")
    } else if status == "running" {
        ("合成中", "")
    } else if status.starts_with("error") {
        ("失败", sentence_error_detail(status))
    } else {
        (status, "")
    };
    let Some(mut row) = rows.row_data(i) else {
        return;
    };
    if row.status.as_str() == label && row.error_detail.as_str() == error_detail {
        return;
    }
    row.status = SharedString::from(label);
    row.error_detail = SharedString::from(error_detail);
    row.oom = error_detail.starts_with("oom") || error_detail.contains("内存不足");
    rows.set_row_data(i, row);
}

/// worker 的句级消息带的是工程 index；排序后必须先找回显示行位置。
fn set_status_by_project_index(rows: &Rc<VecModel<Sentence>>, project_index: usize, status: &str) {
    if let Some(i) = row_position(rows, project_index) {
        set_status(rows, i, status);
    }
}

/// 句级消息（`Msg::Sentence` 的负载）→ 行更新：**按工程 index 定位那一句**再写回
/// 时长与状态，而不是按显示行号。
///
/// 抽出来是为了让"排序视角下按行号写错句"这条路径有测试隔离：真正的调用点（`tick`
/// 里的 `Msg::Sentence`）需要 `MainWindow`，单测造不出来；这里只吃 rows + 消息负载，
/// 用例可以直接喂一份排好序的行模型。把下面两行改回按行号写
/// （`set_status(rows, project_index, …)` / `set_row_duration(rows, project_index, …)`）
/// 必须让 `sentence_message_lands_on_project_index_row` 变红。
fn apply_sentence_msg(
    rows: &Rc<VecModel<Sentence>>,
    project_index: usize,
    status: &str,
    duration: Option<f64>,
) {
    if let Some(d) = duration {
        set_row_duration_by_project_index(rows, project_index, d as f32);
        recompute_total(rows);
    }
    set_status_by_project_index(rows, project_index, status);
}

/// 同上：时长属于工程句子，不属于当前展示位置。
fn set_row_duration_by_project_index(
    rows: &Rc<VecModel<Sentence>>,
    project_index: usize,
    duration: f32,
) {
    if let Some(i) = row_position(rows, project_index) {
        set_row_duration(rows, i, duration);
    }
}

fn clock_label(secs: f32) -> String {
    let total = secs.max(0.0);
    let minutes = (total / 60.0) as u32;
    let seconds = (total % 60.0) as u32;
    format!("{minutes}:{seconds:02}")
}

/// 界面上显示的文件名（音轨标签用）。
fn file_label(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// 清掉分离 Tab 的“当前结果”槽位；历史列表和索引不在这里动。
fn clear_separation_result(ui: &MainWindow, state: &Rc<UiState>) {
    state.sep_tracks.borrow_mut().take();
    ui.set_sep_has_result(false);
    ui.set_sep_result_available(false);
    ui.set_sep_progress(0.0);
}

/// 「回看某条分离历史」要写进界面的那组值（纯投影，便于单测）。
struct SepHistoryView {
    input_path: String,
    input_summary: String,
    has_result: bool,
    progress: f32,
    result_available: bool,
    vocals_label: String,
    accompaniment_label: String,
    /// 可用时是工程内的两轨；不可用时**必须是 `None`**（不能留下上一条的两轨）。
    tracks: Option<(PathBuf, PathBuf)>,
    sep_status: String,
    main_status: String,
}

/// 一条历史 → 回看时要应用的界面状态。
///
/// 抽成纯函数是为了让"不可用条目要如实报原因、且不留下可试听的两轨"这条有测试隔离，
/// 闭包本身（`on_sep_history_open`）需要 `MainWindow`，单测造不出来。
fn sep_history_view_for(item: &SepHistoryItem) -> SepHistoryView {
    let entry = &item.entry;
    let label = file_label(Path::new(&entry.input_path));
    let (tracks, sep_status, main_status) = match item.tracks.paths() {
        Some((vocals, accompaniment)) => (
            Some((vocals.to_path_buf(), accompaniment.to_path_buf())),
            format!("历史结果：{label} · 可试听/导出"),
            format!("已切到分离历史：{label}"),
        ),
        None => (
            None,
            item.tracks.note.clone(),
            format!("这条分离历史不可用：{}", item.tracks.note),
        ),
    };
    SepHistoryView {
        input_path: entry.input_path.clone(),
        input_summary: format!("已回看：{label}"),
        has_result: true,
        progress: 1.0,
        result_available: item.tracks.available(),
        vocals_label: format!("人声 · {}", entry.vocals_file),
        accompaniment_label: format!("伴奏 · {}", entry.accompaniment_file),
        tracks,
        sep_status,
        main_status,
    }
}

/// 分离历史列表一次展示多少条（只投影最近几条，不提供翻页）。
const SEP_HISTORY_VISIBLE: usize = 5;

/// 读取 `stems_dir` 下的历史，投影成"时间倒序、最多 `SEP_HISTORY_VISIBLE` 条"的只读快照
/// 与状态行文案；每条的两轨在投影前都做过工程内安全解析。
///
/// 抽成纯函数（只吃目录，不碰 `MainWindow`）是为了让"倒序取最近 5 条"和"坏索引不当成
/// 没有历史"这两条行为有测试隔离——外层 `refresh_separation_history` 需要 `MainWindow`，
/// 单测里造不出来。
fn separation_history_view(stems_dir: &Path) -> (String, Vec<SepHistoryItem>) {
    match sep_history::load(stems_dir) {
        Ok(sep_history::HistoryLoad::Missing) => ("还没有分离历史".to_string(), Vec::new()),
        Ok(sep_history::HistoryLoad::Loaded(mut entries)) => {
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at_ms));
            let items: Vec<SepHistoryItem> = entries
                .into_iter()
                .take(SEP_HISTORY_VISIBLE)
                .map(|entry| SepHistoryItem {
                    tracks: sep_history::resolve_tracks(stems_dir, &entry),
                    entry,
                })
                .collect();
            if items.is_empty() {
                ("还没有分离历史".to_string(), items)
            } else {
                (format!("最近 {} 条（时间倒序）", items.len()), items)
            }
        }
        Err(e) => {
            // 坏索引不能显示成“没有历史”：状态行保留损坏原因，列表为空。
            (format!("分离历史读不出来：{e}"), Vec::new())
        }
    }
}

/// 一条历史 → 界面行（纯投影；`index` 是它在快照里的下标，点击时原样回传）。
fn separation_history_row(i: usize, now: u64, item: &SepHistoryItem) -> SeparationHistoryRow {
    let entry = &item.entry;
    let detail = if item.tracks.available() {
        let media = match (entry.duration_secs, entry.sample_rate_hz) {
            (Some(duration), Some(rate)) => format!(" · {duration:.1}s / {rate}Hz"),
            (Some(duration), None) => format!(" · {duration:.1}s"),
            (None, Some(rate)) => format!(" · {rate}Hz"),
            (None, None) => String::new(),
        };
        format!(
            "人声 {} · 伴奏 {}{media}",
            entry.vocals_file, entry.accompaniment_file
        )
    } else {
        item.tracks.note.clone()
    };
    SeparationHistoryRow {
        index: i as i32,
        created: relative_time(now, entry.created_at_ms).into(),
        input: file_label(Path::new(&entry.input_path)).into(),
        detail: detail.into(),
        available: item.tracks.available(),
    }
}

/// 读取当前工程目录下的历史，只投影最近 5 条；每条的两轨在投影前已做安全解析。
fn refresh_separation_history(ui: &MainWindow, state: &Rc<UiState>) {
    let stems_dir = project_dir(&file_stem(&ui.get_project_name())).join("stems");
    let (status, items) = separation_history_view(&stems_dir);
    let now = now_ms();
    let ui_rows: Vec<SeparationHistoryRow> = items
        .iter()
        .enumerate()
        .map(|(i, item)| separation_history_row(i, now, item))
        .collect();

    *state.sep_history.borrow_mut() = items;
    ui.set_sep_history_rows(ModelRc::from(Rc::new(VecModel::from(ui_rows))));
    ui.set_sep_history_status(status.into());
}

/// 「当前结果」的两轨能不能试听/导出：必须**同一个工程目录**、都是工程内的普通文件。
///
/// 抽成纯函数（吃两个路径，不碰 `MainWindow`）是为了让"跨工程目录拒绝"这条有测试隔离
/// ——外层 `current_separation_tracks_usable` 需要 `MainWindow`，单测造不出来。
fn separation_tracks_check(vocals: &Path, accompaniment: &Path) -> Result<(), String> {
    let Some(stems_dir) = vocals.parent() else {
        return Err("分离结果路径不完整，不能试听/导出".to_string());
    };
    if accompaniment.parent() != Some(stems_dir) {
        return Err("两轨不在同一个工程目录，拒绝试听/导出".to_string());
    }
    let (Some(vocals_file), Some(accompaniment_file)) = (
        vocals.file_name().and_then(|name| name.to_str()),
        accompaniment.file_name().and_then(|name| name.to_str()),
    ) else {
        return Err("两轨文件名不是 UTF-8，不能试听/导出".to_string());
    };
    let resolution = sep_history::resolve_track_names(stems_dir, vocals_file, accompaniment_file);
    if resolution.available() {
        Ok(())
    } else {
        Err(resolution.note)
    }
}

/// 试听/导出前重新检查“当前结果”的两轨：文件可能已被删除，或缓存快照已过期。
fn current_separation_tracks_usable(ui: &MainWindow, state: &Rc<UiState>) -> bool {
    // 文件可能在应用运行期间被手动删除/替换；每次试听/导出前重新解析，
    // 不只相信列表初次加载时的快照。
    refresh_separation_history(ui, state);
    let Some((vocals, accompaniment)) = state.sep_tracks.borrow().clone() else {
        ui.set_sep_status_text("还没有分离结果".into());
        return false;
    };
    match separation_tracks_check(&vocals, &accompaniment) {
        Ok(()) => true,
        Err(note) => {
            ui.set_sep_result_available(false);
            ui.set_sep_status_text(note.into());
            false
        }
    }
}

/// 切换/粘贴待分离音频：输入变了就把旧两轨标为过期（导出与试听都要求 has-result）。
fn set_separation_input(ui: &MainWindow, state: &Rc<UiState>, path: String) {
    let same = state.sep_input.borrow().as_deref() == Some(path.as_str());
    ui.set_sep_input_path(path.clone().into());
    let label = file_label(Path::new(&path));
    ui.set_sep_input_summary(format!("待分离：{label}").into());
    if same {
        return;
    }
    *state.sep_input.borrow_mut() = Some(path);
    clear_separation_result(ui, state);
    ui.set_sep_status_text("已就绪，可分离".into());
}

/// 人声分离：选文件 / 分离 / 停止 / 两轨试听与导出。
/// 质检入口：配音页「高级」里的「质检」按钮 → 排队跑一遍 ASR 回读。
fn wire_quality_check(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
    eval_stop: &Arc<AtomicBool>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    let stop_flag = Arc::clone(eval_stop);
    ui.on_quality_check(move || {
        let Some(ui) = weak.upgrade() else { return };
        if !ui.get_has_result() {
            ui.set_status_text("还没有成品：先合成一轮再质检".into());
            return;
        }
        // 同一时刻只允许一条质检（结果只有一个槽位）；它不碰配音工程，可排在别的任务后面
        if st.eval_task.get().is_some() {
            ui.set_status_text("已有一条质检在队列里：等它跑完再点".into());
            return;
        }
        let stem = file_stem(&ui.get_project_name());
        let dir = project_dir(&stem);
        stop_flag.store(false, Ordering::Relaxed);
        // 这里就把生效模型定下来发给 worker：UI 回显、报告落盘、真实请求三处同一个值，
        // 不给"两处各算一份"留机会（运行中改设置也不会让它们漂移）。
        let model = effective_asr_model(&settings_snapshot());
        let id = enqueue_task(
            &ui,
            &st,
            &st.eval_task,
            tasks::TaskKind::Eval,
            "质检 · ASR 回读",
        );
        let note: String = match queue_note(&st, id) {
            Some(q) => format!("{q} · 轮到它时自动开始质检（回读 {model}）"),
            None => {
                format!("质检中：用 {model} 逐句 ASR 回读（首次会加载该模型，约 8s）…")
            }
        };
        ui.set_status_text(note.into());
        if tx
            .send(Cmd::RunEval {
                revision: st.project_revision.get(),
                task_id: id,
                dir,
                model,
            })
            .is_err()
        {
            let note = "工作线程不可用：质检未发出，请重启应用";
            finish_task(&ui, &st, &st.eval_task, tasks::TaskState::Failed, note);
            ui.set_status_text(note.into());
        }
    });
}

fn wire_separation(
    ui: &MainWindow,
    msg_tx: &Sender<WorkerMsg>,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
    player: &Rc<player::Player>,
    stop: &Arc<AtomicBool>,
) {
    // 选择音频（系统文件框，跑在后台线程）
    let weak = ui.as_weak();
    let msg = msg_tx.clone();
    ui.on_sep_pick_file(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_sep_busy() {
            return;
        }
        ui.set_status_text("正在打开系统文件选择框…".into());
        spawn_file_pick(msg.clone());
    });

    // 分离
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let st = state.clone();
    let stop1 = Arc::clone(stop);
    ui.on_sep_run(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 分离不依赖配音工程的可变状态（读自己的输入文件、写 stems/），所以运行中
        // 也能提交：排进同一条队列，轮到时 worker 会回报 TaskStarted。但本 Tab 只有
        // 一个进度槽位，同一条分离还没结束时不接受第二条。
        if st.sep_task.get().is_some() {
            ui.set_sep_status_text("已有一条分离在队列里：等它跑完或先停止".into());
            return;
        }
        let input = ui.get_sep_input_path().trim().to_string();
        if input.is_empty() {
            ui.set_sep_status_text("先选择一段音频".into());
            return;
        }
        if !Path::new(&input).is_file() {
            ui.set_sep_status_text(format!("音频不存在或不可读：{input}").into());
            return;
        }
        // 分块秒数：非数字/留空都当"用模型默认"，不让输入错误变成阻断
        let chunk = ui
            .get_sep_chunk_seconds()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|v| *v > 0);

        let stem = file_stem(&ui.get_project_name());
        let out_dir = project_dir(&stem).join("stems");
        stop1.store(false, Ordering::Relaxed);
        ui.set_sep_busy(true);
        clear_separation_result(&ui, &st);
        let id = enqueue_task(
            &ui,
            &st,
            &st.sep_task,
            tasks::TaskKind::Separation,
            format!("人声分离 · {stem}"),
        );
        let sep_note: String = match queue_note(&st, id) {
            Some(note) => format!("{note} · 轮到它时自动开始"),
            None => "正在加载模型并分离（首次会下载约 200MB 模型）…".to_string(),
        };
        ui.set_sep_status_text(sep_note.into());

        if tx
            .send(Cmd::RunSeparation {
                revision: 0,
                task_id: id,
                input: PathBuf::from(input),
                out_dir,
                stem,
                model_dir: Some(model_dir()),
                chunk_seconds: chunk,
            })
            .is_err()
        {
            ui.set_sep_busy(false);
            let note = "工作线程不可用：分离未发出，请重启应用";
            finish_task(&ui, &st, &st.sep_task, tasks::TaskState::Failed, note);
            ui.set_sep_status_text(note.into());
        }
    });

    // 高级里手输/粘贴路径：与"选文件"走同一套 stale 逻辑（否则旧两轨还能导出）
    let weak = ui.as_weak();
    let st_edit = state.clone();
    ui.on_sep_path_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_sep_busy() {
            return;
        }
        let path = ui.get_sep_input_path().trim().to_string();
        if path.is_empty() {
            return;
        }
        set_separation_input(&ui, &st_edit, path);
    });

    // 停止：排队中直接出队；在跑则走协作式停止位（上游没有中断 API，只"别再落盘"）
    let weak = ui.as_weak();
    let state2 = state.clone();
    let stop2 = Arc::clone(stop);
    ui.on_sep_stop(move || {
        let Some(ui) = weak.upgrade() else { return };
        stop_separation(&ui, &state2, &stop2);
    });

    // 两轨试听（0 = 人声，1 = 伴奏）
    let weak = ui.as_weak();
    let st2 = state.clone();
    let player2 = player.clone();
    ui.on_sep_preview_track(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        if !ui.get_sep_has_result() {
            ui.set_sep_status_text("还没有分离结果".into());
            return;
        }
        if !ui.get_sep_result_available() {
            ui.set_sep_status_text("这条分离结果的两轨文件不在了，不能试听".into());
            return;
        }
        if !current_separation_tracks_usable(&ui, &st2) {
            return;
        }
        let Some((vocals, accompaniment)) = st2.sep_tracks.borrow().clone() else {
            ui.set_sep_status_text("还没有分离结果".into());
            return;
        };
        let (path, what) = if i == 0 {
            (vocals, "人声")
        } else {
            (accompaniment, "伴奏")
        };
        match player2.play_wav(&path) {
            Ok(()) => {
                ui.set_playing(true);
                ui.set_status_text(format!("试听{what}轨：{}", file_label(&path)).into());
            }
            Err(e) => ui.set_sep_status_text(format!("试听失败：{e}").into()),
        }
    });

    // 两轨导出（复制到导出目录）
    let weak = ui.as_weak();
    let st3 = state.clone();
    ui.on_sep_export_track(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        if !ui.get_sep_has_result() {
            ui.set_sep_status_text("还没有分离结果".into());
            return;
        }
        if !ui.get_sep_result_available() {
            ui.set_sep_status_text("这条分离结果的两轨文件不在了，不能导出".into());
            return;
        }
        if !current_separation_tracks_usable(&ui, &st3) {
            return;
        }
        let Some((vocals, accompaniment)) = st3.sep_tracks.borrow().clone() else {
            ui.set_sep_status_text("还没有分离结果".into());
            return;
        };
        let (src, suffix) = if i == 0 {
            (vocals, "vocals")
        } else {
            (accompaniment, "accompaniment")
        };
        let dir = PathBuf::from(ui.get_export_dir().to_string());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            ui.set_sep_status_text(aw_core::dub::write_failure_note(&dir, 0, &e).into());
            return;
        }
        let dst = dir.join(format!(
            "{}_{suffix}.wav",
            file_stem(&ui.get_project_name())
        ));
        match aw_core::dub::copy_atomic(&src, &dst) {
            Ok(_) => {
                ui.set_sep_status_text(format!("已导出：{}", dst.display()).into());
                toast(&ui, &format!("已导出 {}", file_label(&dst)));
            }
            Err(e) => ui.set_sep_status_text(aw_core::dub::write_failure_note(&dst, 0, &e).into()),
        }
    });

    // 历史回看：只使用已解析成功的安全路径；缺失/软链条目仍可点击看到原因。
    let weak = ui.as_weak();
    let st_history = state.clone();
    ui.on_sep_history_open(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        // 用户可能在应用运行期间删掉音频；点击时先重读索引并重新解析，
        // 让“文件不在了”的提示和按钮禁用状态来自当前磁盘，不来自旧快照。
        refresh_separation_history(&ui, &st_history);
        let Some(item) = st_history.sep_history.borrow().get(index as usize).cloned() else {
            ui.set_sep_status_text("历史条目已刷新，请重新点击".into());
            return;
        };
        let view = sep_history_view_for(&item);
        ui.set_sep_input_path(view.input_path.clone().into());
        ui.set_sep_input_summary(view.input_summary.into());
        *st_history.sep_input.borrow_mut() = Some(view.input_path);
        ui.set_sep_has_result(view.has_result);
        ui.set_sep_progress(view.progress);
        ui.set_sep_result_available(view.result_available);
        ui.set_sep_vocals_label(view.vocals_label.into());
        ui.set_sep_accompaniment_label(view.accompaniment_label.into());
        // 不可用时必须是 None：不能把上一条历史的两轨留在槽位里（否则试听会放错音频）
        *st_history.sep_tracks.borrow_mut() = view.tracks;
        ui.set_sep_status_text(view.sep_status.into());
        ui.set_status_text(view.main_status.into());
    });

    // 启动/接线时先读一次当前工程的历史；换工程名时也会刷新。
    refresh_separation_history(ui, state);
}

fn file_stem(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "未命名工程".to_string();
    }
    let sanitized: String = trimmed
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                ch
            }
        })
        .collect();
    if sanitized == "." || sanitized == ".." {
        "未命名工程".to_string()
    } else {
        sanitized
    }
}

fn toast(ui: &MainWindow, text: &str) {
    ui.set_toast_text(text.into());
    ui.set_toast_shown(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_capabilities::{Capability, Requires};

    /// 目录扫描：深度 ≤2（覆盖 `models/<模型名>/*.gguf`），深度 3 的文件不算；
    /// 目录不存在不能假装扫到东西。
    #[test]
    fn scan_model_dir_honours_depth_and_missing_dir() {
        let dir = temp_dir("scan");
        // 直接放一个
        std::fs::write(dir.join("a.gguf"), b"x").unwrap();
        // 深度 1：models/<模型名>/x.gguf
        std::fs::create_dir_all(dir.join("m1")).unwrap();
        std::fs::write(dir.join("m1/x.gguf"), b"x").unwrap();
        // 深度 2：models/a/b/z.gguf
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/z.gguf"), b"x").unwrap();
        // 深度 3：不算
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::write(dir.join("a/b/c/too-deep.gguf"), b"x").unwrap();
        // 非 gguf 不算
        std::fs::write(dir.join("note.txt"), b"x").unwrap();

        let (exists, n) = scan_model_dir(&dir);
        assert!(exists);
        assert_eq!(
            n, 3,
            "应数到 a.gguf + m1/x.gguf + a/b/z.gguf，深度 3 与非 .gguf 不算"
        );

        let missing = dir.join("nope");
        assert_eq!(scan_model_dir(&missing), (false, 0));
    }

    // ---------- 默认模型目录：跟着 server.json 的模型根走 ----------

    /// 测试里拼一个**本平台意义上的绝对路径**。
    /// Windows 上裸 `/x` 不算绝对（没有盘符），会走"相对路径必须回落"那条，
    /// 断言就变成在测另一件事——这里统一拼成各平台的绝对形状。
    fn abs_path(rest: &str) -> String {
        let sep = if cfg!(windows) { '\\' } else { '/' };
        let body: String = rest
            .trim_start_matches('/')
            .chars()
            .map(|c| if c == '/' { sep } else { c })
            .collect();
        if cfg!(windows) {
            format!("C:\\{body}")
        } else {
            format!("/{body}")
        }
    }

    /// 一批 path 拼成本仓 `server.json` 那种清单（只关心 path 的模型根推导）。
    fn cfg_with_paths(paths: &[&str]) -> ServerConfig {
        ServerConfig {
            host: None,
            port: None,
            min_free_memory_mb: None,
            models: paths
                .iter()
                .enumerate()
                .map(|(i, path)| ServerModel {
                    id: format!("m{i}"),
                    task: "tts".into(),
                    family: "f".into(),
                    path: (*path).to_string(),
                    // 本批只关心 path；新增字段（known_issues/mode/product_excluded/requires/role）
                    // 走默认，免得每加一个字段都要回来补这里。
                    ..Default::default()
                })
                .collect(),
        }
    }

    /// ① 多条 path（文件 + 目录混着，真机清单就是这种形状）→ 收敛到它们的公共父目录。
    #[test]
    fn model_root_is_the_common_parent_of_declared_paths() {
        let a = abs_path("models/ModelA/a.gguf");
        let b = abs_path("models/ModelB/b.gguf");
        let dir_model = abs_path("models/ModelC"); // gen 类：path 指目录
        let same = |_: &Path| Some("root".to_string());
        assert_eq!(
            model_root_with(&[a.as_str(), b.as_str(), dir_model.as_str()], &same),
            Some(PathBuf::from(abs_path("models")))
        );
        // 顺序无关
        assert_eq!(
            model_root_with(&[dir_model.as_str(), a.as_str(), b.as_str()], &same),
            Some(PathBuf::from(abs_path("models")))
        );
        // 与真机同一形状：`/Volumes/DataExt/models/<模型目录>/<文件>`
        let real = abs_path("Volumes/DataExt/models/Qwen3-ASR-0.6B-GGUF/qwen3-asr.gguf");
        let real2 = abs_path("Volumes/DataExt/models/Yue2-3B-GGUF");
        assert_eq!(
            model_root_with(&[real.as_str(), real2.as_str()], &same),
            Some(PathBuf::from(abs_path("Volumes/DataExt/models")))
        );

        // 祖先要按**路径组件**找，不是字符串前缀：`/models-2` 不是 `/models` 里的东西。
        // 字面前缀比较会把这两条判成"公共根 = /models"，而那根本不是 `/models-2/y.gguf` 的祖先。
        let sibling_a = abs_path("models/in/x.gguf");
        let sibling_b = abs_path("models-2/y.gguf");
        assert_eq!(
            model_root_with(&[sibling_a.as_str(), sibling_b.as_str()], &same),
            None,
            "`/models-2` 与 `/models` 没有公共模型根，不能被字符串前缀凑成一个"
        );
    }

    /// ① 单条 path：它的**父目录**就是模型根（文件与目录两种都取 `parent()`）。
    #[test]
    fn model_root_of_a_single_path_is_its_parent() {
        let same = |_: &Path| Some("root".to_string());
        let file = abs_path("models/ModelA/a.gguf");
        assert_eq!(
            model_root_with(&[file.as_str()], &same),
            Some(PathBuf::from(abs_path("models/ModelA")))
        );
        let dir_model = abs_path("models/DirModel");
        assert_eq!(
            model_root_with(&[dir_model.as_str()], &same),
            Some(PathBuf::from(abs_path("models")))
        );
    }

    /// ① 空：清单缺失 / 字段全空 → 推不出来 → 回落旧默认值。
    #[test]
    fn model_root_is_none_when_there_is_no_path() {
        let same = |_: &Path| Some("root".to_string());
        assert_eq!(model_root_with(&[], &same), None);
        assert_eq!(model_root_with(&["", "   "], &same), None);
        assert_eq!(model_root_from_paths(&[]), None);
        assert_eq!(default_model_dir_from(None), fallback_model_dir());
        assert_eq!(
            default_model_dir_from(Some(&cfg_with_paths(&[]))),
            fallback_model_dir()
        );
    }

    /// ① 相对路径：服务按**它自己的** cwd 解析，我们猜不到 → 整批回落，
    /// 不许"只用绝对的那几条"拼一个看起来能用的根出来。
    #[test]
    fn model_root_is_none_when_any_path_is_relative() {
        let same = |_: &Path| Some("root".to_string());
        let rel = "models/ModelA/a.gguf";
        assert_eq!(model_root_with(&[rel], &same), None);

        let abs = abs_path("models/ModelA/a.gguf");
        assert_eq!(
            model_root_with(&[abs.as_str(), rel], &same),
            None,
            "有一条相对路径就不能给出「公共根」"
        );
        assert_eq!(model_root_from_paths(&[abs.as_str(), rel]), None);
    }

    /// ① 跨根 → 回落，不许瞎猜一个根。两类各测一次，且都能单独变红：
    /// · 两条 path 唯一的公共祖先是**文件系统根**（Unix `/`、Windows `C:\`）→ 不算公共父目录；
    /// · 两条 path 落在**不同卷/盘**上 → 硬算出来的是 `/mnt` 这种"挂载点容器"，不是模型根。
    ///   （后一类注入假 volume：一台机器上造不出第二个文件系统。）
    #[test]
    fn model_root_is_none_across_roots() {
        let same = |_: &Path| Some("same".to_string());
        // 唯一的公共祖先是文件系统根 → 回落（删掉 `root.parent()?` 这道守卫这条会红）
        let a = abs_path("volA/models/x.gguf");
        let b = abs_path("volB/models/y.gguf");
        assert_eq!(model_root_with(&[a.as_str(), b.as_str()], &same), None);
        assert_eq!(model_root_from_paths(&[a.as_str(), b.as_str()]), None);

        // 两条 path 有公共祖先（`/mnt`、`C:\mnt`），但分属两个卷 → 也回落
        let ma = abs_path("mnt/volA/models/x.gguf");
        let mb = abs_path("mnt/volB/models/y.gguf");
        assert_eq!(
            model_root_with(&[ma.as_str(), mb.as_str()], &same),
            Some(PathBuf::from(abs_path("mnt"))),
            "同一个卷时公共父目录必须推得出来（否则下面那条断言就是在骗自己）"
        );
        let two_volumes = |p: &Path| {
            let s = p.to_string_lossy();
            Some(if s.contains("volA") { "A" } else { "B" }.to_string())
        };
        assert_eq!(
            model_root_with(&[ma.as_str(), mb.as_str()], &two_volumes),
            None,
            "跨卷不许拿挂载点容器当模型根"
        );
    }

    /// ② 用户显式设置过 `model_dir` 时**一定**优先：推出来的默认值不许盖掉它。
    #[test]
    fn explicit_model_dir_beats_the_manifest_derived_root() {
        let a = abs_path("models/ModelA/a.gguf");
        let b = abs_path("models/ModelB/b.gguf");
        let cfg = cfg_with_paths(&[a.as_str(), b.as_str()]);
        let picked = AppSettings {
            model_dir: Some(abs_path("my/own/models")),
            ..Default::default()
        };

        assert_eq!(
            effective_model_dir(&picked, Some(&cfg)),
            PathBuf::from(abs_path("my/own/models"))
        );
        assert_eq!(
            effective_model_dir(&AppSettings::default(), Some(&cfg)),
            PathBuf::from(abs_path("models")),
            "没设置过才用推导出来的模型根"
        );
        assert_eq!(
            effective_model_dir(&picked, None),
            PathBuf::from(abs_path("my/own/models")),
            "清单读不出来时显式设置照样优先"
        );
        // 清单推不出来（跨根）时也是显式设置优先、否则回落旧默认值
        let cross = cfg_with_paths(&[
            abs_path("volA/models/x.gguf").as_str(),
            abs_path("volB/models/y.gguf").as_str(),
        ]);
        assert_eq!(
            effective_model_dir(&AppSettings::default(), Some(&cross)),
            fallback_model_dir()
        );
        assert_eq!(
            effective_model_dir(&picked, Some(&cross)),
            PathBuf::from(abs_path("my/own/models"))
        );
    }

    /// 「清单里有多少模型落在该目录下」是**路径组件前缀**判定：
    /// `/models-2/x.gguf` 不能被算进 `/models`（字符串前缀会误算）。
    #[test]
    fn models_under_dir_uses_path_prefix_not_string_prefix() {
        let model = |id: &str, path: &str| ServerModel {
            id: id.into(),
            task: "tts".into(),
            family: "f".into(),
            path: path.into(),
            ..Default::default()
        };
        let cfg = Some(ServerConfig {
            host: None,
            port: None,
            min_free_memory_mb: None,
            models: vec![
                model("in", "/models/in/x.gguf"),
                model("sibling", "/models-2/s/x.gguf"),
                model("out", "/elsewhere/out/y.gguf"),
            ],
        });
        assert_eq!(
            models_under_dir(&cfg, Path::new("/models")),
            1,
            "/models-2 是同级目录，不能被字符串前缀误算进来"
        );
        assert_eq!(models_under_dir(&cfg, Path::new("/elsewhere")), 1);
        assert_eq!(models_under_dir(&None, Path::new("/models")), 0);
    }

    // ── 模型暴露与能力契约（批次 cc-ai-audio-workshop-model-exposure）─────────
    // 每条都写了阳性对照：故意改坏哪一处会让它红（改完已复原）。

    /// 造一条清单模型（只填本批关心的字段，其余取默认）。
    fn sm(id: &str, task: &str) -> ServerModel {
        ServerModel {
            id: id.into(),
            task: task.into(),
            ..Default::default()
        }
    }

    /// 造一条清单模型并直接给它一组**生效能力**（其余取默认）。
    fn sm_with_caps(id: &str, task: &str, caps: Capability) -> ServerModel {
        ServerModel {
            caps,
            ..sm(id, task)
        }
    }

    /// 把清单模型转成"下拉行"（要求它是可选的 tts 引擎）。
    fn voice_of(m: &ServerModel) -> Voice {
        tts_engine_voices(std::slice::from_ref(m))
            .into_iter()
            .next()
            .expect("用例前提：这条模型应当是可选的 tts 引擎")
    }

    /// 验收①：配音引擎下拉**不再**出现 `product_excluded` 与 streaming-only 的模型。
    ///
    /// 真机反例：`audio8-tts-01b` 选中后 HTTP 200 却产出听不懂的音频（可懂度 0~3%），
    /// 全程无报错 —— 最坏的失败形态。
    ///
    /// 阳性对照：去掉 `is_selectable_tts_engine` 里的 `!m.product_excluded`
    /// 或 `!m.is_streaming_only()` → 被排除/流式的 id 立刻回到列表，本用例红。
    #[test]
    fn tts_engine_list_drops_product_excluded_and_streaming_only() {
        let models = vec![
            sm("audio8-tts", "tts"),
            sm_with_caps(
                "audio8-tts-01b",
                "tts",
                Capability {
                    product_excluded: true,
                    ..Default::default()
                },
            ),
            sm_with_caps(
                "audio8-tts-stream",
                "tts",
                Capability {
                    mode: "streaming".into(),
                    ..Default::default()
                },
            ),
            sm_with_caps(
                "audio8-tts-01b-stream",
                "tts",
                Capability {
                    mode: "streaming".into(),
                    role: "streaming".into(),
                    product_excluded: true,
                    ..Default::default()
                },
            ),
            sm("index-tts2", "tts"),
            sm("yue2", "gen"), // 不是 tts：本来就不该出现在配音下拉
            sm("qwen3-asr", "asr"),
        ];
        let names: Vec<String> = tts_engine_voices(&models)
            .iter()
            .map(|v| v.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec!["audio8-tts", "index-tts2"],
            "只留没被排除、且是离线的 tts 模型"
        );
        // role 单独承载流式时也要认（schema 里 audio8-tts-01b-stream 两种都写了）
        let role_only = vec![sm_with_caps(
            "stream-only",
            "tts",
            Capability {
                role: "streaming".into(),
                ..Default::default()
            },
        )];
        assert!(
            tts_engine_voices(&role_only).is_empty(),
            "role=streaming 也算流式专用"
        );
    }

    /// 验收②：引擎硬要求参考音（index-tts2）时不能只用内置音色 —— 判据说清原因。
    ///
    /// 阳性对照：删掉 `voice_readiness` 里 `engine.requires_voice_ref` 那一段
    /// → 第一条断言变 `Ready`，本用例红。
    #[test]
    fn engine_requiring_reference_cannot_start_on_builtin_voice() {
        let needs_ref = sm_with_caps(
            "index-tts2",
            "tts",
            Capability {
                // index-tts2：必须参考音、**不要求**参考文本（真机：只给 voice_ref 就 200）
                requires: Some(Requires {
                    voice_ref: true,
                    reference_text: false,
                }),
                ..Default::default()
            },
        );
        let v = voice_of(&needs_ref);
        assert!(v.requires_voice_ref, "能力要跟着行走到判据里");
        assert!(!v.requires_reference_text, "index-tts2 不要求参考文本");

        assert_eq!(
            voice_readiness(Some(&v), "", false, ""),
            VoiceReadiness::EngineNeedsReference
        );
        assert!(
            VoiceReadiness::EngineNeedsReference
                .note("index-tts2")
                .contains("必须提供参考音频"),
            "阻断提示要说清是这个引擎的硬要求"
        );
        // 填了路径但文件不在 → 报"路径不对"，不冒充"引擎需要参考音"
        assert_eq!(
            voice_readiness(Some(&v), "/tmp/ref.wav", false, "念的内容"),
            VoiceReadiness::ReferenceMissing
        );
        // 参考音存在但没文本：index-tts2 **不要求**文本 → 放行（改前全局硬要求拦错）
        assert_eq!(
            voice_readiness(Some(&v), "/tmp/ref.wav", true, ""),
            VoiceReadiness::Ready
        );
        // 路径 + 文本都给全 → 照样开工
        assert_eq!(
            voice_readiness(Some(&v), "/tmp/ref.wav", true, "念的内容"),
            VoiceReadiness::Ready
        );

        // 反例：audio8-tts 的克隆路径要求参考文本（只给 voice_ref 就 500，真机实测）；
        // 内置音色不给 voice_ref，不拦。
        let needs_text = voice_of(&sm_with_caps(
            "audio8-tts",
            "tts",
            Capability {
                requires: Some(Requires {
                    voice_ref: false,
                    reference_text: true,
                }),
                ..Default::default()
            },
        ));
        assert!(!needs_text.requires_voice_ref);
        assert!(needs_text.requires_reference_text, "能力要跟着行走到判据里");
        assert_eq!(
            voice_readiness(Some(&needs_text), "", false, ""),
            VoiceReadiness::Ready,
            "audio8-tts 的内置音色不需要参考音/文本"
        );
        assert_eq!(
            voice_readiness(Some(&needs_text), "/tmp/ref.wav", true, ""),
            VoiceReadiness::ReferenceTextMissing,
            "audio8-tts 给了参考音却缺文本 → 发请求前拦（不出现逐句 500）"
        );
        // 提示语与提交处真正拦住用户的那一句**同源**（同一个函数，不另写一份）
        assert_eq!(
            VoiceReadiness::ReferenceTextMissing.note("audio8-tts"),
            reference_text_required_note("audio8-tts"),
            "界面提示与提交拦截必须逐字一致"
        );
        assert!(
            VoiceReadiness::ReferenceTextMissing
                .note("audio8-tts")
                .contains("自动转写"),
            "提示要点名引擎并给「自动转写」这条出路"
        );
        assert_eq!(
            voice_readiness(Some(&needs_text), "/tmp/ref.wav", true, "念的内容"),
            VoiceReadiness::Ready
        );

        // 反例：不吃参考音/文本的引擎，空参考音就该能开工（过滤器不能顺手把正常引擎也禁了）
        let plain = voice_of(&sm("plain-tts", "tts"));
        assert!(!plain.requires_voice_ref);
        assert!(!plain.requires_reference_text);
        assert_eq!(
            voice_readiness(Some(&plain), "", false, ""),
            VoiceReadiness::Ready
        );
        // "给了参考音却忘了文本"对不要求文本的引擎**不拦**（按引擎，不是全局）
        assert_eq!(
            voice_readiness(Some(&plain), "/tmp/ref.wav", true, ""),
            VoiceReadiness::Ready
        );
        // 一个引擎都没有
        assert_eq!(
            voice_readiness(None, "", false, ""),
            VoiceReadiness::NoEngine
        );
    }

    /// 验收②（续）：引擎说明里要带上清单登记的 `known_issues`，且硬要求要点名。
    #[test]
    fn engine_note_surfaces_requires_and_known_issues() {
        let m = sm_with_caps(
            "index-tts2",
            "tts",
            Capability {
                requires: Some(Requires {
                    voice_ref: true,
                    reference_text: false,
                }),
                known_issues: vec!["不接 voice_ref 会直接报错".into()],
                ..Default::default()
            },
        );
        let note = engine_note(Some(&voice_of(&m)));
        assert!(note.contains("必须提供参考音频"), "{note}");
        assert!(note.contains("不接 voice_ref 会直接报错"), "{note}");
        // 需要参考文本的引擎要点名（audio8-tts 克隆路径只给 voice_ref 就 500）
        let needs_text = sm_with_caps(
            "audio8-tts",
            "tts",
            Capability {
                requires: Some(Requires {
                    voice_ref: false,
                    reference_text: true,
                }),
                ..Default::default()
            },
        );
        let note = engine_note(Some(&voice_of(&needs_text)));
        assert!(note.contains("参考文本必填"), "{note}");
        // 干净引擎没有说明行（不能给所有引擎加噪声）
        assert_eq!(engine_note(Some(&voice_of(&sm("plain-tts", "tts")))), "");
        assert_eq!(engine_note(None), "");
    }

    /// 验收③：默认引擎**按能力**选，不写死 `audio8-tts`。
    ///
    /// 阳性对照：把 `default_engine_index` 改回"找名字等于 audio8-tts"
    /// （或返回常量 -1）→ 第一条断言红。
    #[test]
    fn default_engine_follows_capability_not_a_hardcoded_id() {
        // 清单里没有 audio8-tts（只有 index-tts2）→ 仍要选得到一个可用 TTS
        let needs_ref = || {
            sm_with_caps(
                "index-tts2",
                "tts",
                Capability {
                    requires: Some(Requires {
                        voice_ref: true,
                        reference_text: false,
                    }),
                    ..Default::default()
                },
            )
        };
        let only_index = vec![voice_of(&needs_ref())];
        assert_eq!(
            default_engine_index(&only_index),
            0,
            "清单里没有 audio8-tts 时也要能选到可用 TTS（改前会显示「没有可用音色」）"
        );

        // 都在时优先"免参考音"的那个：新用户第一次进来就能直接合成
        let both = vec![voice_of(&needs_ref()), voice_of(&sm("audio8-tts", "tts"))];
        assert_eq!(
            default_engine_index(&both),
            1,
            "优先开箱可用的引擎（不需要参考音）"
        );

        // 全都要参考音时退回第一个，而不是 -1
        let all_need_ref = vec![voice_of(&needs_ref())];
        assert_eq!(default_engine_index(&all_need_ref), 0);
        assert_eq!(default_engine_index(&[]), -1, "一个引擎都没有才是 -1");
    }

    /// 验收④：清单里出现未知 gen 引擎时**显式报错**，不静默当 yue2 跑。
    ///
    /// 阳性对照：把 `song_model_for_id` 的 `other => Err(..)` 改回
    /// `_ => Ok(SongModel::Yue2)` → 第三条断言红。
    #[test]
    fn unknown_song_engine_is_an_explicit_error_not_a_silent_yue2() {
        assert_eq!(song_model_for_id("yue2"), Ok(SongModel::Yue2));
        assert_eq!(song_model_for_id("ace-step"), Ok(SongModel::AceStep));
        let err = song_model_for_id("experimental-gen").expect_err("未知引擎不能有实现");
        assert!(
            err.contains("experimental-gen"),
            "错误要点名是哪个引擎：{err}"
        );
        assert!(err.contains("还没有接入"), "{err}");
    }

    /// 验收④（续）：歌曲引擎选项来自清单（含展示名），清单为空才退回内置清单。
    #[test]
    fn song_engine_options_come_from_the_manifest() {
        let models = vec![
            sm("ace-step", "gen"),
            sm("yue2", "gen"),
            sm("experimental-gen", "gen"),
            sm("audio8-tts", "tts"), // 不是 gen
            sm_with_caps(
                "excluded-gen",
                "gen",
                Capability {
                    product_excluded: true,
                    ..Default::default()
                },
            ),
            sm_with_caps(
                "stream-gen",
                "gen",
                Capability {
                    mode: "streaming".into(),
                    ..Default::default()
                },
            ),
        ];
        let opts = song_engine_options(&models);
        assert_eq!(
            opts.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            vec!["ace-step", "yue2", "experimental-gen"],
            "顺序跟清单走；被排除/流式的 gen 不进列表"
        );
        assert_eq!(
            opts[0].label, "ACE-Step（文生音乐）",
            "内置引擎用友好展示名"
        );
        assert_eq!(
            opts[2].label, "experimental-gen",
            "表里没有的用 id 当展示名"
        );

        // 清单里一个 gen 都没有 → 退回内置清单。**不能返回空列表**：
        // Slint 的 PixelSegmentedControl 宽度按 options.length 做除数。
        let fallback = song_engine_options(&[]);
        assert_eq!(
            fallback.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            vec!["yue2", "ace-step"]
        );
        // 兜底清单本身也要能过 id → 实现（不然 UI 选得到、提交时才发现不支持）
        for o in &fallback {
            assert!(
                song_model_for_id(&o.id).is_ok(),
                "兜底项 {} 必须真支持",
                o.id
            );
        }
    }

    /// **清单键名契约**：`server.json` 的键名 → `ServerModel` 的 serde 字段名必须一一对上。
    ///
    /// 单独立一条的理由：Python 侧（`tools/audio_config.py::to_server`）与这里的字段名
    /// 一旦漂移（`requires` 改名、`requires.voice_ref` 换键、`known_issues` 拼错），
    /// serde 会**静默**取默认值 —— 于是"被排除的模型"照旧出现在下拉里、
    /// "引擎硬要求参考音"这条规则悄悄消失。那正是本批要修的最坏形态
    /// （选了不报错、产出听不懂的音频），而**纯函数用例永远发现不了**：
    /// 它们喂的是 Rust 结构体，不经过 JSON。
    ///
    /// 阳性对照：把 `ServerModel` 的 `requires` 改成 `#[serde(rename = "requiresX")]`
    /// （或把 `ModelRequires::voice_ref` 改名）→ 本用例红。
    #[test]
    fn server_json_keys_match_the_serde_field_names() {
        // **故意用不在随包清单里的 id**：用真 id 的话，键名漂移会退化成"服务端没写"
        // → 落回随包清单 → 断言照样绿，这条用例就没牙了（本批的兜底正会把漂移盖住）。
        // 形状与真实 server.json 一致（键名逐字取自 schema 的渲染落点）。
        let raw = r#"{
            "host": "127.0.0.1",
            "port": 8080,
            "models": [
                {
                    "id": "srv-excluded",
                    "task": "tts",
                    "family": "audio8_tts",
                    "path": "/models/Audio8-TTS-Preview-0.1B-GGUF/a.gguf",
                    "mode": "offline",
                    "product_excluded": true
                },
                {
                    "id": "srv-needs-ref",
                    "task": "tts",
                    "family": "index_tts2",
                    "path": "/models/IndexTTS2.5-GGUF/i.gguf",
                    "mode": "offline",
                    "requires": { "voice_ref": true },
                    "known_issues": ["不接 voice_ref 会直接报错"]
                },
                {
                    "id": "srv-stream",
                    "task": "tts",
                    "family": "audio8_tts",
                    "path": "/models/Audio8-TTS-Preview-0.1B-GGUF/a.gguf",
                    "mode": "streaming",
                    "role": "streaming",
                    "product_excluded": true
                },
                {
                    "id": "srv-scoring",
                    "task": "asr",
                    "family": "qwen3_asr",
                    "path": "/models/Qwen3-ASR-0.6B-GGUF/q.gguf",
                    "mode": "offline",
                    "role": "scoring"
                },
                {
                    "id": "srv-plain",
                    "task": "tts",
                    "family": "audio8_tts",
                    "path": "/models/plain/a.gguf"
                }
            ]
        }"#;

        let cfg = parse_server_config(raw).expect("最小清单要能解析");
        assert_eq!(cfg.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(cfg.port, Some(8080));
        assert_eq!(cfg.models.len(), 5);

        assert!(
            cfg.models[0].caps.product_excluded,
            "product_excluded 必须解析到（漂移就会静默变成 false=照旧暴露）"
        );
        assert_eq!(
            cfg.models[0].path, "/models/Audio8-TTS-Preview-0.1B-GGUF/a.gguf",
            "path 也必须解析到（漂移会让模型目录推导与落点校验全错）"
        );
        assert!(
            cfg.models[1].caps.requires_voice_ref(),
            "requires.voice_ref 必须解析到（漂移就会静默变成 false=不拦）"
        );
        assert_eq!(
            cfg.models[1].caps.known_issues_note(),
            "不接 voice_ref 会直接报错",
            "known_issues 必须解析到（漂移就会静默变成空）"
        );
        assert!(
            cfg.models[2].caps.is_streaming_only(),
            "mode / role = streaming 必须解析到"
        );
        assert_eq!(cfg.models[3].caps.role, "scoring", "role 必须解析到");
        assert_eq!(cfg.models[4].url, "", "没写 url 就是空串，不是解析失败");
        assert!(
            cfg.models[4].caps.known_issues.is_empty()
                && !cfg.models[4].caps.product_excluded
                && cfg.models[4].caps.mode.is_empty(),
            "随包清单里没有这个 id → 没写的字段就是未声明默认，不编造"
        );

        // 端到端：判据作用在"从 JSON 来的"清单上，结果要和 Rust 结构体的一致
        let names: Vec<String> = tts_engine_voices(&cfg.models)
            .iter()
            .map(|v| v.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec!["srv-needs-ref", "srv-plain"],
            "JSON 来的清单走同一条过滤：被排除的与流式专用的不进下拉"
        );
        assert!(
            tts_engine_voices(&cfg.models)[0].requires_voice_ref,
            "能力也要穿过 JSON 到达下拉行"
        );
    }

    // ── 随包能力清单兜底（批次 cc-ai-audio-workshop-capability-fallback）─────
    //
    // 每条都写了阳性对照：故意改坏哪一处会让它红。

    /// 验收①：**不改 server.json** 也能拿到能力提示 —— 服务端没写的字段回落随包清单。
    ///
    /// 本机真机的形状就是这条：server.json 的模型里 `requires` / `role` / `known_issues`
    /// 一个都没有（只有 `mode`，两个 0.1b 有 `product_excluded`），所以改前界面
    /// "不误禁、但也不提示"。
    ///
    /// 阳性对照：把 `parse_server_config` 里的 `model_capabilities::resolve` 换成
    /// 直接抄原始值（或删掉随包清单回落）→ `requires_voice_ref` 掉成 false，本用例红。
    #[test]
    fn server_omitting_capabilities_still_gets_them_from_the_bundled_catalog() {
        let raw = r#"{"models":[
            {"id":"index-tts2","task":"tts","family":"index_tts2","path":"/m/i.gguf","mode":"offline"},
            {"id":"audio8-tts","task":"tts","family":"audio8_tts","path":"/m/a.gguf","mode":"offline"}
        ]}"#;
        let cfg = parse_server_config(raw).expect("清单要能解析");

        let index = &cfg.models[0];
        assert!(
            index.caps.requires_voice_ref(),
            "服务端没写 requires → 必须回落随包清单（这是本批的核心价值）"
        );
        assert!(
            !index.caps.known_issues.is_empty(),
            "known_issues 也要回落，否则提示上不了屏"
        );
        assert_eq!(index.caps.mode, "offline", "服务端写了 mode → 用服务端的");

        // 端到端：这条能力真的走到了"能不能开工"的判据上
        let v = voice_of(index);
        assert!(v.requires_voice_ref, "能力要穿过清单到达下拉行");
        assert_eq!(
            voice_readiness(Some(&v), "", false, ""),
            VoiceReadiness::EngineNeedsReference,
            "不填参考音就该被拦（改前这里会放行）"
        );
        let note = engine_note(Some(&v));
        assert!(note.contains("必须提供参考音频"), "{note}");
        assert!(
            note.contains("不接 voice_ref 会直接报错"),
            "随包的 known_issues 要上屏：{note}"
        );

        // 不吃参考音的引擎不能被顺带禁掉；它的克隆路径**要求参考文本**（audio8-tts）
        let audio8 = &cfg.models[1];
        assert!(!audio8.caps.requires_voice_ref());
        assert!(
            audio8.caps.requires_reference_text(),
            "服务端没写 requires → audio8-tts 的 requires.reference_text 也要回落随包清单"
        );
        assert_eq!(
            voice_readiness(Some(&voice_of(audio8)), "", false, ""),
            VoiceReadiness::Ready,
            "audio8-tts 内置音色（没给 voice_ref）不需要参考文本"
        );
        assert_eq!(
            voice_readiness(Some(&voice_of(audio8)), "/tmp/ref.wav", true, ""),
            VoiceReadiness::ReferenceTextMissing,
            "audio8-tts 克隆缺文本要在发请求前拦（随包清单的 requires.reference_text 生效）"
        );
        // index-tts2 不要求参考文本：给了参考音 + 无文本也放行
        assert!(
            !index.caps.requires_reference_text(),
            "index-tts2 显式 reference_text=false 要回落（真机：只给 voice_ref 就 200）"
        );
        assert_eq!(
            voice_readiness(Some(&v), "/tmp/ref.wav", true, ""),
            VoiceReadiness::Ready,
            "index-tts2 缺文本不拦（改前全局硬要求会在这里冤枉用户）"
        );
    }

    /// 验收②：服务端**显式**给了就以服务端为准，而且是**逐字段**（不是整条二选一）。
    ///
    /// 阳性对照：把 `resolve` 改成"服务端任一能力字段存在就整条用服务端"
    /// → 第二条断言（没补的 requires 仍回落）红。
    #[test]
    fn server_explicit_capabilities_win_field_by_field() {
        let raw = r#"{"models":[
            {"id":"index-tts2","task":"tts","path":"/m/i.gguf",
             "mode":"offline","role":"fast-asr","requires":{"voice_ref":false},"known_issues":[]}
        ]}"#;
        let cfg = parse_server_config(raw).expect("清单要能解析");
        let caps = &cfg.models[0].caps;
        assert!(
            !caps.requires_voice_ref(),
            "服务端显式 voice_ref=false 必须压过随包清单的 true"
        );
        assert!(
            caps.known_issues.is_empty(),
            "服务端显式空数组必须压过随包清单的非空（否则用户删不掉随包登记的提示）"
        );
        assert_eq!(caps.role, "fast-asr", "服务端显式 role 优先");
        assert_eq!(caps.mode, "offline");
        assert!(!caps.product_excluded);

        // 逐字段：服务端只补了 mode，其余仍要拿到兜底
        let raw = r#"{"models":[
            {"id":"index-tts2","task":"tts","path":"/m/i.gguf","mode":"streaming"}
        ]}"#;
        let caps = parse_server_config(raw)
            .expect("清单要能解析")
            .models
            .remove(0)
            .caps;
        assert_eq!(caps.mode, "streaming", "服务端补的 mode 生效");
        assert!(caps.requires_voice_ref(), "服务端没补的 requires 仍要回落");
        assert!(
            !caps.known_issues.is_empty(),
            "服务端没补的 known_issues 仍要回落"
        );
        assert!(caps.is_streaming_only());
    }

    /// 验收③：**不渲染** server.json 时，`product_excluded` 与 streaming-only 的过滤也生效。
    ///
    /// 真机反例：`audio8-tts-01b` 会产出听不懂的音频却全程不报错（最坏的失败形态）；
    /// `audio8-tts-stream` 只走 `/v1/audio/speech/live`，离线批量与评估都不适用。
    ///
    /// 阳性对照：去掉 `parse_server_config` 的兜底 → 这两个 id 回到下拉，本用例红。
    #[test]
    fn excluded_and_streaming_are_filtered_even_when_the_server_declares_nothing() {
        let raw = r#"{"models":[
            {"id":"audio8-tts","task":"tts","path":"/m/a.gguf","mode":"offline"},
            {"id":"audio8-tts-01b","task":"tts","path":"/m/b.gguf","mode":"offline"},
            {"id":"audio8-tts-stream","task":"tts","path":"/m/c.gguf"},
            {"id":"index-tts2","task":"tts","path":"/m/i.gguf","mode":"offline"}
        ]}"#;
        let cfg = parse_server_config(raw).expect("清单要能解析");
        let names: Vec<String> = tts_engine_voices(&cfg.models)
            .iter()
            .map(|v| v.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec!["audio8-tts", "index-tts2"],
            "服务端没声明 product_excluded/mode 时，靠随包清单也要把 0.1b 与 stream 挡在下拉外"
        );
    }

    /// 源码级守卫：配音主按钮的可用性必须**消费 Rust 的投影**，不能在 Slint 里
    /// 重拼条件（改前是 `root.voice-index >= 0`，于是"这个引擎必须给参考音"
    /// 这条能力在按钮可用性上完全看不见）。
    ///
    /// 阳性对照：把 app.slint 那行改回 `voice-ready: root.voice-index >= 0;`
    /// → 本用例红（见 `LESSON_同一语义两处实现必然漂移` 的补充实例）。
    #[test]
    fn dub_voice_ready_is_projected_not_recomputed_in_slint() {
        let app = include_str!("../ui/app.slint");
        assert!(
            app.contains("voice-ready: root.dub-voice-ready;"),
            "配音主按钮的可用性要消费 Rust 侧投影"
        );
        assert!(
            !app.contains("voice-ready: root.voice-index >= 0;"),
            "不许在 Slint 里重拼『有引擎就绪』——那会漏掉引擎的能力要求"
        );
    }

    /// 源码级守卫：内置默认音色行的文案与可点性都要由**引擎能力**驱动，
    /// 不能再是"对任何引擎都说免参考音、开箱可用"的常量。
    ///
    /// 阳性对照：把 `enabled` 里的 `&& !root.engine-requires-voice-ref` 删掉
    /// → 本用例红。
    #[test]
    fn builtin_voice_row_is_gated_by_engine_capability() {
        let picker = include_str!("../ui/voice_picker.slint");
        assert!(
            picker.contains("in property <bool> engine-requires-voice-ref"),
            "音色区要接收引擎能力"
        );
        assert!(
            picker.contains("&& !root.engine-requires-voice-ref;"),
            "硬要求参考音的引擎必须把内置音色行置灰"
        );
        assert!(
            picker.contains("该引擎必须提供参考音频，不能用内置音色"),
            "文案要说清是引擎的硬要求"
        );
    }

    /// 源码级守卫：参考文本的占位/提示/红色告警都按**引擎要求**驱动，
    /// 不再写死"服务端要求与音频成对"（index-tts2 就不要文本，真机 200）。
    ///
    /// 阳性对照：把 placeholder 改回常量文案 / 把告警颜色改回
    /// `reference-text == ""` → 本用例红。
    #[test]
    fn reference_text_ui_is_driven_by_engine_requirement() {
        let picker = include_str!("../ui/voice_picker.slint");
        assert!(
            picker.contains("in property <bool> engine-requires-reference-text"),
            "音色区要接收『是否要求参考文本』能力"
        );
        assert!(
            picker.contains("placeholder: root.engine-requires-reference-text"),
            "占位文案要按引擎要求分支（必填 vs 可选）"
        );
        assert!(
            picker.contains(
                "color: root.engine-requires-reference-text && root.reference-text == \"\""
            ),
            "红色告警只属于『必填引擎缺文本』，可选引擎的转写状态不吓人"
        );
        // 透传链：app → dub_workbench → voice_picker，缺一段能力就上不了屏
        let app = include_str!("../ui/app.slint");
        assert!(
            app.contains("engine-requires-reference-text: root.engine-requires-reference-text;"),
            "app 要把能力透传给工作台"
        );
        let wb = include_str!("../ui/dub_workbench.slint");
        assert!(
            wb.contains("engine-requires-reference-text: root.engine-requires-reference-text;"),
            "工作台要把能力透传给音色区"
        );
        // 音色设计 Tab 与配音页共用 voice-ref 状态：红色告警与文件框也必须同口径。
        let design = include_str!("../ui/extra_tabs.slint");
        assert!(
            design.contains("in property <bool> engine-requires-reference-text"),
            "设计 Tab 要接收引擎要求（否则可选引擎缺文本也会标红）"
        );
        assert!(
            design.contains("root.engine-requires-reference-text && root.reference-text == \"\""),
            "设计 Tab 的红色告警也要按引擎要求"
        );
        assert!(
            design.contains("callback reference-pick();")
                && design.contains("root.reference-pick();"),
            "设计 Tab 的参考音频要有系统文件框入口"
        );
        assert!(
            app.contains("reference-pick => { root.reference-pick(); }"),
            "app 要把文件框回调透传给设计 Tab"
        );
    }

    /// 旁车修复只在「修复前缺失 → 修复后出现」时作废旧音频；其他组合不动作
    /// （不能借机清掉本来正常的工程音频）。
    #[test]
    fn reference_text_repair_only_invalidates_on_missing_to_present() {
        assert!(reference_text_was_repaired(true, true));
        assert!(
            !reference_text_was_repaired(true, false),
            "补不出旁车不能借机清音频"
        );
        assert!(
            !reference_text_was_repaired(false, true),
            "本来就有旁车不得重复作废"
        );
        assert!(!reference_text_was_repaired(false, false));
    }

    /// 旁车修复后必须**同时**挡住慢路径的逐句继承：快路径跳过只是不再整体复用，
    /// 慢路径 `reuse_done_sentences` 仍会把旧 done wav 拷回来（独立复评 reproduction
    /// 实测 left=1/right=0）。
    #[test]
    fn text_repaired_invalidation_must_also_skip_slow_path_reuse() {
        let dir = temp_dir("sidecar-invalidate");
        let vt = voice_trimmed_dir();
        std::fs::create_dir_all(&vt).unwrap();
        let tag = format!("aw-test-{}-{}", std::process::id(), line!());
        let copy = vt.join(format!("{tag}-10s.wav"));
        std::fs::write(&copy, test_wav_bytes(10.0, None)).unwrap();
        let sidecar = aw_core::ref_audio::reference_text_sidecar_path(&copy);
        let _ = std::fs::remove_file(&sidecar); // 入参必须是「修复前无旁车」
        let sentences_dir = dir.join("sentences");
        std::fs::create_dir_all(&sentences_dir).unwrap();
        std::fs::write(sentences_dir.join("000.wav"), b"old-mismatched-audio").unwrap();
        let mut saved = saved_project("第一句。第二句。", Some(copy.to_str().unwrap()));
        saved.voice_ref_text = Some("全文很长很长，比裁剪段多得多。".to_string());
        saved.voice_ref_hash = Some(sha256_file(&copy).unwrap());
        saved.sentences[0].status = "done".into();
        saved.save(&dir).unwrap();

        let loaded = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            Some(copy.to_string_lossy().into_owned()),
            Some("全文很长很长，比裁剪段多得多。".to_string()),
            GAP_MS,
            true,
            &empty_dict(),
        )
        .expect("应能加载");
        assert_eq!(
            loaded.reused, 0,
            "旁车修复后旧音频必须作废（快慢两条路径都不许继承）"
        );
        assert!(
            loaded.project.sentences.iter().all(|s| s.status != "done"),
            "旁车修复后已合成句必须回到待合成"
        );
        let _ = std::fs::remove_file(&copy);
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 已迁移副本（≤15s 且在 `voice-trimmed/`）在加载时补旁车：这是 0.1.10 用户
    /// 现有工程的唯一修复入口（它不会再触发裁剪，早返回路径必须也补旁车）。
    #[test]
    fn cached_trim_copy_in_trimmed_dir_gets_sidecar_without_retrim() {
        let vt = voice_trimmed_dir();
        std::fs::create_dir_all(&vt).unwrap();
        let tag = format!("aw-test-{}-{}", std::process::id(), line!());
        let copy = vt.join(format!("{tag}-10s.wav"));
        std::fs::write(&copy, test_wav_bytes(10.0, None)).unwrap();
        let sidecar = aw_core::ref_audio::reference_text_sidecar_path(&copy);
        let _ = std::fs::remove_file(&sidecar);
        let text = "很长的全文，比十秒能念下的内容多得多得多。";
        let prepared = prepare_reference_for_clone(copy.to_str().unwrap(), Some(text)).unwrap();
        assert_eq!(prepared.path, copy, "≤15s 的副本不该再裁");
        assert!(
            aw_core::ref_audio::paired_reference_text(&copy).is_some(),
            "voice-trimmed/ 里的旧副本必须在加载时补旁车"
        );
        let _ = std::fs::remove_file(&copy);
        let _ = std::fs::remove_file(&sidecar);
    }

    /// 裁剪副本补旁车：即使没有 ASR（CI 无引擎/引擎不可用），估算兜底也必须落一个
    /// 旁车，否则「84 字音频 × 1077 字文本」的错配会原样留在请求里。
    #[test]
    fn trimmed_reference_text_sidecar_is_written_even_without_asr() {
        let dir = std::env::temp_dir().join(format!("aw-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let trimmed = dir.join("cached-15s.wav");
        std::fs::write(&trimmed, b"x").unwrap();
        let long = "一二三四五六七八九十".repeat(20); // 200 字
        ensure_trimmed_reference_text_sidecar(&trimmed, 15.0, Some(&long));
        let paired = aw_core::ref_audio::paired_reference_text(&trimmed)
            .expect("必须写旁车（ASR 不是前提）");
        assert!(long.starts_with(&paired), "兜底必须是原文前缀：{paired}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 句柄检查必须走 `take_live_supervisor`（单一取锁）：旧的内联写法
    /// `if let Some(sup) = ENGINE_SUPERVISOR.lock()…as_mut()` 会在块内二次 lock，
    /// 同线程重入**永久死锁**，且只在引擎已崩溃的自愈分支触发（2026-09-22 复评）。
    #[test]
    fn engine_supervisor_handle_check_is_single_lock() {
        let src = production_source();
        let body = source_window(&src, "fn ensure_engine_serving(", 1400);
        assert!(
            body.contains("take_live_supervisor(&ENGINE_SUPERVISOR)"),
            "句柄检查必须用单一取锁的 helper：{body}"
        );
        assert!(
            !body.contains("as_mut()"),
            "不许再内联 as_mut()（块内二次 lock 会死锁）：{body}"
        );
    }

    /// 引导文案的时长范围必须与 `REFERENCE_MAX_SECONDS` 一致：
    /// 静态文案（slint / audio_client / docs）由本测试钉住，动态 prompt 必须由
    /// `aw_core::reference_range_label()` 生成（2026-09-22 复评 M1）。
    #[test]
    fn reference_range_label_is_pinned_to_static_copy() {
        let label = aw_core::reference_range_label();
        for (file, text) in [
            (
                "crates/aw-core/src/audio_client.rs",
                include_str!("../crates/aw-core/src/audio_client.rs"),
            ),
            (
                "ui/voice_picker.slint",
                include_str!("../ui/voice_picker.slint"),
            ),
            (
                "ui/extra_tabs.slint",
                include_str!("../ui/extra_tabs.slint"),
            ),
            (
                "docs/voice-clone.md",
                include_str!("../docs/voice-clone.md"),
            ),
        ] {
            assert!(
                text.contains(&label),
                "{file} 的引导文案必须含 {label}（改 REFERENCE_MAX_SECONDS 时要同步这些静态字面量）"
            );
        }
        let src = production_source();
        assert!(
            src.contains("aw_core::reference_range_label()"),
            "main.rs 的动态 prompt 必须由 helper 生成"
        );
        assert!(
            !src.contains("5–15 秒干净人声"),
            "main.rs 不许硬编码范围字面量（改常量会漂移）"
        );
    }

    /// `..` 与软链都要按**真实路径**算（LESSON：路径包含判定必须按真实路径）：
    /// 词法比较会把 `/models/x/../in/y.gguf` 判成"不在 /models 里"、把"经软链指向模型目录"
    /// 的路径也判成不在——模型盘明明挂了却显示没挂。
    #[test]
    fn models_under_dir_resolves_dotdot_and_symlinks() {
        let root = std::env::temp_dir().join(format!("aw-mud-{}", std::process::id()));
        let models = root.join("models");
        std::fs::create_dir_all(models.join("in")).unwrap();
        let model = |id: &str, path: String| ServerModel {
            id: id.into(),
            task: "tts".into(),
            family: "f".into(),
            path,
            url: String::new(),
            ..Default::default()
        };

        let cfg_with = |id: &str, path: String| {
            Some(ServerConfig {
                host: None,
                port: None,
                min_free_memory_mb: None,
                models: vec![model(id, path)],
            })
        };

        // ① `..` **逃出去**：字面上以 `<root>/models` 开头，真实路径在 `<root>/outside` 里
        //    —— 词法比较会误报成"模型盘上有这个模型"。
        std::fs::create_dir_all(root.join("outside")).unwrap();
        let escape = models
            .join("x")
            .join("..")
            .join("..")
            .join("outside")
            .join("y.gguf")
            .display()
            .to_string();
        assert_eq!(
            models_under_dir(&cfg_with("escape", escape), &models),
            0,
            "`..` 逃出模型目录的不能算在里面（词法前缀会误报）"
        );

        // ② `..` 只是绕一下、真实路径仍在模型目录里 → 要算
        let dotdot = models
            .join("x")
            .join("..")
            .join("in")
            .join("y.gguf")
            .display()
            .to_string();
        assert_eq!(
            models_under_dir(&cfg_with("dotdot", dotdot), &models),
            1,
            "`..` 要按真实路径消解"
        );

        // 软链：`link` 指向 models，挂在 link 下面的权重也算在 models 里
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&models, &link).unwrap();
            // ③ 经软链指向模型目录 → 真实在模型目录里，字面上不在
            let viasymlink = link.join("in").join("z.gguf").display().to_string();
            assert_eq!(
                models_under_dir(&cfg_with("viasymlink", viasymlink), &models),
                1,
                "经软链指向模型目录的路径也要算在里面"
            );

            // ④ 反过来：字面上在模型目录里，但中间那段是个指向外面的软链 → 不算
            let out_link = models.join("捷径");
            std::os::unix::fs::symlink(root.join("outside"), &out_link).unwrap();
            let sneaky = out_link.join("w.gguf").display().to_string();
            assert_eq!(
                models_under_dir(&cfg_with("sneaky", sneaky), &models),
                0,
                "模型目录里被软链引到外面的路径不能算（词法前缀会误报）"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    // 下载落盘位置现在由 `model_sources::plan_rows` 统一算（内置清单的上游布局优先，
    // 服务清单没给布局时按 path/url 兜底）：那些分支的用例在 `src/model_sources.rs`，
    // 这里不重复一份——两份实现正是这条链路上被抓过的错。

    /// B1（复核阻塞项）：点取消之后**不能**立刻允许为同一模型再排一条。
    ///
    /// 取消是协作式的：旧任务可能还在写 `.part`（读循环最多再落一个 64KiB 块才看到
    /// 标志）。旧实现点取消就把 `download_ids` 摘了，于是下一次点击被判成"没有在跑"
    /// 而再排一条，两条任务抢同一个 `.part`；只给 size 不给 sha256 的条目只按大小校验，
    /// "内容坏但长度恰好对上"会被 rename 成正式文件。
    ///
    /// 这条对旧实现会红：旧实现第二次、第三次点击都会再调 `enqueue`。
    #[test]
    fn cancel_click_keeps_the_task_registered_so_it_cannot_restart_immediately() {
        let state = Rc::new(UiState::default());
        let dest = PathBuf::from("/models/m.gguf");
        let mut enqueues: Vec<u64> = Vec::new();

        // 第一次点击：排一条（id=7）
        let fx = apply_download_click(
            &state,
            "m",
            |_| unreachable!("还没在跑，不该走取消"),
            || {
                enqueues.push(1);
                Some((download::Enqueued::Started(7), dest.clone()))
            },
        );
        assert_eq!(
            fx,
            ClickEffect::Enqueued {
                id: 7,
                dest: dest.clone()
            }
        );
        assert_eq!(active_download_id(&state, "m"), Some(7));

        // 点取消：请求发出去，但 id 必须**留在** download_ids 里（等 worker 终态快照摘）
        let fx = apply_download_click(
            &state,
            "m",
            |id| {
                assert_eq!(id, 7);
                download::CancelOutcome::Requested
            },
            || {
                enqueues.push(2);
                Some((download::Enqueued::Started(8), dest.clone()))
            },
        );
        assert_eq!(fx, ClickEffect::CancelRequested(7));
        assert_eq!(
            active_download_id(&state, "m"),
            Some(7),
            "取消时不能摘 id：摘了下一次点击就会再排一条，两条抢同一个 .part"
        );

        // 取消还没收尾就再点：必须是 no-op（仍然是「正在取消」），不能再排
        let fx = apply_download_click(
            &state,
            "m",
            |_| download::CancelOutcome::AlreadyRequested,
            || {
                enqueues.push(3);
                Some((download::Enqueued::Started(9), dest.clone()))
            },
        );
        assert_eq!(fx, ClickEffect::AlreadyCancelling(7));
        assert_eq!(
            enqueues,
            vec![1],
            "取消未收尾期间不得再排新任务（两条会抢同一个 .part）"
        );

        // 只有 worker 的终态快照摘掉 id 之后，才允许重下
        state.download_ids.borrow_mut().remove("m");
        let fx = apply_download_click(
            &state,
            "m",
            |_| unreachable!("已经摘了 id，不该走取消"),
            || {
                enqueues.push(4);
                Some((download::Enqueued::Started(10), dest.clone()))
            },
        );
        assert_eq!(fx, ClickEffect::Enqueued { id: 10, dest });
        assert_eq!(enqueues, vec![1, 4]);
    }

    /// `cancel` 返回 Finished（任务其实已经收尾）时要如实说，别谎报"已取消"；
    /// 重复点取消是 no-op，也不能再排新下载。
    #[test]
    fn click_on_already_finished_task_says_finished_and_never_enqueues() {
        let state = Rc::new(UiState::default());
        state.download_ids.borrow_mut().insert("m".into(), 7);
        let fx = apply_download_click(
            &state,
            "m",
            |_| download::CancelOutcome::Finished,
            || panic!("已经不在队列里，绝不能排新任务"),
        );
        assert_eq!(fx, ClickEffect::AlreadyFinished(7));
        // 终态快照还没到，id 仍在册；下一次点击还是按"取消"这条路走
        assert_eq!(active_download_id(&state, "m"), Some(7));
    }

    /// 复核第二轮 B1：旧任务的**迟到终态快照**不能把用户刚登记的新任务摘掉。
    ///
    /// 同一个模型可以反复登记（取消后再下 / 重下）：旧任务摘掉自己的登记、放行下一次
    /// 下载；若只按 label 摘不看 id，旧任务的迟到终态就会把**新**任务的登记摘掉——
    /// UI 以为空闲，「下载」又可点，同一个 dest 上叠出第二条任务抢同一个 `.part`。
    ///
    /// 这条对"只按 label 摘"的旧实现会红：第一、二条断言都会失败。
    #[test]
    fn stale_terminal_snapshot_does_not_release_the_new_task() {
        let mut ids: HashMap<String, u64> = HashMap::new();
        ids.insert("m".to_string(), 2); // 用户已经给同一个模型登记了新任务
                                        // 旧任务（id=1）迟到来的终态：不许摘掉新任务
        assert!(
            !release_download_id(&mut ids, "m", 1),
            "旧任务的终态不该认领新任务的登记"
        );
        assert_eq!(
            ids.get("m"),
            Some(&2),
            "旧任务的终态快照不能把新任务的 id 摘掉"
        );
        // 确实是这条任务自己的终态才摘
        assert!(release_download_id(&mut ids, "m", 2));
        assert!(ids.is_empty());
    }

    /// 同理，旧任务的迟到快照也不能盖掉**界面行**里新任务的状态：
    /// 否则按钮显示「下载/重下」，而 `download_ids` 里其实还挂着在跑的新任务。
    ///
    /// 这条对"按 label 无条件覆盖"的旧实现会红（旧实现会把 id=1 的 Cancelled 盖上去）。
    #[test]
    fn stale_snapshot_does_not_overwrite_the_newer_task_row() {
        let snap = |id: u64, label: &str, state: download::State| download::Snapshot {
            id,
            label: label.to_string(),
            dest: PathBuf::from("/models/m.gguf"),
            state,
            downloaded: 0,
            total: None,
            note: String::new(),
        };
        let mut list: Vec<download::Snapshot> = Vec::new();
        merge_download_snapshot(
            &mut list,
            snap(2, "m", download::State::Downloading), // 新任务正在跑
        );
        // 旧任务（id=1）的迟到终态：直接丢掉，不许盖掉新任务那一行
        merge_download_snapshot(&mut list, snap(1, "m", download::State::Cancelled));
        assert_eq!(list.len(), 1, "同一模型只占一行");
        assert_eq!(list[0].id, 2, "留下的必须是新任务那条");
        assert_eq!(list[0].state, download::State::Downloading);
        // 新任务自己的终态照常合并
        merge_download_snapshot(&mut list, snap(2, "m", download::State::Done));
        assert_eq!(list[0].state, download::State::Done);
    }

    /// 清单里找不到模型时如实报 UnknownModel（点之前清单被改过），不静默排空任务。
    #[test]
    fn click_with_unknown_model_reports_unknown() {
        let state = Rc::new(UiState::default());
        let fx = apply_download_click(&state, "gone", |_| unreachable!("没有在跑"), || None);
        assert_eq!(fx, ClickEffect::UnknownModel);
        assert!(state.download_ids.borrow().is_empty());
    }

    /// `resolve_base` 的字段级覆盖语义：单边覆盖必须生效。
    ///
    /// 注意本用例**测的是纯函数**，钉不住"调用方是否真的用它"——审查复核过：
    /// 把 `discover_engine` 改回旧的成对逻辑，这条仍绿。那条不变式目前只能靠
    /// "非测试代码里只有 resolve_base 一处拼 `http://host:port`"这一构造来保证
    /// （见该函数注释）。要真正钉住得让 `discover_engine` 也吃显式参数（环境变量
    /// 无法在测试里安全设置，见 LESSON_多线程测试中set_var修改进程环境是UB）。
    #[test]
    fn resolve_base_handles_field_level_overrides() {
        let cfg = Some(ServerConfig {
            host: Some("manifest-host".into()),
            port: Some(1111),
            min_free_memory_mb: None,
            models: vec![],
        });

        // 只覆盖端口 → host 回落清单
        let over_port = AppSettings {
            port: Some(9999),
            ..Default::default()
        };
        assert_eq!(
            resolve_base(&over_port, &cfg, None).0,
            "http://manifest-host:9999"
        );

        // 只覆盖 host → 端口回落清单
        let over_host = AppSettings {
            host: Some("10.0.0.1".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_base(&over_host, &cfg, None).0,
            "http://10.0.0.1:1111"
        );

        // AW_SERVER 整串优先，且标记 from_env
        let (base, from_env) = resolve_base(&over_host, &cfg, Some("http://env:2222"));
        assert_eq!(base, "http://env:2222");
        assert!(from_env);

        // 有没有地址来源：决定 discover_engine 返回 None（"未发现服务"）还是可用地址
        let empty = AppSettings::default();
        assert!(!has_endpoint_source(&empty, &None, None));
        assert!(has_endpoint_source(&empty, &cfg, None));
        assert!(has_endpoint_source(&empty, &None, Some("http://env:2222")));
    }

    /// 扫描条目上限是**整次扫描的总预算**（不是每个目录各一份）。
    /// 样本刻意放成 3 个子目录 × 2 个文件：per-dir 上限会数到 6，
    /// 总预算必须停在 ≤2，这条断言才能真的区分两种实现。
    #[test]
    fn scan_model_dir_stops_at_entry_budget() {
        let dir = temp_dir("scan-cap");
        for sub in ["a", "b", "c"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            for i in 0..2 {
                std::fs::write(dir.join(format!("{sub}/m{i}.gguf")), b"x").unwrap();
            }
        }
        let (exists, n) = scan_model_dir_with_limit(&dir, 2);
        assert!(exists);
        assert!(
            n <= 2,
            "总预算 2 时最多数到 2 个（per-dir 实现会数到 6），实得 {n}"
        );
    }

    /// 服务地址三档优先级：AW_SERVER 整串优先；全局设置 > 清单；host/port **各自独立**回落
    /// （早期要求成对，导致"只改端口"被静默忽略）。
    #[test]
    fn resolve_endpoint_prefers_env_then_settings_then_manifest() {
        // 环境变量整串覆盖
        assert_eq!(
            resolve_endpoint(
                Some("10.0.0.1"),
                Some(9999),
                Some("manifest-host"),
                Some(1111),
                Some("http://env:2222")
            ),
            ("http://env:2222".to_string(), String::new(), true)
        );

        // 只覆盖端口：host 回落清单
        assert_eq!(
            resolve_endpoint(None, Some(9999), Some("manifest-host"), Some(1111), None),
            ("manifest-host".to_string(), "9999".to_string(), false)
        );

        // 只覆盖 host：port 回落清单
        assert_eq!(
            resolve_endpoint(
                Some("10.0.0.1"),
                None,
                Some("manifest-host"),
                Some(1111),
                None
            ),
            ("10.0.0.1".to_string(), "1111".to_string(), false)
        );

        // 空白 host 视同没填
        assert_eq!(
            resolve_endpoint(Some("   "), None, Some("manifest-host"), None, None),
            ("manifest-host".to_string(), "8080".to_string(), false)
        );

        // 全空 → 默认
        assert_eq!(
            resolve_endpoint(None, None, None, None, None),
            ("127.0.0.1".to_string(), "8080".to_string(), false)
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("audio-workshop-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试用的空词典（P5 把词典接进文本层后，构造/续作都要显式给一份）。
    fn empty_dict() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::new()
    }

    fn saved_project(script: &str, voice_ref: Option<&str>) -> Project {
        Project::new(
            script,
            "audio8-tts",
            GAP_MS,
            BASE_SEED,
            voice_ref.map(str::to_string),
            DEFAULT_PUNCTUATION,
            MAX_CHARS,
            |t| aw_core::normalize(t, &Default::default()),
        )
    }

    /// 主 crate 没有 hound，RIFF 头手写（与 tiny_silent_wav 同一套路）：
    /// 8kHz 单声道 16bit 静音 wav，**头里声明 `seconds` 秒**，实际数据可截断
    /// （`data_seconds` = None 时写满）——用来造"头声明超长、数据损坏"的假参考音。
    fn test_wav_bytes(seconds: f64, data_seconds: Option<f64>) -> Vec<u8> {
        let rate = 8_000u32;
        let declared = (seconds * rate as f64).round() as u32 * 2;
        let actual = (data_seconds.unwrap_or(seconds) * rate as f64).round() as u32 * 2;
        let mut out = Vec::with_capacity(44 + actual as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + declared).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&declared.to_le_bytes());
        out.resize(44 + actual as usize, 0);
        out
    }

    // ── 音色克隆：参考文本是音色的一部分（招牌功能 D11）────────────────────
    //
    // 背景（真机）：audio8-tts 收到 voice_ref 却收不到 reference_text 会 HTTP 500，
    // index-tts2 只给 voice_ref 就 200 —— 参考文本是**按引擎**的要求。
    // 这里钉住"改回去就红"的判据：只有 requires.reference_text 的引擎在
    // "克隆态 + 缺文本"时才拦。

    /// 拦截判据**按引擎**：要求文本的引擎在"克隆态 + 缺文本"时才为真；
    /// 不要求文本的引擎与内置音色都不拦。
    #[test]
    fn reference_text_missing_only_fires_for_required_engine_clone_without_text() {
        let path = Some("/x/我的声线.wav".to_string());
        // audio8-tts（requires.reference_text = true）
        assert!(
            reference_text_missing(true, &path, &None),
            "必填引擎的克隆态没填文本 → 必须拦住（否则 N 句各撞一次 500）"
        );
        assert!(
            reference_text_missing(true, &path, &Some("   ".to_string())),
            "纯空白不算填了"
        );
        assert!(
            !reference_text_missing(true, &path, &Some("实际念的内容".to_string())),
            "填了就该放行"
        );
        assert!(
            !reference_text_missing(true, &None, &None),
            "必填引擎的内置音色（没给 voice_ref）不需要参考文本"
        );
        // index-tts2（requires.reference_text = false）：给了参考音也不要求文本
        assert!(
            !reference_text_missing(false, &path, &None),
            "可选引擎不该被拦（真机：只给 voice_ref 就 200）"
        );
        assert!(
            !reference_text_missing(false, &None, &None),
            "可选引擎的内置音色也不需要参考文本"
        );
    }

    /// 参考文本变了 → 旧音频**不能**复用。
    ///
    /// 这是"让用户核对/修正 ASR 转写"能不能真的生效的关键：少了这一条，
    /// 用户改完文本点开始合成，句子全是 done 被直接复用，听到的还是旧转写的声音。
    #[test]
    fn reuse_is_refused_when_only_the_reference_text_changed() {
        let mut saved = saved_project("第一句。", Some("/x/我的声线.wav"));
        saved.voice_ref_hash = Some("hash-a".into());
        saved.voice_ref_text = Some("旧转写。".into());

        let same = (Some("/x/我的声线.wav".to_string()), Some("hash-a".into()));
        assert!(
            settings_allow_reuse(
                &saved,
                "audio8-tts",
                &same.0,
                &same.1,
                &Some("旧转写。".into()),
                true,
                &effective_dict_hash(&saved),
            ),
            "三样都一致才允许复用"
        );
        assert!(
            !settings_allow_reuse(
                &saved,
                "audio8-tts",
                &same.0,
                &same.1,
                &Some("改过的转写。".into()),
                true,
                &effective_dict_hash(&saved),
            ),
            "只改参考文本也必须判成换音色"
        );
    }

    /// 没有参考音就不能留孤立的参考文本（否则切回内置音色再选回同一段音频，
    /// 会静默沿用上一份转写）。
    #[test]
    fn orphan_reference_text_is_dropped() {
        let p = new_project_from_inputs(
            "第一句。",
            "audio8-tts",
            None,
            Some("这段文本没有对应的参考音".into()),
            GAP_MS,
            true,
            &empty_dict(),
        );
        assert_eq!(p.voice_ref_text, None, "无参考音时不该留下文本");

        let with_ref = new_project_from_inputs(
            "第一句。",
            "audio8-tts",
            Some("/x/我的声线.wav".into()),
            Some("实际念的内容".into()),
            GAP_MS,
            true,
            &empty_dict(),
        );
        assert_eq!(with_ref.voice_ref_text.as_deref(), Some("实际念的内容"));
    }

    /// 参考文本只在"还是同一段音频"时保留。
    #[test]
    fn reference_text_only_survives_when_the_path_is_unchanged() {
        assert!(
            keeps_reference_text("/x/a.wav", "/x/a.wav"),
            "同一段音频要保留（否则用户点一下输入框就丢转写）"
        );
        assert!(
            keeps_reference_text("  /x/a.wav ", "/x/a.wav"),
            "只差首尾空白算同一段"
        );
        assert!(
            !keeps_reference_text("/x/a.wav", "/x/b.wav"),
            "换到别的音频必须清（否则拿 A 的转写去条件 B）"
        );
        assert!(!keeps_reference_text("/x/a.wav", ""), "清空必须清");
        assert!(
            !keeps_reference_text("", "/x/a.wav"),
            "从空变成有：本来也没有文本可留"
        );
    }

    /// 换参考音频的每条路径都必须把参考文本一起清 —— 否则**静默**拿上一段的转写当条件。
    ///
    /// 这条是**源码级守卫**（第四轮复核真机抓到的）：音色库「应用」与手改路径只换
    /// `voice-ref-path`、不清 `voice-ref-text` ⇒ 文本非空时 `reference_text_missing`
    /// 拦不住，而 `settings_allow_reuse` 见 `voice_ref` 变了又去重录 ⇒ 拿 A 音频的转写
    /// 去条件 B 音频。服务端**不报错**、产物已经变了（复核真机：正确文本 273241B /
    /// 完全无关文本 289628B，sha 也不同）。
    ///
    /// 加锁方式：把"写 `voice-ref-path`"收敛成唯一一处 `set_voice_ref`（它内部按
    /// `keeps_reference_text` 决定要不要清文本）。谁再直接写这个属性，这条立刻红。
    #[test]
    fn reference_path_writes_go_through_the_helper_that_clears_the_text() {
        let src = src_lf(include_str!("main.rs"));
        // 拼接构造 needle：否则这条用例自己的源码就会被算成一次命中
        let needle = concat!("ui.set_voice_ref_", "path(");
        let writes = src.matches(needle).count();
        assert_eq!(
            writes, 1,
            "写 voice-ref-path 只允许在 set_voice_ref 里一处（现在 {writes} 处）：\
             多出来的地方必须改走 set_voice_ref，否则换音频会留着上一段的参考文本"
        );

        // 反向也要钉住：helper 本身必须真的清（不能只留个名字）
        let body = source_window(&src, "fn set_voice_ref(", 400);
        assert!(
            body.contains("clear_reference_text(") && body.contains("keeps_reference_text("),
            "set_voice_ref 必须按 keeps_reference_text 判断、并真的清文本：{body}"
        );
    }

    fn save_done_project(dir: &Path, project: &mut Project) {
        std::fs::create_dir_all(dir.join("sentences")).unwrap();
        for sentence in &mut project.sentences {
            sentence.status = "done".into();
            sentence.duration = Some(1.0);
            std::fs::write(
                dir.join(format!("sentences/{:03}.wav", sentence.index)),
                format!("wav-{}-{}", sentence.index, sentence.text).as_bytes(),
            )
            .unwrap();
        }
        project.save(dir).unwrap();
    }

    fn wav_sentinel(sentence: &aw_core::Sentence) -> String {
        format!("wav-{}-{}", sentence.index, sentence.text)
    }

    /// 复核抓到的：`Cmd::Assemble` 改 gap 后 save 失败，**内存里的 gap 不能被改掉**。
    /// 做法：让"保存工程"这条路必失败，连续发两次同值 Assemble——两次都必须报"保存工程失败"。
    /// 如果实现是"先改内存再 save"，第二次会因为字段已相等而跳过 save、直接去拼装，
    /// 最后拼出与 project.json 记录不一致的成品。
    #[test]
    fn assemble_gap_save_failure_does_not_mutate_memory() {
        let root = std::env::temp_dir().join(format!("aw-assemble-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut project = saved_project("第一句。第二句。", None);
        save_done_project(&root, &mut project);

        // 注入"保存工程必失败"。**必须与平台无关**：
        //
        // 原来这里是把工程目录 chmod 成只读 —— 那条注入只在 Unix 生效。Windows 的
        // READONLY 属性**不阻止**在目录里创建/改名文件，于是那边 assemble 会真的拼成功，
        // 测试反过来报"目录不可写，不该拼成功"（2026-09-18 三平台 CI 实测）。
        //
        // 换成"project.json 是非空目录"：`write_atomic` 最后那步 rename(临时文件 → project.json)
        // 在 Windows 与 Unix 上**都会**失败，注入与平台无关；而 out/ 仍可写，所以拼装本身不受影响
        // —— 这正好保持用例的原意（拼装能跑，是**保存工程**失败）。
        std::fs::remove_file(root.join("project.json")).unwrap();
        std::fs::create_dir_all(root.join("project.json")).unwrap();
        std::fs::write(root.join("project.json").join("keep"), b"x").unwrap();

        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let worker_root = root.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: worker_root,
                cancel: cancel::CancelRegistry::new(),
            })
        });
        cmd_tx
            .send(Cmd::OpenProject {
                revision: 1,
                dir: root.clone(),
                project,
            })
            .unwrap();
        let new_gap = GAP_MS + 500;
        let expect_save_failure =
            |cmd_tx: &Sender<Cmd>, msg_rx: &Receiver<WorkerMsg>, nth: &str| {
                cmd_tx
                    .send(Cmd::Assemble {
                        revision: 1,
                        gap_ms: new_gap,
                    })
                    .unwrap();
                loop {
                    let m = msg_rx.recv().expect("worker 应有消息");
                    match m.msg {
                        Msg::AssembleFailed(e) => break e,
                        Msg::Assembled { .. } => panic!("{nth}：目录不可写，不该拼成功"),
                        _ => {}
                    }
                }
            };
        let first = expect_save_failure(&cmd_tx, &msg_rx, "第一次");
        assert!(first.contains("保存工程失败"), "{first}");
        let second = expect_save_failure(&cmd_tx, &msg_rx, "第二次");
        assert!(
            second.contains("保存工程失败"),
            "第二次仍应尝试保存并失败（说明内存里的 gap 没被偷偷改掉）：{second}"
        );

        drop(cmd_tx);
        handle.join().unwrap();
        // 收尾：把注入的"project.json 目录"还原掉，别给 temp 清理留坑
        // （原来是恢复目录权限；注入方式换了，收尾也跟着换）
        let _ = std::fs::remove_file(root.join("project.json").join("keep"));
        let _ = std::fs::remove_dir_all(root.join("project.json"));
    }

    /// 停顿输入的即时反馈：留空/非法/超上限各有说法（与 `normalize_gap_ms` 同一判据）。
    #[test]
    fn gap_hint_explains_what_will_be_used() {
        assert!(gap_hint_for("").contains("默认"));
        assert!(gap_hint_for("abc").contains("毫秒数"), "非法要说明填什么");
        let over = gap_hint_for("3000");
        assert!(over.contains("上限") && over.contains("2000"), "{over}");
        let ok = gap_hint_for("300");
        assert!(ok.contains("300"), "{ok}");
    }

    /// 复核抓到的关键点：版本快照里的句子全是 pending，**回滚必须先把音频按文本继承过来**
    /// 再落盘——否则下一次合成走 `load_resumable` 快路径会原样返回这份全 pending 工程，
    /// "文本相同的句子自动复用"就落空了。
    #[test]
    fn rollback_inherits_audio_for_unchanged_sentences() {
        let dir = temp_dir("rollback-inherit");
        // 当前工程：两句都已合成（有 wav + done 状态）
        let mut current = saved_project("第一句。第二句。", None);
        save_done_project(&dir, &mut current);
        let inherited = Project::load(&dir).unwrap();

        // 版本快照 = 稿件 + 设置（重建出来全是 pending），这正是留档写下的那份
        let mut restored = new_project_from_inputs(
            "第一句。第二句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        );
        assert!(
            restored.sentences.iter().all(|s| s.status == "pending"),
            "前提：重建成的新工程没有句级状态"
        );

        let reused = reuse_done_sentences(&mut restored, &inherited, &dir).unwrap();
        assert_eq!(reused, 2, "文本相同的两句都该继承到音频");
        versions::commit_rollback(&dir, &restored).unwrap();

        // 关键断言：下一次「开始合成」不会把这句当成待录
        let loaded = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert!(
            loaded.project.sentences.iter().all(|s| s.status == "done"),
            "回滚后同文本的句子必须仍是已合成（否则会全部重录）"
        );
    }

    /// 复核给的反例：回滚的"按文本继承"也必须过与 `load_resumable` 相同的设置门槛——
    /// 模型 / 兜底开关 / 参考音不一致时复用音频，会把"另一个模型合成的 wav"标成这份
    /// 版本的已合成（界面显示 A、听起来是 B）。
    #[test]
    fn rollback_does_not_inherit_audio_when_settings_differ() {
        let dir = temp_dir("rollback-settings");
        // 当前工程：一套具体设置 + 两句已合成
        let mut current = saved_project("第一句。第二句。", None);
        current.model = "index-tts2".into();
        current.auto_normalize = false;
        current.voice_ref = Some("/x/别的声线.wav".into());
        current.voice_ref_hash = Some("hash-x".into());
        save_done_project(&dir, &mut current);
        let inherited = Project::load(&dir).unwrap();
        assert!(settings_allow_reuse(
            &inherited,
            &inherited.model,
            &inherited.voice_ref,
            &inherited.voice_ref_hash,
            &inherited.voice_ref_text,
            inherited.auto_normalize,
            &effective_dict_hash(&inherited),
        ));

        // 版本与当前工程只在"被 tweak 的那一项"上不同（其余整份克隆，避免测试自己写错）
        type Tweak = fn(&mut Project);
        let cases: Vec<(&str, Tweak)> = vec![
            ("模型不同", |p: &mut Project| {
                p.model = "audio8-tts".into()
            }),
            ("兜底开关不同", |p: &mut Project| {
                p.auto_normalize = true
            }),
            ("参考音不同", |p: &mut Project| {
                p.voice_ref = Some("/y/另一个声线.wav".into());
                p.voice_ref_hash = Some("hash-y".into());
            }),
            ("同名但内容变了", |p: &mut Project| {
                p.voice_ref_hash = Some("hash-z".into());
            }),
            ("换了词典", |p: &mut Project| {
                p.dict_hash = Some("别的词典指纹".into());
            }),
        ];
        for (name, tweak) in cases {
            let mut version = inherited.clone();
            tweak(&mut version);
            assert!(
                !settings_allow_reuse(
                    &inherited,
                    &version.model,
                    &version.voice_ref,
                    &version.voice_ref_hash,
                    &version.voice_ref_text,
                    version.auto_normalize,
                    &effective_dict_hash(&version),
                ),
                "{name}：设置不一致就不该允许复用音频"
            );
        }
    }

    /// **当前工程损坏时回滚必须中止、一个字节都不写**：损坏的 project.json 是唯一可人工
    /// 恢复的现场，回滚把它当成"没有当前工程"继续写盘就把它抹了（复核抓到的数据安全阻塞）。
    #[test]
    fn rollback_refuses_when_current_project_is_corrupt() {
        let dir = temp_dir("rollback-corrupt");
        let id = versions::save(
            &dir,
            "好版本",
            &new_project_from_inputs(
                "第一句。",
                "audio8-tts",
                None,
                None,
                GAP_MS,
                true,
                &empty_dict(),
            ),
            1,
        )
        .unwrap();
        let corrupt = b"{ this is not json";
        std::fs::write(dir.join("project.json"), corrupt).unwrap();

        let err = rollback_with_inheritance(&dir, &id, &empty_dict()).unwrap_err();
        assert!(err.contains("不会覆盖现场"), "{err}");
        assert_eq!(
            std::fs::read(dir.join("project.json")).unwrap(),
            corrupt,
            "损坏的现场必须原样保留"
        );

        // 对照：工程文件正常时回滚照常完成
        new_project_from_inputs(
            "第一句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .save(&dir)
        .unwrap();
        assert!(rollback_with_inheritance(&dir, &id, &empty_dict()).is_ok());
    }

    /// 版本列表里的时间用"多久以前"说（不引日期库）：四档 + 未来时间兜底。
    #[test]
    fn relative_time_states_the_scale() {
        // 选一个足够大的 now：下面要减到"2 天前"，小数值会把它减成负溢出
        let now = 1_000_000_000u64;
        assert_eq!(relative_time(now, now), "刚刚");
        assert_eq!(relative_time(now, now - 30_000), "刚刚");
        assert_eq!(relative_time(now, now - 90_000), "1 分钟前");
        assert_eq!(relative_time(now, now - 3 * 3_600_000), "3 小时前");
        assert_eq!(relative_time(now, now - 2 * 86_400_000), "2 天前");
        assert_eq!(
            relative_time(now, now + 5_000),
            "刚刚",
            "时钟回拨也不出负数"
        );
    }

    /// 造工程只有一个入口：worker 造新工程与"版本留档"都用它。
    /// 这里钉住它真的按输入记下了停顿、兜底开关，并按开关决定 spoken 文本。
    #[test]
    fn new_project_from_inputs_records_settings() {
        let on = new_project_from_inputs(
            "2024年第一句。",
            "audio8-tts",
            None,
            None,
            750,
            true,
            &empty_dict(),
        );
        assert_eq!(on.gap_ms, 750);
        assert!(on.auto_normalize);
        assert_ne!(
            on.sentences[0].spoken, on.sentences[0].text,
            "开兜底时 spoken 应被规范化（2024年 → 二零二四年）"
        );

        let off = new_project_from_inputs(
            "2024年第一句。",
            "audio8-tts",
            None,
            None,
            750,
            false,
            &empty_dict(),
        );
        assert!(!off.auto_normalize);
        assert_eq!(
            off.sentences[0].spoken, off.sentences[0].text,
            "关兜底就是原文照念"
        );
        assert_eq!(off.voice_ref, None);
        assert_eq!(off.voice_ref_hash, None, "没有参考音就没有哈希");
    }

    /// 存模板前必须有个名字：空白名字直接拒绝（否则会存出一条没法选中的无名模板）。
    #[test]
    fn template_from_inputs_requires_a_name() {
        let t = template_from_inputs(
            "  口播标准  ",
            "audio8-tts",
            Some("/v.wav".into()),
            None,
            1.1,
            300,
            false,
        )
        .unwrap();
        assert_eq!(t.name, "口播标准", "名字要 trim");
        assert_eq!(t.model, "audio8-tts");
        assert_eq!(t.voice_ref.as_deref(), Some("/v.wav"));
        assert!((t.speed - 1.1).abs() < 1e-6);
        assert_eq!(t.gap_ms, 300);
        assert!(!t.auto_normalize);

        for blank in ["", "   ", "\t"] {
            let err =
                template_from_inputs(blank, "audio8-tts", None, None, 1.0, 250, true).unwrap_err();
            assert!(err.contains("名字"), "{err}");
        }
    }

    /// 复核要求的"真正覆盖 handler 那条判据"：导入应用函数收到 `busy=true`
    /// （= 文件框打开期间起了任何任务）时必须**什么都不做**；空闲时才真的导入。
    #[test]
    fn dict_import_is_refused_while_busy_and_applies_when_idle() {
        let root = temp_dir("dict-import-busy");
        let file = root.join("词条.tsv");
        std::fs::write(&file, "重庆\t崇庆\n单于\t蝉于\n").unwrap();

        // 忙：一个词条都不许进库（这就是"文件框开着时起了歌曲/分离"的场景）
        let err = import_entries_into_library(&root, &file, None, true, 1).unwrap_err();
        assert!(err.contains("任务进行中"), "{err}");
        assert!(
            dictionaries::list(&root).0.is_empty(),
            "忙的时候不许动词典库"
        );

        // 空闲：正常导入，并按文件名建一套
        let applied = import_entries_into_library(&root, &file, None, false, 2).unwrap();
        assert_eq!(applied.imported, 2);
        assert!(applied.skipped.is_empty(), "{:?}", applied.skipped);
        let d = dictionaries::load_file(&root, &applied.entry.file).unwrap();
        assert_eq!(d.entries.get("重庆").map(String::as_str), Some("崇庆"));
    }

    /// 复核抓到的竞态：**文件框打开后再起别的任务**（歌曲/分离这类不设全局
    /// `running`/`busy`，只进台账），回调到达时必须据此拒绝导入。
    /// 这里钉判据本身：台账里有任务在飞 / 批量在飞 → 词典控件判定为忙。
    #[test]
    fn dictionary_controls_are_busy_when_any_task_is_in_flight() {
        let state = Rc::new(UiState::default());
        assert!(state_dictionary_idle(&state), "什么都没跑时应空闲");

        // 歌曲/分离这类任务只进台账（不设全局 running/busy）
        state
            .tasks
            .borrow_mut()
            .enqueue(tasks::TaskKind::Song, "音乐制作 · 生成歌曲");
        assert!(
            !state_dictionary_idle(&state),
            "台账里有任务在飞时，词典控件必须算忙（否则文件框回调会偷偷换词典）"
        );

        // 批量在飞：在跑那一行是 Running
        let batch_state = Rc::new(UiState::default());
        batch_state.batch_rows.borrow_mut().push(BatchRowState {
            name: "甲".into(),
            script: String::new(),
            sentences: 1,
            task_id: Some(1),
            state: batch::ItemState::Running,
            detail: String::new(),
            out: None,
        });
        assert!(!state_dictionary_idle(&batch_state), "批量在跑也算忙");
    }

    /// 复核抓到的：没启用词典时导入，若库里已有同名词典，**必须合并而不是覆盖**
    /// （否则原词条被同名覆盖语义吃掉）。这里直接测"合并策略"这段逻辑本身。
    #[test]
    fn importing_without_active_dict_merges_an_existing_same_name_dict() {
        let root = temp_dir("dict-import-merge");
        // 库里已有一套"甲"（未启用）
        let existing = dictionaries::save(
            &root,
            "甲",
            [("a".to_string(), "1".to_string())].into_iter().collect(),
            1,
            file_stem,
        )
        .unwrap();
        // 模拟导入：目标是同名"甲"，先把已有条目读出来
        let (rows, _) = dictionaries::list(&root);
        let mut merged = rows
            .iter()
            .filter(|r| r.name.eq_ignore_ascii_case("甲"))
            .find_map(|r| dictionaries::load_file(&root, &r.file).ok())
            .map(|d| d.entries)
            .unwrap_or_default();
        merged.insert("b".to_string(), "2".to_string());
        let saved = dictionaries::save(&root, "甲", merged, 2, file_stem).unwrap();

        assert_ne!(saved.file, existing.file, "写新文件、不覆盖旧资产");
        let after = dictionaries::load_file(&root, &saved.file).unwrap();
        assert_eq!(after.entries.len(), 2, "原词条 + 新词条：{after:?}");
        assert_eq!(after.entries.get("a").map(String::as_str), Some("1"));
        assert_eq!(after.entries.get("b").map(String::as_str), Some("2"));
    }

    /// 词典真的进了文本层：同一个稿件，带词典时 spoken 变成替换后的读法；
    /// 而且**关掉数字兜底不影响词典**（词典是"这个词怎么念"，与数字规则是两件事）。
    #[test]
    fn dictionary_applies_to_spoken_text() {
        let dict: std::collections::BTreeMap<String, String> =
            [("重庆".to_string(), "崇庆".to_string())]
                .into_iter()
                .collect();

        let with_dict =
            new_project_from_inputs("重庆的桥。", "audio8-tts", None, None, 250, true, &dict);
        assert_eq!(with_dict.sentences[0].spoken, "崇庆的桥。");
        assert_eq!(
            with_dict.dict_hash,
            Some(dictionaries::fingerprint(&dict)),
            "工程要记下词典指纹"
        );

        // 关掉数字兜底，词典仍然生效
        let no_rule =
            new_project_from_inputs("重庆的桥。", "audio8-tts", None, None, 250, false, &dict);
        assert_eq!(no_rule.sentences[0].spoken, "崇庆的桥。");

        // 空词典 = 原文
        let empty = new_project_from_inputs(
            "重庆的桥。",
            "audio8-tts",
            None,
            None,
            250,
            true,
            &empty_dict(),
        );
        assert!(
            empty.sentences[0].spoken.contains("重庆") || empty.sentences[0].spoken.contains("重")
        );
        assert_ne!(empty.dict_hash, with_dict.dict_hash);
    }

    /// 换词典 = 改 spoken 文本 → 旧音频一律不复用（与换模型/换兜底开关同类）。
    #[test]
    fn dictionary_change_invalidates_reuse() {
        let dict_a: std::collections::BTreeMap<String, String> =
            [("重庆".to_string(), "崇庆".to_string())]
                .into_iter()
                .collect();
        let dict_b: std::collections::BTreeMap<String, String> =
            [("重庆".to_string(), "重青".to_string())]
                .into_iter()
                .collect();

        let dir = temp_dir("dict-change");
        // 当前工程：用词典 A 合成好的句子
        let mut old = new_project_from_inputs(
            "重庆的桥。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &dict_a,
        );
        save_done_project(&dir, &mut old);

        // 用词典 B 续作：不能复用（旧音频念的是另一套）
        let loaded = load_resumable(
            &dir,
            "重庆的桥。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &dict_b,
        )
        .unwrap();
        assert_eq!(loaded.reused, 0);
        assert!(
            loaded
                .project
                .sentences
                .iter()
                .all(|s| s.status == "pending"),
            "换词典要重录，不能把旧读法留在成品里"
        );
        assert_eq!(
            loaded.project.dict_hash,
            Some(dictionaries::fingerprint(&dict_b))
        );

        // 同一套词典：照旧续作（句子仍是 done）
        let mut old2 = new_project_from_inputs(
            "重庆的桥。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &dict_a,
        );
        save_done_project(&dir, &mut old2);
        let same = load_resumable(
            &dir,
            "重庆的桥。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &dict_a,
        )
        .unwrap();
        assert!(
            same.project.sentences.iter().all(|s| s.status == "done"),
            "词典没变就该续作"
        );
    }

    /// 停顿输入的归一：留空/非法回落默认，越界夹住（不做数值输入报错，界面上两句话写清）。
    #[test]
    fn gap_input_normalizes_and_clamps() {
        assert_eq!(normalize_gap_ms("250"), 250);
        assert_eq!(normalize_gap_ms(" 500 "), 500);
        assert_eq!(normalize_gap_ms("0"), 0, "0 = 不留静音，是合法值");
        assert_eq!(normalize_gap_ms(""), GAP_MS, "留空 = 默认");
        assert_eq!(normalize_gap_ms("abc"), GAP_MS, "非数字 = 默认（不阻断）");
        assert_eq!(
            normalize_gap_ms("9999"),
            templates::MAX_GAP_MS,
            "越界夹到上限"
        );
        assert_eq!(normalize_gap_ms("-5"), GAP_MS, "负数不是合法毫秒，回落默认");
    }

    /// 停顿改了**不需要重录**：已合成句照样复用，只是工程里的 gap 更新成新值。
    #[test]
    fn gap_change_reuses_done_sentences_and_updates_project() {
        let dir = temp_dir("gap-change");
        let mut old = saved_project("第一句。第二句。", None);
        save_done_project(&dir, &mut old);

        let loaded = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            None,
            None,
            GAP_MS + 250,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.reused, 0, "不改文本时走快路径，不报「继承」");
        assert_eq!(loaded.project.gap_ms, GAP_MS + 250);
        assert!(
            loaded.project.sentences.iter().all(|s| s.status == "done"),
            "停顿只影响拼装，已合成的句子不该作废"
        );
        assert_eq!(
            Project::load(&dir).unwrap().gap_ms,
            GAP_MS + 250,
            "新停顿要落盘，重开工程后仍是它"
        );
    }

    /// 兜底规则开关变了 → 旧音频一律不复用（spoken 文本变了，念的是另一套）。
    #[test]
    fn normalize_toggle_change_invalidates_reuse() {
        let dir = temp_dir("normalize-change");
        let mut old = saved_project("2024年第一句。第二句。", None);
        old.auto_normalize = false; // 旧工程是"原文照念"
        save_done_project(&dir, &mut old);

        // 开关打开 → 不能复用（spoken 文本会从"2024年"变成"二零二四年"）
        let loaded = load_resumable(
            &dir,
            "2024年第一句。第二句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.reused, 0);
        assert!(
            loaded
                .project
                .sentences
                .iter()
                .all(|s| s.status == "pending"),
            "开关变了就要重录，不能把旧读法留在成品里"
        );
        assert!(loaded.project.auto_normalize, "新工程要记上新开关");

        // 开关没变（还是 false）→ 快路径复用，句子仍是 done
        let mut old2 = saved_project("2024年第一句。第二句。", None);
        old2.auto_normalize = false;
        save_done_project(&dir, &mut old2);
        let same = load_resumable(
            &dir,
            "2024年第一句。第二句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            false,
            &empty_dict(),
        )
        .unwrap();
        assert!(
            same.project.sentences.iter().all(|s| s.status == "done"),
            "开关没变就照旧续作"
        );
        assert!(!same.project.auto_normalize);
    }

    /// 评审 MUST-1：稿件改一句后，未变句必须按文本复用，而不是整工程重录。
    #[test]
    fn edited_script_reuses_unchanged_done_sentences() {
        let dir = temp_dir("resume-edited-script");
        let mut old = saved_project("第一句。第二句。第三句。", None);
        save_done_project(&dir, &mut old);

        let loaded = load_resumable(
            &dir,
            "第一句。改过的第二句。第三句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        let loaded = loaded.project;
        assert_eq!(loaded.sentences.len(), 3);

        assert_eq!(loaded.sentences[0].status, "done");
        assert_eq!(loaded.sentences[1].status, "pending");
        assert_eq!(loaded.sentences[2].status, "done");
        assert_eq!(
            std::fs::read_to_string(dir.join("sentences/000.wav")).unwrap(),
            wav_sentinel(&old.sentences[0])
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sentences/002.wav")).unwrap(),
            wav_sentinel(&old.sentences[2])
        );
    }

    #[test]
    fn reordered_script_reuses_content_without_source_overwrite() {
        let dir = temp_dir("resume-reordered");
        let mut old = saved_project("甲句。乙句。丙句。", None);
        save_done_project(&dir, &mut old);

        let loaded = load_resumable(
            &dir,
            "丙句。甲句。丁句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.reused, 2);
        let loaded = loaded.project;
        assert_eq!(loaded.sentences[0].status, "done");
        assert_eq!(loaded.sentences[1].status, "done");
        assert_eq!(loaded.sentences[2].status, "pending");
        assert_eq!(
            std::fs::read_to_string(dir.join("sentences/000.wav")).unwrap(),
            wav_sentinel(&old.sentences[2])
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sentences/001.wav")).unwrap(),
            wav_sentinel(&old.sentences[0])
        );
    }

    #[test]
    fn duplicate_text_reuses_each_done_occurrence_once() {
        let dir = temp_dir("resume-duplicates");
        let mut old = saved_project("重复句。不同句。重复句。", None);
        save_done_project(&dir, &mut old);

        let loaded = load_resumable(
            &dir,
            "重复句。重复句。不同句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.reused, 3);
        let loaded = loaded.project;
        assert!(loaded.sentences.iter().all(|s| s.status == "done"));
        assert_eq!(
            std::fs::read_to_string(dir.join("sentences/000.wav")).unwrap(),
            wav_sentinel(&old.sentences[0])
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sentences/001.wav")).unwrap(),
            wav_sentinel(&old.sentences[2])
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sentences/002.wav")).unwrap(),
            wav_sentinel(&old.sentences[1])
        );
    }

    /// 评审 SHOULD-2：参考音色变了就不能复用旧声音的 wav。
    #[test]
    fn voice_ref_change_invalidates_all_sentence_reuse() {
        let dir = temp_dir("resume-voice-change");
        let mut old = saved_project("第一句。第二句。", None);
        save_done_project(&dir, &mut old);
        let new_voice = dir.join("new-voice.wav");
        std::fs::write(&new_voice, b"new-voice").unwrap();

        let loaded = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            Some(new_voice.display().to_string()),
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.reused, 0);
        let loaded = loaded.project;

        assert!(loaded.sentences.iter().all(|s| s.status == "pending"));
    }

    #[test]
    fn unreadable_voice_ref_is_reported_instead_of_reusing_old_audio() {
        let dir = temp_dir("resume-voice-missing");
        let missing = dir.join("missing.wav");
        let missing_path = missing.display().to_string();
        let mut old = saved_project("第一句。第二句。", Some(&missing_path));
        assert_eq!(old.voice_ref_hash, None, "模拟旧工程尚无内容哈希");
        save_done_project(&dir, &mut old);

        let err = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            Some(missing_path),
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap_err();
        assert!(err.contains("参考音频不可读"), "应明确报错: {err}");
        let on_disk = Project::load(&dir).unwrap();
        assert_eq!(on_disk.sentences[0].status, "done", "旧工程不得被覆盖");
    }

    /// duck 强度是语义档位（弱/中/强），映射到系数必须单调：越"强"压得越狠。
    #[test]
    fn duck_gain_is_monotonic_across_strength_levels() {
        let weak = duck_gain_for(0);
        let mid = duck_gain_for(1);
        let strong = duck_gain_for(2);
        assert!(
            weak > mid && mid > strong,
            "弱/中/强 必须逐渐压低：{weak} {mid} {strong}"
        );
        assert_eq!(duck_gain_for(99), mid, "越界档位回落中档，不 panic");
        assert!(strong > 0.0, "压到 0 等于把人声段 BGM 静音，不算「强」");
    }

    /// 独立生成 BGM 的时长档位：与 UI 的 ["30 秒","60 秒","120 秒","180 秒"] 同序，越界回落 60。
    #[test]
    fn standalone_bgm_seconds_follow_the_ui_options() {
        assert_eq!(bgm_standalone_seconds(0), 30.0);
        assert_eq!(bgm_standalone_seconds(1), 60.0);
        assert_eq!(bgm_standalone_seconds(2), 120.0);
        assert_eq!(bgm_standalone_seconds(3), 180.0);
        assert_eq!(bgm_standalone_seconds(99), 60.0);
        assert_eq!(
            bgm_standalone_seconds(-1),
            60.0,
            "负索引（未初始化）也回落默认"
        );
    }

    /// 结果区三行索引 → 三轨产物；越界按混音处理（与 UI 三行定义一致）。
    #[test]
    fn bgm_track_path_maps_rows_to_artifacts() {
        let a = BgmArtifacts {
            voice: Some(PathBuf::from("/x/voice.wav")),
            bgm: PathBuf::from("/x/bgm.wav"),
            mixed: Some(PathBuf::from("/x/mixed.wav")),
            srt: Some(PathBuf::from("/x/a.srt")),
            duration: 12.0,
            segments: 1,
        };
        assert_eq!(bgm_track_path(&a, 0), a.voice);
        assert_eq!(bgm_track_path(&a, 1), Some(a.bgm.clone()));
        assert_eq!(bgm_track_path(&a, 2), a.mixed);
        assert_eq!(bgm_track_path(&a, 7), a.mixed, "越界按混音处理");
        // 导出用的下标映射必须与列表一致（第 0 行是人声），两处一起改才不会错位
        assert_eq!(stem_for_track(0), export::Stem::Voice);
        assert_eq!(stem_for_track(1), export::Stem::Bgm);
        assert_eq!(stem_for_track(2), export::Stem::Mixed);
        assert_eq!(stem_for_track(7), export::Stem::Mixed, "越界按混音处理");

        // 独立生成：只有 BGM 轨 → 另外两行返回 None（UI 不显示，不给假路径）
        let only = BgmArtifacts {
            voice: None,
            bgm: PathBuf::from("/x/bgm.wav"),
            mixed: None,
            srt: None,
            duration: 60.0,
            segments: 2,
        };
        assert_eq!(bgm_track_path(&only, 0), None);
        assert_eq!(bgm_track_path(&only, 1), Some(only.bgm.clone()));
        assert_eq!(bgm_track_path(&only, 2), None);
    }

    #[test]
    fn invalidation_bumps_revision_and_clears_ready() {
        let state = Rc::new(UiState {
            project_ready: std::cell::Cell::new(true),
            project_revision: std::cell::Cell::new(7),
            ..UiState::default()
        });
        let (tx, rx) = channel();
        invalidate_worker_project(&tx, &state);
        assert!(!state.project_ready.get());
        assert_eq!(state.project_revision.get(), 8);
        assert!(matches!(rx.recv().unwrap(), Cmd::InvalidateProject));
    }

    #[test]
    fn stale_worker_messages_are_filtered_by_revision() {
        let stale = WorkerMsg {
            revision: 7,
            msg: Msg::Fatal("旧任务的错误".into()),
        };
        let current = WorkerMsg {
            revision: 8,
            msg: Msg::Fatal("新任务的错误".into()),
        };
        assert!(!worker_message_is_current(&stale, 8));
        assert!(worker_message_is_current(&current, 8));
    }

    #[test]
    fn model_change_invalidates_all_sentence_reuse() {
        let dir = temp_dir("resume-model-change");
        let mut old = saved_project("第一句。第二句。", None);
        save_done_project(&dir, &mut old);

        let loaded = load_resumable(
            &dir,
            "第一句。第二句。",
            "index-tts2",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.reused, 0);
        assert!(loaded
            .project
            .sentences
            .iter()
            .all(|s| s.status == "pending"));
    }

    #[test]
    fn same_path_voice_ref_content_change_invalidates_reuse() {
        let dir = temp_dir("resume-voice-content");
        let voice = dir.join("voice.wav");
        std::fs::write(&voice, b"voice-a").unwrap();
        let voice_path = voice.display().to_string();
        let mut old = saved_project("第一句。第二句。", Some(&voice_path));
        old.voice_ref_hash = Some(sha256_file(&voice).unwrap());
        save_done_project(&dir, &mut old);

        std::fs::write(&voice, b"voice-b").unwrap();
        let loaded = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            Some(voice_path),
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.reused, 0);
        assert!(loaded
            .project
            .sentences
            .iter()
            .all(|s| s.status == "pending"));
        assert_eq!(
            loaded.project.voice_ref_hash,
            Some(sha256_file(&voice).unwrap())
        );
    }

    /// 单篇导出走的是 `export::export_one`（批量导出同一个函数）：
    /// 这里钉住"未选格式不建目录 + 写了哪些文件"，模块内的批量用例再钉复制语义。
    #[test]
    fn export_one_reports_selection_and_writes_expected_files() {
        let dir = temp_dir("export-copies");
        let src_dir = dir.join("src");
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&src_dir).unwrap();
        let wav = src_dir.join("source.wav");
        let srt = src_dir.join("source.srt");
        std::fs::write(&wav, b"wav-data").unwrap();
        std::fs::write(&srt, b"srt-data").unwrap();

        match export::export_one("我的工程", &out_dir, &wav, Some(&srt), true, true) {
            export::ExportOutcome::Exported(path) => {
                assert_eq!(path.file_name().unwrap(), "我的工程.srt");
            }
            _ => panic!("应导出成功"),
        }
        assert_eq!(
            std::fs::read(out_dir.join("我的工程.wav")).unwrap(),
            b"wav-data"
        );
        assert_eq!(
            std::fs::read(out_dir.join("我的工程.srt")).unwrap(),
            b"srt-data"
        );

        let no_export_dir = dir.join("no-export");
        assert!(matches!(
            export::export_one("我的工程", &no_export_dir, &wav, Some(&srt), false, false),
            export::ExportOutcome::NoneSelected
        ));
        assert!(!no_export_dir.exists());
    }

    #[test]
    fn fatal_marks_running_rows_failed() {
        let rows: Rc<VecModel<Sentence>> = Rc::new(VecModel::from(vec![Sentence {
            index: 0,
            eval_label: "".into(),
            error_detail: "".into(),
            oom: false,
            no: 1,
            text: "测试句。".into(),
            status: "合成中".into(),
            duration: 1.0,
            start: 0.0,
            duration_label: "1.0s".into(),
            start_label: "0:00".into(),
            qa_enabled: false,
            qa_sorted: false,
            qa_scroll_y: 0.0,
        }]));
        mark_running_rows_failed(&rows);
        let row = rows.row_data(0).unwrap();
        assert_eq!(row.status, "失败");
        assert!(
            row.error_detail.is_empty(),
            "fatal 没有具体错误时不能伪造详情"
        );
    }

    /// 工程恢复走的就是 apply_project_to_rows（restore_project 与 Msg::ProjectLoaded
    /// 都调它）；完整 error status 必须带下去，重开后按钮不能消失。
    #[test]
    fn applying_project_status_keeps_oom_detail_for_restore() {
        let rows: Rc<VecModel<Sentence>> = Rc::new(VecModel::from(vec![test_sentence_row(0)]));
        let mut project = Project::new(
            "测试句。",
            "audio8-tts",
            GAP_MS,
            BASE_SEED,
            None,
            DEFAULT_PUNCTUATION,
            MAX_CHARS,
            |t| t.to_string(),
        );
        project.sentences[0].status =
            "error: oom: 内存不足（OOM）：释放模型内存（会卸载服务上所有已加载模型）".into();

        apply_project_sentence_statuses(&rows, &project);
        let row = rows.row_data(0).unwrap();
        assert!(row.oom, "恢复后 OOM 按钮不能消失：{row:?}");
        assert!(row.error_detail.contains("释放模型内存"), "{row:?}");
        assert!(row.error_detail.contains("所有已加载模型"), "{row:?}");
    }

    /// 源码级接线守卫：单测 helper 不足以证明生产 wrapper 真的走它。
    /// 若 apply_project_to_rows 改成不调用 helper，或自己把错误压成裸 error，
    /// 这条会红（对应第三轮复核的“只测 helper 不等于有隔离”）。
    #[test]
    fn apply_project_to_rows_routes_through_status_helper() {
        let source = src_lf(include_str!("main.rs"));
        let start = source
            .find("fn apply_project_to_rows(")
            .expect("生产恢复 wrapper 必须存在");
        let rest = &source[start..];
        let end = rest
            .find("\n}\n")
            .expect("apply_project_to_rows 必须有函数级结束")
            + 2;
        let wrapper = &rest[..end];
        assert!(
            wrapper.contains("apply_project_sentence_statuses(rows, project)"),
            "wrapper 必须走共享状态回灌，否则恢复详情会再次丢失"
        );
        assert!(
            !wrapper.contains("set_status(rows, i, \"error\")"),
            "wrapper 不得绕过 helper 把完整 error status 压成裸 error"
        );
    }

    #[test]
    fn unload_button_requires_confirmation_and_warns_global_effect() {
        let source = include_str!("../ui/dub_workbench.slint");
        assert!(
            source.contains("PixelPopconfirm"),
            "释放模型内存必须二次确认"
        );
        assert!(
            source.contains("这会卸载服务上所有模型的常驻内存"),
            "确认文案要说清全局误伤边界"
        );
        assert!(source.contains("确认释放"), "确认按钮文案要明确");
        assert!(
            source.contains("confirm => { root.unload-models(); }"),
            "只有确认回调才能发卸载请求"
        );
        assert!(
            !source.contains("clicked => { root.unload-models(); }"),
            "触发按钮本身不能直接发卸载请求"
        );
    }

    #[test]
    fn oom_error_detail_survives_status_mapping_and_finish_note() {
        let rows: Rc<VecModel<Sentence>> = Rc::new(VecModel::from(vec![Sentence {
            index: 0,
            eval_label: "".into(),
            error_detail: "".into(),
            oom: false,
            no: 1,
            text: "测试句。".into(),
            status: "待合成".into(),
            duration: 1.0,
            start: 0.0,
            duration_label: "1.0s".into(),
            start_label: "0:00".into(),
            qa_enabled: false,
            qa_sorted: false,
            qa_scroll_y: 0.0,
        }]));
        let body = r#"{"error":{"message":"cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}"#;
        let detail = aw_core::memory_shortfall_note(body).expect("测试 body 必须是结构化 OOM");
        let status = format!("error: oom: {detail}");
        set_status_by_project_index(&rows, 0, &status);
        let row = rows.row_data(0).unwrap();
        assert_eq!(row.status, "失败");
        assert!(row.oom, "OOM 行必须显示释放模型内存按钮：{row:?}");
        assert!(row.error_detail.contains("qwen3-asr"), "{row:?}");
        assert!(row.error_detail.contains("3.31 GiB"), "{row:?}");
        assert!(row.error_detail.contains("3.84 GiB"), "{row:?}");
        assert!(row.error_detail.contains("释放模型内存"), "{row:?}");

        let note = run_finished_note(1, false, 0, 2, 3, Some(&status));
        assert!(note.contains("2/3 句"), "{note}");
        assert!(note.contains("1 句失败"), "{note}");
        assert!(
            note.contains("qwen3-asr") && note.contains("3.84 GiB"),
            "{note}"
        );
        assert!(
            note.contains("释放模型内存") && note.contains("q4_0"),
            "{note}"
        );
        assert!(
            note.contains("继续合成") && note.contains("只重跑失败句"),
            "{note}"
        );
    }

    #[test]
    fn report_progress_emits_running_then_terminal_result() {
        let (tx, rx) = channel();
        let mut started = std::collections::HashSet::new();
        report_progress(&tx, 7, &mut started, 2, "第二句");
        report_progress(&tx, 7, &mut started, 2, "done 1.25s");

        let first = rx.recv().unwrap();
        assert_eq!(first.revision, 7);
        match first.msg {
            Msg::Sentence {
                index,
                status,
                duration,
            } => {
                assert_eq!(index, 2);
                assert_eq!(status, "running");
                assert_eq!(duration, None);
            }
            _ => panic!("第一条应为 running"),
        }
        let second = rx.recv().unwrap();
        assert_eq!(second.revision, 7);
        match second.msg {
            Msg::Sentence {
                index,
                status,
                duration,
            } => {
                assert_eq!(index, 2);
                assert_eq!(status, "done");
                assert_eq!(duration, Some(1.25));
            }
            _ => panic!("第二条应为 done"),
        }

        // 失败终态必须原样带走 `error: oom` 的可执行详情，不能再被压成裸 "error"。
        report_progress(
            &tx,
            7,
            &mut started,
            2,
            "error: oom: 内存不足：释放内存后继续",
        );
        let third = rx.recv().unwrap();
        match third.msg {
            Msg::Sentence { status, .. } => {
                assert_eq!(status, "error: oom: 内存不足：释放内存后继续");
            }
            _ => panic!("第三条应为 error"),
        }
    }

    #[test]
    fn file_stem_trims_and_defaults() {
        assert_eq!(file_stem(" 我的工程 "), "我的工程");
        assert_eq!(file_stem("  "), "未命名工程");
        assert_eq!(file_stem(".."), "未命名工程");
        assert_eq!(file_stem("../逃逸/名字"), ".._逃逸_名字");
        assert_eq!(file_stem("/tmp/out"), "_tmp_out");
        assert_eq!(file_stem(r"C:\tmp\x"), "C__tmp_x");
    }

    /// 排队期间被取消的任务，worker 取到它时必须**不执行**：只回报 TaskStarted +
    /// 对应的"已停止"，随后从取消登记表里摘除（表有界）。这是本批采纳
    /// Xmusic-splitter per-job registry 那条的核心不变量——只靠 UI 侧标记终态
    /// 是拦不住 worker 的，它照样会把已出队的任务跑一遍。
    #[test]
    fn cancelled_queued_song_is_not_executed_by_worker() {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let cancel = cancel::CancelRegistry::new();
        cancel.cancel(9);
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: std::env::temp_dir(),
                cancel,
            })
        });

        cmd_tx
            .send(Cmd::RunSong {
                revision: 3,
                task_id: 9,
                project_name: "不存在的工程".into(),
                model: "yue2".into(),
                lyrics: "词".into(),
                style: "风格".into(),
            })
            .unwrap();
        // 关掉发送端让 worker 退出：否则 msg_rx.iter() 永远等下去
        drop(cmd_tx);
        let msgs: Vec<Msg> = msg_rx.iter().map(|m| m.msg).collect();
        handle.join().unwrap();

        assert_eq!(
            msgs.len(),
            2,
            "取消掉的任务只该有\"开始\"与\"停止\"两条消息，不该有任何执行痕迹"
        );
        assert!(
            matches!(msgs[0], Msg::TaskStarted { task_id: 9 }),
            "第一条必须是 TaskStarted（台账据此把排队提升为运行，再立刻终态）"
        );
        assert!(
            matches!(msgs[1], Msg::SongStopped { task_id: 9 }),
            "第二条必须是 SongStopped：没有载入工程、没有请求服务端"
        );
    }

    /// 取消登记表被 worker 摘除后不再有残留（同一 id 复用时不会被误取消）。
    #[test]
    fn worker_takes_cancellation_mark_so_the_table_stays_bounded() {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let cancel = cancel::CancelRegistry::new();
        cancel.cancel(1);
        let worker_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: std::env::temp_dir(),
                cancel: worker_cancel,
            })
        });

        cmd_tx
            .send(Cmd::RunSong {
                revision: 3,
                task_id: 1,
                project_name: "不存在的工程".into(),
                model: "yue2".into(),
                lyrics: "词".into(),
                style: "风格".into(),
            })
            .unwrap();
        drop(cmd_tx);
        let msgs: Vec<Msg> = msg_rx.iter().map(|m| m.msg).collect();
        handle.join().unwrap();
        assert_eq!(msgs.len(), 2);

        // 表已回到空：worker 侧摘除，UI 侧那张表也应该查不到（共享同一张表）
        assert_eq!(cancel.len(), 0, "取消登记必须被摘除，否则长跑会无限增长");
    }

    /// 任务中心的行文案：排队中的条目必须带位次（"排队中"三字没法回答"还要等几个"）。
    #[test]
    fn task_rows_show_queue_position_for_pending_entries() {
        let mut q = tasks::TaskQueue::default();
        let running = q.start(tasks::TaskKind::Dub, "配音 · 34 句");
        q.progress(running, 0.5, "第 17/34 句");
        let queued = q.enqueue(tasks::TaskKind::Separation, "人声分离 · 频道口播");

        let rows = task_rows(&q, &TaskSlots::default());
        assert_eq!(rows.len(), 2);
        // 新建在前：排队的那条在最上面
        assert_eq!(rows[0].id, queued as i32);
        assert_eq!(
            rows[0].state.as_str(),
            "排队中 #1",
            "位次只数排队项，正在跑的不占位次"
        );
        assert_eq!(rows[1].state.as_str(), "运行中");
        assert_eq!(rows[1].progress, 0.5);
    }

    /// 状态栏 chip：运行中带排队数、空闲时带"下一个是谁"——不然用户不知道队列里还有东西。
    #[test]
    fn task_chip_reports_pending_queue() {
        let mut q = tasks::TaskQueue::default();
        let running = q.start(tasks::TaskKind::Dub, "配音 · 34 句");
        q.progress(running, 0.25, "第 9/34 句");
        q.enqueue(tasks::TaskKind::Separation, "人声分离 · 频道口播");
        let chip = task_chip_text(&q);
        assert!(
            chip.contains("25%") && chip.contains("另排队 1"),
            "运行中还要说清后面排着 1 条，实得 {chip}"
        );

        // 只有排队、没有在跑（例如worker 刚收完上一条）
        let mut only_queue = tasks::TaskQueue::default();
        only_queue.enqueue(tasks::TaskKind::Song, "音乐制作 · 生成歌曲");
        let chip = task_chip_text(&only_queue);
        assert!(
            chip.contains("排队 1") && chip.contains("音乐制作"),
            "空闲但有排队时说清排的是哪一类，实得 {chip}"
        );
    }

    /// 时长文案的三档边界：秒 / 分 / 时。都要按"已过去的整秒"写，不四舍五入。
    #[test]
    fn format_elapsed_switches_at_minute_and_hour_boundaries() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59m59s");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h00m");
        assert_eq!(format_elapsed(Duration::from_secs(7384)), "2h03m");
    }

    /// 任务中心副标题：排队报"已排队"，运行报"阶段 · 已运行"，终态只在收尾时冻结
    /// （不挂时长，由结果文案收尾）。
    #[test]
    fn task_detail_reports_wait_and_run_time() {
        let mut q = tasks::TaskQueue::default();
        let running = q.start(tasks::TaskKind::Song, "音乐制作 · 生成歌曲");

        let rows = task_rows(&q, &TaskSlots::default());
        assert!(
            rows[0].detail.starts_with("已运行 "),
            "还没报阶段时也该说明跑了多久，实得 {}",
            rows[0].detail
        );

        // worker 报阶段 → 阶段 + 已运行（歌曲没有百分比，这两样就是全部信息）
        assert!(q.note(running, "已请求服务端（yue2 整段生成，无中间进度）"));
        let rows = task_rows(&q, &TaskSlots::default());
        assert!(
            rows[0].detail.starts_with("已请求服务端") && rows[0].detail.contains(" · 已运行 "),
            "实得 {}",
            rows[0].detail
        );

        // 排队中的那条报"已排队"，不报运行时长
        let queued = q.enqueue(tasks::TaskKind::Separation, "人声分离 · 频道口播");
        let rows = task_rows(&q, &TaskSlots::default());
        assert_eq!(rows[0].id, queued as i32);
        assert!(
            rows[0].detail.starts_with("已排队"),
            "实得 {}",
            rows[0].detail
        );

        // 终态不挂时长
        q.finish(running, tasks::TaskState::Done, "成品 168.0s");
        let rows = task_rows(&q, &TaskSlots::default());
        let done = rows.iter().find(|r| r.id == running as i32).unwrap();
        assert_eq!(done.detail, "成品 168.0s");
    }

    /// 歌曲没有中间进度，状态栏 chip 不能写"0%"（看起来像卡死），改报已运行时长；
    /// 有真实进度的任务照旧显示百分比。
    #[test]
    fn task_chip_avoids_fake_zero_percent_for_song() {
        let mut song = tasks::TaskQueue::default();
        song.start(tasks::TaskKind::Song, "音乐制作 · 生成歌曲");
        let chip = task_chip_text(&song);
        assert!(!chip.contains('%'), "歌曲不该显示 0%：{chip}");
        assert!(chip.contains("已运行"), "实得 {chip}");

        let mut dub = tasks::TaskQueue::default();
        let id = dub.start(tasks::TaskKind::Dub, "配音 · 34 句");
        dub.progress(id, 0.35, "第 12/34 句");
        assert!(task_chip_text(&dub).contains("35%"));
    }

    /// 歌曲阶段消息必须**早于真正的请求、晚于客户端就绪**——复核抓到的正是"阶段报早了"：
    /// 目录创建或 make_client 失败时界面已经写着"已请求服务端"。
    /// 这里用假的 run 钉住顺序（真跑 generate_song 会打服务端），并顺带核 task_id 路由。
    #[test]
    fn song_stage_is_sent_right_before_the_request_runs() {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let ctx = WorkerCtx {
            rx: cmd_rx,
            tx: msg_tx,
            stop: Arc::new(AtomicBool::new(false)),
            sep_stop: Arc::new(AtomicBool::new(false)),
            eval_stop: Arc::new(AtomicBool::new(false)),
            projects_root: std::env::temp_dir(),
            cancel: cancel::CancelRegistry::new(),
        };

        let stage_already_sent = std::cell::Cell::new(false);
        let out = with_song_stage(&ctx, 42, "ace-step", || {
            // 闭包（=真正的请求）跑起来时，阶段消息必须已经在通道里
            stage_already_sent.set(matches!(
                msg_rx.try_recv(),
                Ok(WorkerMsg {
                    msg: Msg::TaskStage { task_id: 42, .. },
                    ..
                })
            ));
            "generated"
        });

        assert_eq!(out, "generated");
        assert!(
            stage_already_sent.get(),
            "阶段消息必须先于请求执行，且带对的 task_id"
        );
        drop(cmd_tx);
    }

    /// 阶段文案按模型分别写清"整段生成、无中间进度"，不能只有一句笼统的话；
    /// 有进度的种类与没进度的种类在状态栏 chip 上必须给出不同的东西。
    #[test]
    fn song_stage_note_names_the_model_and_progress_kinds_are_explicit() {
        assert!(song_stage_note("yue2").contains("yue2"));
        assert!(song_stage_note("ace-step").contains("ACE-Step"));
        for note in [song_stage_note("yue2"), song_stage_note("ace-step")] {
            assert!(
                note.contains("整段生成") && note.contains("无中间进度"),
                "实得 {note}"
            );
        }

        // 有进度的种类在 chip 上给百分比，没进度的给时长——两边都不能写成 0%
        for kind in [
            tasks::TaskKind::Dub,
            tasks::TaskKind::Bgm,
            tasks::TaskKind::Separation,
        ] {
            assert!(kind.reports_progress(), "{kind:?} 应该报进度");
            let mut q = tasks::TaskQueue::default();
            let id = q.start(kind, "t");
            q.progress(id, 0.4, "阶段");
            let chip = task_chip_text(&q);
            assert!(chip.contains("40%"), "{kind:?} 的 chip 实得 {chip}");
        }
        assert!(!tasks::TaskKind::Song.reports_progress());
    }

    /// S2 翻唱入口：音乐制作 Tab 的模式切换（0 = 文生歌、1 = 翻唱）。
    #[test]
    fn song_mode_switch_distinguishes_text2song_from_cover() {
        assert_eq!(SongMode::from_ui(0), SongMode::Text2Song);
        assert_eq!(SongMode::from_ui(1), SongMode::Cover);
        // 模式开关只有两档；任何未知下标都按文生歌兜底，不静默跳到翻唱
        assert_eq!(SongMode::from_ui(-1), SongMode::Text2Song);
        assert_eq!(SongMode::from_ui(2), SongMode::Text2Song);
    }

    /// S2 翻唱入口：主按钮可用性由**一份判据**按模式/输入投影。
    #[test]
    fn song_generate_availability_projects_mode_and_inputs() {
        // 文生歌：歌词 + 风格齐 → 可点
        assert!(!song_generate_blocked(
            SongMode::Text2Song,
            "词",
            "风格",
            None,
            false,
            false
        ));
        // 文生歌：缺歌词 / 缺风格 → 灰
        assert!(song_generate_blocked(
            SongMode::Text2Song,
            "",
            "风格",
            None,
            false,
            false
        ));
        assert!(song_generate_blocked(
            SongMode::Text2Song,
            "词",
            "",
            None,
            false,
            false
        ));
        // 翻唱：没选源音频（None 或空串）→ 灰
        assert!(song_generate_blocked(
            SongMode::Cover,
            "词",
            "风格",
            None,
            false,
            false
        ));
        assert!(song_generate_blocked(
            SongMode::Cover,
            "词",
            "风格",
            Some(""),
            false,
            false
        ));
        // 翻唱：源音频就绪 → 可点
        assert!(!song_generate_blocked(
            SongMode::Cover,
            "词",
            "风格",
            Some("/tmp/source.wav"),
            false,
            false
        ));
        // 排队中 → 可点（按钮变「取消排队」）
        assert!(!song_generate_blocked(
            SongMode::Cover,
            "",
            "",
            None,
            true,
            true
        ));
        // 运行中（busy 且还没轮到的窗口已关闭）→ 灰
        assert!(song_generate_blocked(
            SongMode::Text2Song,
            "词",
            "风格",
            None,
            true,
            false
        ));
    }

    /// S2 翻唱入口：未选源文件的禁用原因要能**做点什么**——点名源音频与两条链路。
    #[test]
    fn song_generate_refusal_names_missing_cover_source() {
        let reason =
            song_generate_refusal(SongMode::Cover, "词", "风格", None, false, false).unwrap();
        assert!(reason.contains("源音频"), "要说出缺的是源音频：{reason}");
        assert!(reason.contains("sheetsage2"), "要说清转谱链路：{reason}");
        assert!(reason.contains("yue2"), "要说清唱新词的引擎：{reason}");
    }

    /// 源码级守卫：音乐制作主按钮的可用性必须**消费 Rust 的投影**，不能在 Slint 里
    /// 重拼「歌词/风格是否为空、翻唱有没有源音频」（与 backup-blocked 同一条约定）。
    #[test]
    fn song_generate_button_consumes_projection_not_recomputed_in_slint() {
        let workbench = include_str!("../ui/song_workbench.slint");
        assert!(
            workbench.contains("enabled: !root.generate-blocked;"),
            "主按钮的 enabled 必须直接吃 Rust 投影 generate-blocked"
        );
        assert!(
            !workbench.contains("root.lyrics != \"\""),
            "不许在 Slint 里重拼歌词判据"
        );
        assert!(
            !workbench.contains("root.style != \"\""),
            "不许在 Slint 里重拼风格判据"
        );
    }

    /// 音色设计模型挑选：`task == "vdes"`、已知 family、离线且未被产品层排除。
    #[test]
    fn design_models_from_requires_vdes_offline_known_family() {
        let excluded = ServerModel {
            id: "qwen3-tts-voicedesign-bf16".into(),
            task: "vdes".into(),
            family: "qwen3_tts".into(),
            caps: model_capabilities::Capability {
                product_excluded: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let tts = ServerModel {
            id: "audio8-tts".into(),
            task: "tts".into(),
            family: "audio8_tts".into(),
            ..Default::default()
        };
        let streaming = ServerModel {
            id: "qwen3-tts-voicedesign-stream".into(),
            task: "vdes".into(),
            family: "qwen3_tts".into(),
            caps: model_capabilities::Capability {
                mode: "streaming".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let wrong_family = ServerModel {
            id: "not-a-design-model".into(),
            task: "vdes".into(),
            family: "mystery_tts".into(),
            ..Default::default()
        };
        let first = ServerModel {
            id: "qwen3-tts-voicedesign-q8_0".into(),
            task: "vdes".into(),
            family: "qwen3_tts".into(),
            caps: model_capabilities::Capability {
                mode: "offline".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let cfg = ServerConfig {
            host: None,
            port: None,
            min_free_memory_mb: None,
            models: vec![excluded, tts, streaming, wrong_family, first],
        };
        assert_eq!(
            design_models_from(Some(&cfg)).into_iter().next().as_deref(),
            Some("qwen3-tts-voicedesign-q8_0"),
            "只能选已确认支持 VoiceDesign 的离线 family"
        );
        let empty = ServerConfig {
            host: None,
            port: None,
            min_free_memory_mb: None,
            models: vec![],
        };
        assert_eq!(
            design_models_from(Some(&empty)).into_iter().next(),
            None,
            "没有 vdes 模型时不能凭空捏一个 id"
        );
    }

    /// 用真实 server.json 形状过一遍解析：VoiceDesign 选中后不再混进普通 TTS 下拉。
    #[test]
    fn parsed_voice_design_model_is_isolated_from_tts_picker() {
        let raw = r#"{
          "host": "127.0.0.1",
          "port": 8080,
          "models": [
            {
              "id": "audio8-tts",
              "family": "audio8_tts",
              "task": "tts",
              "path": "/tmp/audio8.gguf"
            },
            {
              "id": "qwen3-tts-voicedesign",
              "family": "qwen3_tts",
              "task": "vdes",
              "path": "/tmp/qwen3-vd.gguf",
              "mode": "offline"
            }
          ]
        }"#;
        let cfg = parse_server_config(raw).expect("server.json 形状应可解析");
        assert_eq!(
            design_models_from(Some(&cfg)).into_iter().next().as_deref(),
            Some("qwen3-tts-voicedesign")
        );
        let tts_ids: Vec<String> = tts_engine_voices(&cfg.models)
            .into_iter()
            .map(|v| v.name.to_string())
            .collect();
        assert_eq!(tts_ids, vec!["audio8-tts"], "vdes 不能被普通 TTS 下拉选中");
    }

    /// 设计模型下拉：候选顺序来自清单，显示名做产品化映射；清单外旧选择不会丢。
    #[test]
    fn design_picker_view_labels_known_models_and_keeps_stale_choice() {
        let models = vec![
            "breeze-tts-voicedesign".to_string(),
            "qwen3-tts-voicedesign".to_string(),
        ];
        let (ids, labels, index) = design_picker_view(&models, Some("qwen3-tts-voicedesign"));
        assert_eq!(ids, models);
        assert_eq!(
            labels,
            vec!["BreezeTTS 2 · 音色设计", "Qwen3-TTS 1.7B · 音色设计"]
        );
        assert_eq!(index, 1);

        let (ids, labels, index) = design_picker_view(&models, Some("gone-model"));
        assert_eq!(ids[0], "gone-model");
        assert_eq!(labels[0], "gone-model（不在当前清单）");
        assert_eq!(index, 0);
        assert_eq!(ids.len(), 3, "旧选择插到第 0 项，不静默换模型");
    }

    /// 生效设计模型：有保存值用保存值；否则取清单第一个（Breeze 在前就用 Breeze）。
    #[test]
    fn effective_design_model_honors_saved_choice_then_first_candidate() {
        let models = vec![
            "breeze-tts-voicedesign".to_string(),
            "qwen3-tts-voicedesign".to_string(),
        ];
        let default = AppSettings::default();
        assert_eq!(
            effective_design_model_from(&default, &models).as_deref(),
            Some("breeze-tts-voicedesign")
        );
        let picked = AppSettings {
            design_model: Some("qwen3-tts-voicedesign".into()),
            ..AppSettings::default()
        };
        assert_eq!(
            effective_design_model_from(&picked, &models).as_deref(),
            Some("qwen3-tts-voicedesign")
        );
        let stale = AppSettings {
            design_model: Some("gone-model".into()),
            ..AppSettings::default()
        };
        assert_eq!(
            effective_design_model_from(&stale, &models).as_deref(),
            Some("gone-model"),
            "清单外旧选择照用不改，服务拒绝才能暴露真因"
        );
    }

    /// 音色设计「生成」的可用性矩阵：模型 / 试听文本 / 音色描述 / busy 都要有说法。
    #[test]
    fn design_generate_refusal_requires_model_text_and_description() {
        let model = Some("qwen3-tts-voicedesign-q8_0");
        assert!(design_generate_refusal(model, "试听", "低沉男声", false, false).is_none());
        let no_model = design_generate_refusal(None, "试听", "低沉男声", false, false).unwrap();
        assert!(
            no_model.contains("VoiceDesign"),
            "缺模型要说清是哪个：{no_model}"
        );
        let no_text = design_generate_refusal(model, "   ", "低沉男声", false, false).unwrap();
        assert!(no_text.contains("试听文本"), "缺试听文本要点名：{no_text}");
        let no_desc = design_generate_refusal(model, "试听", "   ", false, false).unwrap();
        assert!(no_desc.contains("音色描述"), "缺描述要点名：{no_desc}");
        let busy = design_generate_refusal(model, "试听", "低沉男声", true, false).unwrap();
        assert!(busy.contains("生成中"), "生成中要给出等待说法：{busy}");
        let any = design_generate_refusal(model, "试听", "低沉男声", false, true).unwrap();
        assert!(any.contains("任务正在进行"), "有任务在飞要拒绝：{any}");
    }

    /// 音色设计失败**必须**复用 OOM 唯一文案入口：503 + insufficient_memory 时，
    /// 用户看到的是可执行下一步（释放模型内存 / 需要多少 / 可用多少），不是裸 503。
    #[test]
    fn design_voice_failure_preserves_oom_actionable_note() {
        let body = r#"{"error":{"message":"cannot load model 'qwen3-tts-12hz-1.7b-voicedesign-q8_0': estimated 4.06 GiB + 1024 MiB headroom exceeds available host memory (2.25 GiB)","type":"insufficient_memory"}}"#;
        let err = aw_core::ClientError::Server(503, body.to_string());
        let msg = design_voice_msg(
            "qwen3-tts-12hz-1.7b-voicedesign-q8_0".into(),
            "试听文本".into(),
            Err(err),
        );
        let Msg::DesignVoiceFailed { error, model } = msg else {
            panic!("合成失败应为 DesignVoiceFailed")
        };
        assert!(error.contains("内存不足（OOM）"), "{error}");
        assert!(error.contains("释放模型内存"), "{error}");
        assert!(error.contains("4.06 GiB"), "{error}");
        assert!(error.contains("2.25 GiB"), "{error}");
        assert!(model.contains("voicedesign"), "{model}");
    }

    /// 阳性对照：模型忙碌也是 503，不能因为 503 就说成内存不足。
    #[test]
    fn design_voice_busy_503_is_not_reported_as_oom() {
        let busy = aw_core::ClientError::Server(
            503,
            r#"{"error":{"message":"model is busy","type":"model_busy"}}"#.to_string(),
        );
        let msg = design_voice_msg("m".into(), "t".into(), Err(busy));
        let Msg::DesignVoiceFailed { error, .. } = msg else {
            panic!("合成失败应为 DesignVoiceFailed")
        };
        assert!(!error.contains("释放模型内存"), "{error}");
        assert!(!error.contains("内存不足（OOM）"), "{error}");
    }

    /// 源码级守卫：音色设计「生成」按钮必须吃 Rust 投影，且不再写「后端没有 voice design」。
    #[test]
    fn design_generate_button_consumes_projection_and_drops_stale_copy() {
        let extra = include_str!("../ui/extra_tabs.slint");
        assert!(
            extra.contains("enabled: !root.design-blocked;"),
            "生成按钮 enabled 必须直接吃 Rust 投影 design-blocked"
        );
        assert!(
            !extra.contains("audio.cpp 无 voice design"),
            "后端明明有 VoiceDesign（task vdes / options.instruction），不许再写「后端没有」"
        );
        assert!(
            !extra.contains("文本描述生成音色：后端未接入"),
            "文本描述生成音色已接入，不许再标未接入"
        );
    }

    /// 工程损坏时开始合成必须**中止**而不是静默重建：以前 `Project::load(dir).ok()` 会把它
    /// 当成"没有工程"，已合成句全变待合成且没有一句解释；更糟的是随后落盘会覆盖掉损坏
    /// 文件——那是唯一可人工恢复的现场。这里同时守住"缺文件仍是全新工程"这条回归。
    #[test]
    fn corrupt_project_aborts_the_run_and_keeps_the_file() {
        let dir = temp_dir("corrupt-project");
        let broken = br#"{"sentences": [{"index": 1,"#;
        std::fs::write(dir.join("project.json"), broken).unwrap();

        let err = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap_err();
        assert!(err.contains("工程文件损坏"), "实得 {err}");
        assert!(err.contains("project.json"), "要说清哪个文件：{err}");
        assert!(err.contains("没有自动重建"), "要明确不替用户做决定：{err}");
        assert_eq!(
            std::fs::read(dir.join("project.json")).unwrap(),
            broken,
            "损坏的工程文件必须原样留着（不能被新工程覆盖）"
        );

        // 回归：没有 project.json 的目录仍然按全新工程走，不能被这条守卫误伤
        let fresh = temp_dir("fresh-project");
        let loaded = load_resumable(
            &fresh,
            "第一句。第二句。",
            "audio8-tts",
            None,
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .unwrap();
        assert_eq!(loaded.project.sentences.len(), 2);
    }

    /// 任务中心的「停止」按钮判据：种类能力 × 状态 × 是否该 Tab 在飞那条。
    /// 反例是关键——重录任务也是 Dub 类，但它不能停（停止位属于配音合成那条）。
    #[test]
    fn can_stop_follows_kind_state_and_slot() {
        let mut q = tasks::TaskQueue::default();
        let dub = q.start(tasks::TaskKind::Dub, "配音 · 34 句");
        let redo = q.start(tasks::TaskKind::Dub, "重录第 3 句");
        let bgm = q.start(tasks::TaskKind::Bgm, "BGM · 生成并混音");
        let sep = q.enqueue(tasks::TaskKind::Separation, "人声分离 · 试音");
        let song = q.enqueue(tasks::TaskKind::Song, "音乐制作 · 生成歌曲");
        let slots = TaskSlots {
            dub: Some(dub),
            bgm: Some(bgm),
            sep: Some(sep),
            song: Some(song),
            eval: None,
            batch: Vec::new(),
        };
        let by_id = |id: u32| {
            task_rows(&q, &slots)
                .into_iter()
                .find(|r| r.id == id as i32)
                .unwrap()
        };

        assert!(by_id(dub).can_stop, "运行中的配音可以停");
        assert!(
            !by_id(redo).can_stop,
            "重录也是 Dub 类，但停止位不是它的：不能给按钮"
        );
        assert!(by_id(bgm).can_stop, "运行中的 BGM 可以停");
        assert!(by_id(sep).can_stop, "排队中的分离可以硬取消");
        assert!(by_id(song).can_stop, "排队中的歌曲可以取消");

        // 歌曲开始跑之后请求就中断不了：按钮必须消失，不给假停止
        let mut q2 = tasks::TaskQueue::default();
        let song2 = q2.enqueue(tasks::TaskKind::Song, "歌曲");
        q2.promote(song2);
        let slots2 = TaskSlots {
            song: Some(song2),
            ..TaskSlots::default()
        };
        assert!(
            !task_rows(&q2, &slots2)[0].can_stop,
            "运行中的歌曲没有停止手段（服务端不支持取消）"
        );

        // 分离跑起来之后仍可停（协作式：整轮跑完丢弃结果）
        let mut q3 = tasks::TaskQueue::default();
        let sep3 = q3.enqueue(tasks::TaskKind::Separation, "分离");
        q3.promote(sep3);
        let slots3 = TaskSlots {
            sep: Some(sep3),
            ..TaskSlots::default()
        };
        assert!(
            task_rows(&q3, &slots3)[0].can_stop,
            "运行中的分离可协作停止"
        );

        // 质检：排队中可硬取消、运行中可协作停止（句间检查）
        let mut q5 = tasks::TaskQueue::default();
        let eval_queued = q5.enqueue(tasks::TaskKind::Eval, "质检");
        let slots5 = TaskSlots {
            eval: Some(eval_queued),
            ..TaskSlots::default()
        };
        assert!(task_rows(&q5, &slots5)[0].can_stop, "排队中的质检可取消");
        q5.promote(eval_queued);
        assert!(
            task_rows(&q5, &slots5)[0].can_stop,
            "运行中的质检可协作停止（当前句转写完就停）"
        );

        // 终态条目（含失败）一律不给停止按钮
        let mut q4 = tasks::TaskQueue::default();
        let failed = q4.start(tasks::TaskKind::Bgm, "BGM");
        q4.finish(failed, tasks::TaskState::Failed, "服务端 500");
        let slots4 = TaskSlots {
            bgm: Some(failed),
            ..TaskSlots::default()
        };
        assert!(!task_rows(&q4, &slots4)[0].can_stop, "失败条目不能停");
    }

    /// 批量条目在任务中心的可停性：**只有排队中**能停那一条（硬取消、不消耗算力）。
    /// 正在跑的那条要停就等于停整批——语义不同，不在任务中心摆一个同名按钮。
    #[test]
    fn batch_rows_are_stoppable_only_while_pending() {
        let mut q = tasks::TaskQueue::default();
        let pending = q.enqueue(tasks::TaskKind::Dub, "配音 · 甲（批量）");
        let running = q.enqueue(tasks::TaskKind::Dub, "配音 · 乙（批量）");
        q.promote(running);
        let slots = TaskSlots {
            batch: vec![pending, running],
            ..Default::default()
        };
        let rows = task_rows(&q, &slots);
        let by_id = |id: u32| rows.iter().find(|r| r.id == id as i32).unwrap();
        assert!(by_id(pending).can_stop, "排队中的批量条目要能单独取消");
        assert!(
            !by_id(running).can_stop,
            "运行中的批量条目不在任务中心停（那是停整批，按钮在批量面板）"
        );
        assert_eq!(
            stop_target(pending, tasks::TaskKind::Dub, &slots),
            Some(StopTarget::Batch),
            "批量条目要先按批量表认领，不能落进普通配音槽位"
        );
    }

    /// 排队中就被取消的批量条目：worker 轮到它时**不加载模型、不合成**，
    /// 直接回报 skipped，并且整批继续往下跑。这条不需要服务端（压根不该发出请求）。
    ///
    /// 两条取消都必须在 `cmd_tx.send` **之前**登记：worker 线程 spawn 后随时可能
    /// 消费消息，若把 42 的取消放在发送之后，就与 worker 形成竞态——worker 一旦
    /// 抢先处理到 42，它会当成正常条目去 `make_client()`，在干净 runner 上（没有
    /// server.json）必然 Failed，测试变成 flaky（曾经红过）。
    #[test]
    fn cancelled_queued_batch_items_are_skipped_without_work() {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let cancel = cancel::CancelRegistry::new();
        cancel.cancel(41);
        cancel.cancel(42);
        let worker_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: std::env::temp_dir(),
                cancel: worker_cancel,
            })
        });

        cmd_tx
            .send(Cmd::RunBatch {
                revision: 1,
                model: "audio8-tts".into(),
                voice_ref: None,
                voice_ref_text: None,
                gap_ms: GAP_MS,
                auto_normalize: true,
                dict: empty_dict(),
                items: vec![
                    BatchCmdItem {
                        task_id: 41,
                        name: "被取消的甲".into(),
                        script: "第一句。".into(),
                    },
                    BatchCmdItem {
                        task_id: 42,
                        name: "也被取消的乙".into(),
                        script: "第二句。".into(),
                    },
                ],
            })
            .unwrap();

        let mut skipped_ids = Vec::new();
        let summary = loop {
            let m = msg_rx.recv().expect("worker 应有消息");
            match m.msg {
                Msg::TaskStarted { task_id } => skipped_ids.push(("started", task_id)),
                Msg::BatchItemProgress { .. } => {
                    panic!("取消过的条目不该产生任何进度（没加载模型就对了）")
                }
                Msg::BatchItemDone {
                    task_id,
                    skipped,
                    error,
                    ..
                } => {
                    assert!(skipped, "取消过的条目必须报 skipped：{error:?}");
                    skipped_ids.push(("skipped", task_id));
                }
                Msg::BatchDone {
                    done,
                    failed,
                    skipped,
                    stopped,
                } => break (done, failed, skipped, stopped),
                _ => {}
            }
        };
        drop(cmd_tx);
        handle.join().unwrap();

        assert!(
            skipped_ids.contains(&("started", 41)) && skipped_ids.contains(&("skipped", 41)),
            "TaskStarted 之后直接收尾（队列靠前者把条目抬成运行中）：{skipped_ids:?}"
        );
        assert_eq!(summary, (0, 0, 2, false), "两条都被取消，整批没有失败");
        assert_eq!(cancel.len(), 0, "取走过的登记项不能留在表里（表要有界）");
    }

    /// 单句重录对**工程里存着的超长 voice_ref**（UI 路径只是它当时的投影）：
    /// 发起请求前按工程的 voice_ref 走迁移——31s 参考音被自动裁成 voice-trimmed
    /// 里的 15s 副本、voice_ref 与新 hash 一起落盘，随后照常走到 make_client 之后
    /// 的阶段（**没被时长拦死**）。
    ///
    /// 工程故意给空稿（0 句）：这样无论本机有没有真服务端，都不会发出真实 HTTP
    /// 请求——无服务时报"没有服务地址"、有服务时 redo 报"没有第 0 句"，两条都证明
    /// 已经过了时长判据（旧护栏会在这里直接拦成时长文案）。
    ///
    /// 阳性对照（实测过）：把 `Cmd::Redo` 分支里的 migrate 调用删掉，本用例
    /// 收到的 RedoDone 后工程里 voice_ref 仍是 31s 原路径 → 立刻红。
    #[test]
    fn redo_migrates_overlong_reference_before_any_request() {
        let root = temp_dir("redo-migrate");
        // 31s 真实 wav（8kHz 单声道 16bit 静音，约 0.5MB）
        let ref_wav = root.join("ref-31s.wav");
        std::fs::write(&ref_wav, test_wav_bytes(31.0, None)).unwrap();
        let project = saved_project("", Some(ref_wav.to_str().unwrap()));

        // OpenProject 把工程灌进 worker（模拟启动恢复：工程里存着超长路径）
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let worker_root = root.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: worker_root,
                cancel: cancel::CancelRegistry::new(),
            })
        });
        cmd_tx
            .send(Cmd::OpenProject {
                revision: 1,
                dir: root.clone(),
                project,
            })
            .unwrap();
        cmd_tx
            .send(Cmd::Redo {
                revision: 1,
                index: 0,
            })
            .unwrap();
        let m = msg_rx.recv().expect("redo 应回报终态");
        let Msg::RedoDone { index, error } = m.msg else {
            panic!("应是 RedoDone")
        };
        assert_eq!(index, 0);
        // 两条出路都必须证明"过了时长判据"：无服务报 make_client、有服务报空稿没这句
        let err = error.expect("空稿 redo 必须有终态错误");
        assert!(
            err.contains("没有服务地址") || err.contains("没有第 0 句"),
            "必须是 make_client 之后阶段的错误（不能被时长文案拦死）：{err}"
        );
        assert!(
            !err.contains("上限") && !err.contains("15 秒"),
            "不能是时长拦截文案：{err}"
        );

        // 迁移已落盘：工程里 voice_ref 换成 voice-trimmed 的 15s 副本、hash 同步更新
        let on_disk = aw_core::Project::load(&root).expect("迁移后工程应可读");
        let migrated = on_disk.voice_ref.expect("迁移后仍有 voice_ref");
        assert_ne!(
            migrated,
            ref_wav.to_string_lossy().into_owned(),
            "必须换成裁剪副本，不能还是原路径"
        );
        assert!(
            migrated.starts_with(voice_trimmed_dir().to_string_lossy().as_ref()),
            "副本必须落在 voice-trimmed：{migrated}"
        );
        assert!(migrated.ends_with("-15s.wav"), "{migrated}");
        let secs =
            aw_core::reference_duration_seconds(Path::new(&migrated)).expect("副本应是合法 wav");
        assert!(secs <= 15.0 + 1e-6, "迁移后必须 ≤15s：{secs}");
        assert_eq!(
            on_disk.voice_ref_hash,
            Some(sha256_file(Path::new(&migrated)).unwrap()),
            "hash 必须与新副本一致"
        );
        assert_ne!(
            on_disk.voice_ref_hash.as_deref(),
            Some(sha256_file(&ref_wav).unwrap().as_str()),
            "hash 必须换成新文件的，不是旧文件的"
        );

        drop(cmd_tx);
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 迁移失败（头声明超长但数据截断 → 自动裁剪读报错）必须退回红色拦截文案、
    /// 不发任何请求，且**不落盘**（工程里仍是原路径，用户手动修好后还能再试）。
    ///
    /// 阳性对照：这是"自动裁剪失败才拦"的拦支——上一条用例钉迁移支，本条钉拦支，
    /// 两条缺一不可（只绿一条说明另一条路断了）。
    #[test]
    fn redo_blocks_with_red_note_when_trim_fails() {
        let root = temp_dir("redo-trim-fail");
        let ref_wav = root.join("ref-trunc.wav");
        std::fs::write(&ref_wav, test_wav_bytes(31.0, Some(1.0))).unwrap();
        let project = saved_project("第一句。第二句。", Some(ref_wav.to_str().unwrap()));

        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let worker_root = root.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: worker_root,
                cancel: cancel::CancelRegistry::new(),
            })
        });
        cmd_tx
            .send(Cmd::OpenProject {
                revision: 1,
                dir: root.clone(),
                project,
            })
            .unwrap();
        cmd_tx
            .send(Cmd::Redo {
                revision: 1,
                index: 0,
            })
            .unwrap();
        let m = msg_rx.recv().expect("redo 应回报终态");
        let Msg::RedoDone { index, error } = m.msg else {
            panic!("应是 RedoDone")
        };
        assert_eq!(index, 0);
        let err = error.expect("裁剪失败必须拦");
        assert!(
            err.contains("31.0 秒，超过 15 秒上限"),
            "文案要带实际秒数与上限：{err}"
        );
        assert!(
            err.contains("自动取前 15 秒失败"),
            "文案要说明自动裁剪失败：{err}"
        );
        assert!(err.contains("裁到 15 秒内再合成"), "文案要带动作：{err}");
        // 迁移失败不落盘：工程文件不该被改写出来（手动修好原文件后还能再试）
        assert!(
            !root.join("project.json").exists(),
            "裁剪失败不能把迁移写进工程"
        );

        drop(cmd_tx);
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// prepare 的应用侧行为：20s → voice-trimmed 副本 + 信息提示；10s → 原样；
    /// 读不出 → fail-open 原样；头声明超长但数据截断 → 红色拦截文案（含原因）。
    #[test]
    fn prepare_trims_overlong_and_passes_short_and_unreadable() {
        let dir = temp_dir("prepare-ref");

        // 20s → 自动裁剪副本，原文件不动
        let ref20 = dir.join("ref-20s.wav");
        std::fs::write(&ref20, test_wav_bytes(20.0, None)).unwrap();
        let prepared = prepare_reference_for_clone(&ref20.to_string_lossy(), Some("测试参考文本"))
            .expect("20s 应能自动裁剪");
        assert!(
            prepared.path.starts_with(voice_trimmed_dir()),
            "副本必须落在 voice-trimmed：{}",
            prepared.path.display()
        );
        assert_ne!(prepared.path, ref20);
        let note = prepared.note.expect("裁剪要有信息提示");
        assert!(note.contains("20.0 秒 → 将使用前 15 秒"), "{note}");
        assert!(note.contains("原文件未改动"), "{note}");
        let secs = aw_core::reference_duration_seconds(&prepared.path).expect("副本应是合法 wav");
        assert!((secs - 15.0).abs() < 0.01, "副本时长应约 15s：{secs}");
        assert_eq!(
            std::fs::metadata(&ref20).unwrap().len(),
            44 + 20 * 8000 * 2,
            "原文件必须原封不动"
        );

        // 10s → 原样返回
        let ref10 = dir.join("ref-10s.wav");
        std::fs::write(&ref10, test_wav_bytes(10.0, None)).unwrap();
        let short = prepare_reference_for_clone(&ref10.to_string_lossy(), Some("测试参考文本"))
            .expect("10s 放行");
        assert_eq!(short.path, ref10);
        assert!(short.note.is_none(), "未裁剪就不该有提示");

        // 读不出时长 → fail-open 原样
        let garbage = dir.join("ref-garbage.mp3");
        std::fs::write(&garbage, b"not audio").unwrap();
        let open = prepare_reference_for_clone(&garbage.to_string_lossy(), Some("测试参考文本"))
            .expect("读不出时长必须 fail-open");
        assert_eq!(open.path, garbage);
        assert!(open.note.is_none());

        // 头声明 20s、数据截断 → 裁剪失败 → 红色拦截文案（含原因 + 手动建议）
        let truncated = dir.join("ref-trunc.wav");
        std::fs::write(&truncated, test_wav_bytes(20.0, Some(1.0))).unwrap();
        let err = prepare_reference_for_clone(&truncated.to_string_lossy(), Some("测试参考文本"))
            .expect_err("裁剪失败必须拦");
        assert!(err.contains("20.0 秒，超过 15 秒上限"), "{err}");
        assert!(err.contains("自动取前 15 秒失败"), "{err}");
        assert!(err.contains("裁到 15 秒内再合成"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧工程迁移（load_resumable）：工程里存着超长 voice_ref → 一次性迁移为
    /// 裁剪副本（voice_ref 与新 voice_ref_hash 都变、≤15s）并**落盘**——之后
    /// Cmd::Redo 等直接用工程 voice_ref 的路径才不会再拿超长文件打引擎。
    ///
    /// 阳性对照（实测过）：去掉 load_resumable 里的 migrate 调用，本用例的
    /// voice_ref 还是 20s 原路径 → 立刻红。
    #[test]
    fn load_resumable_migrates_overlong_voice_ref_and_persists() {
        let dir = temp_dir("migrate-ref");
        let ref20 = dir.join("ref-20s.wav");
        std::fs::write(&ref20, test_wav_bytes(20.0, None)).unwrap();
        let mut saved = saved_project("第一句。第二句。", Some(ref20.to_str().unwrap()));
        saved.voice_ref_hash = Some(sha256_file(&ref20).unwrap());
        // 第一句标记已合成：迁移后工程与输入仍匹配 ⇒ 走快路径 ⇒ 已合成句**不丢**
        saved.sentences[0].status = "done".into();
        saved.save(&dir).unwrap();

        let loaded = load_resumable(
            &dir,
            "第一句。第二句。",
            "audio8-tts",
            Some(ref20.to_string_lossy().into_owned()),
            None,
            GAP_MS,
            true,
            &empty_dict(),
        )
        .expect("旧工程应能迁移");
        assert_eq!(
            loaded.project.sentences[0].status, "done",
            "迁移不能破坏续作：输入与工程同源，裁剪后仍应命中复用判据（否则整轮重录）"
        );

        let migrated = loaded.project.voice_ref.expect("迁移后仍有 voice_ref");
        assert_ne!(
            migrated,
            ref20.to_string_lossy().into_owned(),
            "必须换成裁剪副本"
        );
        assert!(
            migrated.starts_with(voice_trimmed_dir().to_string_lossy().as_ref()),
            "副本必须落在 voice-trimmed：{migrated}"
        );
        let secs =
            aw_core::reference_duration_seconds(Path::new(&migrated)).expect("副本应是合法 wav");
        assert!(secs <= 15.0 + 1e-6, "迁移后必须 ≤15s：{secs}");
        assert_eq!(
            loaded.project.voice_ref_hash,
            Some(sha256_file(Path::new(&migrated)).unwrap()),
            "hash 必须与新副本一致"
        );

        // 已落盘：再读一次工程文件也是迁移后的路径（显式迁移，不是内存里顺手改）
        let on_disk = aw_core::Project::load(&dir).expect("迁移后工程应可读");
        assert_eq!(on_disk.voice_ref.as_deref(), Some(migrated.as_str()));
        assert_eq!(on_disk.voice_ref_hash, loaded.project.voice_ref_hash);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 挂死引擎（进程活着但 /health 不响应）的自愈结论必须如实报 Failed，
    /// 不装成 Started——否则状态栏会说"已重新拉起"而进程根本没动过。
    /// 用 `sleep` 当"活着的假引擎"（不监听任何端口，healthy() 必然 false），
    /// 句柄经 engine_supervisor 的 cfg(test) 注入缝塞进全局槽。
    #[test]
    fn throttled_heal_reports_hung_engine_as_failed_not_started() {
        // 本机真跑着用户服务（默认 8080）时，探活会直接通过 → 探测到就如实跳过
        // （detect-and-return，见 RULE_可达性 第 3 条）。
        let (base, _) = server_base_for_engine();
        if engine_supervisor::healthy(&base) {
            eprintln!("跳过：{base} 上有真服务在响应，挂死场景没法构造");
            return;
        }
        // 节流是全局状态：先清账，否则别的测试刚拉过一次就会把本条拦住
        engine_supervisor::reset_autostart_for_test();
        let sup = engine_supervisor::EngineSupervisor::with_child_for_test(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep 应可执行"),
        );
        {
            let mut slot = ENGINE_SUPERVISOR.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(sup);
        }
        let outcome = ensure_engine_serving_throttled().expect("节流应放行本次尝试");
        match outcome {
            engine_supervisor::StartOutcome::Failed(why) => {
                assert!(why.contains("不响应"), "要如实报挂死：{why}");
            }
            other => panic!("挂死引擎必须报 Failed，不能是 {other:?}"),
        }
        // 清槽即 Drop → stop() 收掉 sleep，不留孤儿进程
        *ENGINE_SUPERVISOR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 整批停止：**每篇都要收到终态**，一篇都不能留在"排队中"。
    ///
    /// 提交那一刻 stop 位已经是 true，等价于"刚提交就按了停止"。worker 不能直接
    /// break——留在台账里的 Pending 会让 `tasks_in_flight` 永远为真，
    /// 用户之后连单篇都提交不了（那几条再也没人回报）。
    #[test]
    fn stopped_batch_reports_every_item_as_terminal() {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(true)), // 一开始就是"已按停止"
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: std::env::temp_dir(),
                cancel: cancel::CancelRegistry::new(),
            })
        });

        cmd_tx
            .send(Cmd::RunBatch {
                revision: 1,
                model: "audio8-tts".into(),
                voice_ref: None,
                voice_ref_text: None,
                gap_ms: GAP_MS,
                auto_normalize: true,
                dict: empty_dict(),
                items: vec![
                    BatchCmdItem {
                        task_id: 51,
                        name: "停批甲".into(),
                        script: "第一句。".into(),
                    },
                    BatchCmdItem {
                        task_id: 52,
                        name: "停批乙".into(),
                        script: "第二句。".into(),
                    },
                ],
            })
            .unwrap();

        let mut terminal = Vec::new();
        let summary = loop {
            let m = msg_rx.recv().expect("worker 应有消息");
            match m.msg {
                Msg::BatchItemProgress { .. } => panic!("停止的批不该产生任何进度"),
                Msg::BatchItemDone {
                    task_id,
                    skipped,
                    note,
                    error,
                    ..
                } => {
                    assert!(skipped, "停止后没跑的篇目要报 skipped：{error:?}");
                    let note = note.expect("跳过的原因要如实说");
                    assert!(note.contains("停止"), "原因要说清是被停了：{note}");
                    terminal.push(task_id);
                }
                Msg::BatchDone {
                    done,
                    failed,
                    skipped,
                    stopped,
                } => break (done, failed, skipped, stopped),
                _ => {}
            }
        };
        drop(cmd_tx);
        handle.join().unwrap();

        assert_eq!(terminal, vec![51, 52], "两篇都要有终态，不能留 Pending");
        assert_eq!(summary, (0, 0, 2, true));
    }

    /// 批量消息必须**不受工程版本过滤**：批量跑着的时候用户可以继续改当前稿件
    /// （批量写的是别的工程目录），一改稿 `project_revision` 就 +1——第一版没把批量
    /// 消息放进 agnostic 名单，于是导入结果与逐篇进度全被静默丢掉，界面停在"合成本"、
    /// 台账停在运行中（复核抓到的阻塞项）。
    ///
    /// 这条测试对**每一条**批量消息都要过：以后再加批量消息，忘了进名单就会红。
    /// `BatchExportDone` 就是同一族的第二个例子（后台线程发回、revision 0）。
    #[test]
    fn batch_messages_survive_revision_changes() {
        let batch_msgs = vec![
            Msg::BatchScriptsPicked {
                pick: picker::Outcome::Cancelled,
            },
            Msg::BatchItemStarted {
                task_id: 1,
                index: 0,
                total: 2,
                name: "甲".into(),
            },
            Msg::BatchItemProgress {
                task_id: 1,
                index: 0,
                done: 1,
                total: 2,
            },
            Msg::BatchItemDone {
                task_id: 1,
                index: 0,
                name: "甲".into(),
                wav: None,
                srt: None,
                failed: 0,
                reused: 0,
                skipped: false,
                error: None,
                note: None,
            },
            Msg::BatchDone {
                done: 1,
                failed: 0,
                skipped: 1,
                stopped: false,
            },
            Msg::BatchExportDone {
                dir: PathBuf::from("/tmp/out"),
                outcome: export::BatchExportOutcome::NoneSelected,
            },
            Msg::DownloadUpdate(download::Snapshot {
                id: 1,
                label: "audio8-tts".into(),
                dest: PathBuf::from("/tmp/models/audio8.gguf"),
                state: download::State::Downloading,
                downloaded: 10,
                total: Some(100),
                note: String::new(),
            }),
        ];
        let names = [
            "BatchScriptsPicked",
            "BatchItemStarted",
            "BatchItemProgress",
            "BatchItemDone",
            "BatchDone",
            "BatchExportDone",
            "DownloadUpdate",
        ];
        assert_eq!(names.len(), batch_msgs.len());
        for (i, msg) in batch_msgs.into_iter().enumerate() {
            let m = WorkerMsg { revision: 0, msg };
            assert!(
                message_ignores_revision(&m.msg),
                "{} 必须在不过滤名单里",
                names[i]
            );
            assert!(
                should_handle_message(&m, 7),
                "改过稿（revision=7）之后 {} 也必须被处理",
                names[i]
            );
        }

        // 对照：按工程版本过滤的消息仍要过滤——别为了修批量把过滤整个放开
        let stale_sentence = WorkerMsg {
            revision: 0,
            msg: Msg::Sentence {
                index: 0,
                status: "done".into(),
                duration: Some(1.0),
            },
        };
        assert!(!message_ignores_revision(&stale_sentence.msg));
        assert!(
            !should_handle_message(&stale_sentence, 7),
            "旧 revision 的句级消息仍要丢掉"
        );
        assert!(
            should_handle_message(&stale_sentence, 0),
            "revision 对得上就该处理"
        );
    }

    /// 备份的两条消息同样不过滤：目录选择来自对话框，终态来自长 IO，两者都与稿件
    /// 版本无关。之前复核已经两次抓到"忘了加名单"，所以这里也钉一条。
    #[test]
    fn backup_messages_survive_revision_changes() {
        let msgs = vec![
            Msg::BackupDirPicked {
                pick: picker::Outcome::Picked("/tmp/备份盘".into()),
            },
            Msg::BackupDone {
                result: Ok(
                    "已备份 6 个文件（1.2KB）到 /tmp/备份盘/音频作坊备份-20260917-143012".into(),
                ),
            },
            // 失败路径也不能被当成"过期消息"丢掉，否则状态行停在正在备份
            Msg::BackupDone {
                result: Err("备份目标不能放在应用数据目录里面".into()),
            },
        ];
        let names = ["BackupDirPicked", "BackupDone(Ok)", "BackupDone(Err)"];
        assert_eq!(names.len(), msgs.len());
        for (i, msg) in msgs.into_iter().enumerate() {
            assert!(
                message_ignores_revision(&msg),
                "{} 必须在不过滤名单里",
                names[i]
            );
        }
    }

    /// 音色库「添加音频…」的消息来自系统文件框（后台线程、revision 0），
    /// 与工程版本无关：不在名单里就会被版本过滤静默丢掉，
    /// 界面停在「正在打开文件选择框…」。
    #[test]
    fn voice_files_picked_survives_revision_changes() {
        let msgs = vec![
            Msg::VoiceFilesPicked {
                pick: picker::Outcome::Picked(vec![PathBuf::from("/tmp/甲.wav")]),
            },
            // 取消也要能回来：否则状态行停在「正在打开文件选择框…」
            Msg::VoiceFilesPicked {
                pick: picker::Outcome::Cancelled,
            },
        ];
        let names = ["VoiceFilesPicked(Picked)", "VoiceFilesPicked(Cancelled)"];
        assert_eq!(names.len(), msgs.len());
        for (i, msg) in msgs.into_iter().enumerate() {
            assert!(
                message_ignores_revision(&msg),
                "{} 必须在不过滤名单里",
                names[i]
            );
        }
    }

    /// 引擎自愈的结论来自后台线程（revision 0、与工程版本无关）：
    /// 不在不过滤名单里，改一次稿就会把「服务已恢复」静默丢掉。
    #[test]
    fn engine_self_heal_survives_revision_changes() {
        let msgs = vec![
            Msg::EngineSelfHeal {
                note: Some("随包引擎已重新拉起，服务已恢复".into()),
            },
            Msg::EngineSelfHeal { note: None },
        ];
        let names = ["EngineSelfHeal(Some)", "EngineSelfHeal(None)"];
        assert_eq!(names.len(), msgs.len());
        for (i, msg) in msgs.into_iter().enumerate() {
            assert!(
                message_ignores_revision(&msg),
                "{} 必须在不过滤名单里",
                names[i]
            );
        }
    }

    /// 参考音频「选择…」的消息同理：文件框结果（后台线程、revision 0），
    /// 被版本过滤掉就是「选完没反应」。
    #[test]
    fn reference_audio_picked_survives_revision_changes() {
        let msgs = vec![
            Msg::ReferenceAudioPicked {
                pick: picker::Outcome::Picked("/tmp/参考.wav".into()),
            },
            Msg::ReferenceAudioPicked {
                pick: picker::Outcome::Cancelled,
            },
        ];
        let names = [
            "ReferenceAudioPicked(Picked)",
            "ReferenceAudioPicked(Cancelled)",
        ];
        assert_eq!(names.len(), msgs.len());
        for (i, msg) in msgs.into_iter().enumerate() {
            assert!(
                message_ignores_revision(&msg),
                "{} 必须在不过滤名单里",
                names[i]
            );
        }
    }

    /// 换参考音频的作废语义只该有一份正文：手改路径的回调与文件框结果都走
    /// `apply_voice_ref_change`——把作废逻辑抄两份必然漂移（复核的历史教训）。
    ///
    /// 源码级守卫：`on_voice_ref_changed` 回调体里不许再直接写 invalidate/reset，
    /// 必须调用共享函数；文件框结果也同一函数。
    #[test]
    fn reference_pick_uses_the_same_invalidation_path_as_manual_edit() {
        // src_lf + source_window：CRLF 检出下偏移与 LF 不同，且窗口末端可能落在
        // 多字节中文字符中间（Windows CI 实测 panic），统一归一 + 收缩。
        let src = src_lf(include_str!("main.rs"));
        assert!(
            src.contains("fn apply_voice_ref_change("),
            "共享作废函数必须在"
        );
        let body = source_window(&src, "fn apply_voice_ref_change(", 700);
        for step in [
            "clear_reference_text(",
            "invalidate_worker_project(",
            "reset_bgm(",
            "set_has_result(false)",
            "refresh_voice_labels(",
        ] {
            assert!(body.contains(step), "作废语义缺一步（{step}）：{body}");
        }
        assert!(
            body.contains("任务进行中：参考音暂不可改"),
            "共享函数要带与手工编辑同一条拒绝：{body}"
        );
        // 回调体调用共享函数，而不是自己再写一遍
        let cb_body = source_window(&src, "ui.on_voice_ref_changed(move", 300);
        assert!(
            cb_body.contains("apply_voice_ref_change("),
            "手改路径的回调必须走共享函数：{cb_body}"
        );
        assert!(
            !cb_body.contains("invalidate_worker_project"),
            "回调体不许再抄一遍作废：{cb_body}"
        );
        // 文件框结果也一样走共享函数
        let pick_body = source_window(&src, "Msg::ReferenceAudioPicked { pick } =>", 1200);
        assert!(
            pick_body.contains("set_voice_ref(") && pick_body.contains("apply_voice_ref_change("),
            "文件框结果必须走 set_voice_ref + apply_voice_ref_change：{pick_body}"
        );
    }

    /// 备份的起跑守卫：连点、以及和别的写盘动作撞车，都要被挡住（复核的阻塞项 2）。
    #[test]
    fn backup_refusal_blocks_double_click_and_concurrent_writers() {
        // 参数顺序：(ui_busy, ui_running, tasks_in_flight, batch_in_flight, backup_running)
        assert!(
            backup_refusal(false, false, false, false, false).is_none(),
            "空闲时该放行"
        );
        let again = backup_refusal(false, false, false, false, true).expect("已经在备份时必须拒绝");
        assert!(again.contains("备份还在进行"), "{again}");
        // 单篇配音：设的是 ui.running（不是 busy）——只查 busy 的老版本会漏
        assert!(
            backup_refusal(false, true, false, false, false).is_some(),
            "单篇配音在跑（ui.running）时不能备份"
        );
        assert!(
            backup_refusal(true, false, false, false, false).is_some(),
            "合成/拼装在跑时不能备份（会抄到写了一半的产物）"
        );
        // 歌曲 / 人声分离 / 质检：**只登记台账、不设全局 busy**，这是复核第二轮抓到的漏网
        let from_tasks = backup_refusal(false, false, true, false, false)
            .expect("台账里有任务在飞时必须拒绝（歌曲/分离写 projects/，正是备份要抄的）");
        assert!(from_tasks.contains("还有任务在跑"), "{from_tasks}");
        assert!(
            backup_refusal(false, false, false, true, false).is_some(),
            "批量在跑时不能备份（同上）"
        );
        // 已经在备份时，别的理由不该抢答——否则用户看到的是"合成中"而不是"备份中"
        let both = backup_refusal(true, true, true, true, true).unwrap();
        assert!(both.contains("备份还在进行"), "{both}");
    }

    /// 按钮的禁用态必须**就是**回调用那份判据的投影（复核第三轮的阻塞）。
    ///
    /// 只要按钮在 Slint 里另拼一套 busy（`busy || task-running` 之类），演示态就会
    /// 出现「按钮亮着却点不动」或「按钮灰着但判据说不忙」两种自相矛盾 —— 这条逐项钉住
    /// `backup_blocked` 与 `backup_refusal` 的等价：refusal 有理由 ⇔ 按钮该灰。
    #[test]
    fn backup_button_state_is_the_projection_of_the_same_refusal() {
        let cases = [
            (false, false, false, false, false),
            (true, false, false, false, false),
            (false, true, false, false, false),
            (false, false, true, false, false),
            (false, false, false, true, false),
            (false, false, false, false, true),
        ];
        for (busy, running, tasks, batch, backup) in cases {
            assert_eq!(
                backup_blocked(busy, running, tasks, batch, backup),
                backup_refusal(busy, running, tasks, batch, backup).is_some(),
                "按钮灰不灰必须与回调判据同源：\
                 busy={busy} running={running} tasks={tasks} batch={batch} backup={backup}"
            );
        }
        // 全空闲必须能点（别为了"同源"把按钮钉死）
        assert!(!backup_blocked(false, false, false, false, false));
    }

    /// 按钮的 `enabled` 只能来自 Rust 投影，**不许在 Slint 里另拼计数**。
    ///
    /// 这条是源码级守卫：第三轮复核抓到的正是"Slint 侧自己算 `task-running > 0`"
    /// 与 `tasks_in_flight` 各说各话。谁再把计数拼回 Slint，这条立刻红。
    #[test]
    fn backup_button_enabled_does_not_recompute_busyness_in_slint() {
        let src = src_lf(include_str!("../ui/dub_workbench.slint"));
        let button = source_window(
            &src,
            "text: root.backup-running ? \"备份中…\" : \"一键备份…\";",
            400,
        );
        assert!(
            button.contains("enabled: !root.backup-blocked;"),
            "按钮的 enabled 必须直接吃 Rust 投影 backup-blocked：{button}"
        );
        for forbidden in ["root.busy", "task-running", "task-pending", "tasks-busy"] {
            assert!(
                !button.contains(forbidden),
                "按钮不该自己拼忙判据（出现 `{forbidden}`）：{button}"
            );
        }
    }

    /// 检查更新的终态消息必须**不过滤 revision**（本仓已因漏这条被复核抓过两次）。
    ///
    /// 检查走后台线程、用 revision 0 发回来；一旦被版本过滤掉，界面就永远停在
    /// 「检查中…」并且按钮永久禁用。
    #[test]
    fn update_messages_survive_revision_changes() {
        let msgs = vec![
            Msg::UpdateCheckDone {
                result: Ok(update::UpdateCheck::Newer(update::Release {
                    version: "0.2.0".into(),
                    notes: "n".into(),
                    url: "https://example.com/r".into(),
                    sha256: None,
                    size: None,
                })),
            },
            Msg::UpdateCheckDone {
                result: Ok(update::UpdateCheck::UpToDate),
            },
            // 失败路径更不能当成"过期消息"丢掉，否则状态行停在正在检查
            Msg::UpdateCheckDone {
                result: Err("超时：30 秒内没读到数据".into()),
            },
        ];
        let names = [
            "UpdateCheckDone(Newer)",
            "UpdateCheckDone(UpToDate)",
            "UpdateCheckDone(Err)",
        ];
        assert_eq!(names.len(), msgs.len());
        for (i, msg) in msgs.into_iter().enumerate() {
            assert!(
                message_ignores_revision(&msg),
                "{} 必须在不过滤名单里",
                names[i]
            );
        }
    }

    /// 按钮的禁用态必须**就是**回调用那份判据的投影（同 backup 那条的姊妹）。
    /// 只要 Slint 里另拼一套条件，就会出现「亮着却点不动 / 灰着其实没事」。
    #[test]
    fn update_button_state_is_the_projection_of_the_same_refusal() {
        for running in [false, true] {
            assert_eq!(
                update_blocked(running),
                update_refusal(running).is_some(),
                "按钮灰不灰必须与回调判据同源：running={running}"
            );
        }
        // 空闲必须能点（别为了"同源"把按钮钉死）
        assert!(!update_blocked(false));
        // 注意：`update_refusal` 只吃 `update_running` 一个参数，签名本身就保证了
        // "只读检查不因为别的任务在跑而变灰"（与 backup_refusal 那套写盘守卫无关）。
    }

    /// 两个按钮的 `enabled` 只能来自 Rust 投影，不许在 Slint 里另拼条件（源码级守卫）。
    #[test]
    fn update_buttons_enabled_do_not_recompute_in_slint() {
        let src = src_lf(include_str!("../ui/dub_workbench.slint"));
        let block = source_window(
            &src,
            "text: root.update-running ? \"检查中…\" : \"检查更新\";",
            500,
        );
        assert!(
            block.contains("enabled: !root.update-blocked;"),
            "检查更新按钮的 enabled 必须直接吃 Rust 投影 update-blocked：{block}"
        );
        assert!(
            block.contains("enabled: root.update-has-release;"),
            "打开发布页按钮的 enabled 必须直接吃 Rust 投影 update-has-release：{block}"
        );
        for forbidden in [
            "root.busy",
            "task-running",
            "task-pending",
            "tasks-busy",
            "root.update-info !=",
        ] {
            assert!(
                !block.contains(forbidden),
                "按钮不该自己拼判据（出现 `{forbidden}`）：{block}"
            );
        }
    }

    /// 任务中心点「停止」的分派判定：**错误的 task_id 不会停当前任务**。
    /// 列表里可能同时有更早的同类任务（例如上一条已停止的配音），点它必须什么都不做。
    #[test]
    fn stop_target_requires_the_slot_to_match() {
        let slots = TaskSlots {
            dub: Some(10),
            bgm: Some(20),
            sep: Some(30),
            song: Some(40),
            eval: None,
            batch: Vec::new(),
        };
        assert_eq!(
            stop_target(10, tasks::TaskKind::Dub, &slots),
            Some(StopTarget::Dub)
        );
        assert_eq!(
            stop_target(20, tasks::TaskKind::Bgm, &slots),
            Some(StopTarget::Bgm)
        );
        assert_eq!(
            stop_target(30, tasks::TaskKind::Separation, &slots),
            Some(StopTarget::Separation)
        );
        assert_eq!(
            stop_target(40, tasks::TaskKind::Song, &slots),
            Some(StopTarget::Song)
        );
        assert_eq!(
            stop_target(
                50,
                tasks::TaskKind::Eval,
                &TaskSlots {
                    eval: Some(50),
                    ..TaskSlots::default()
                }
            ),
            Some(StopTarget::Eval)
        );
        assert_eq!(
            stop_target(50, tasks::TaskKind::Eval, &slots),
            None,
            "质检槽位为空时不该派发"
        );

        // 同类但 id 不是当前在飞那条（更早的任务）→ 不派发
        assert_eq!(stop_target(9, tasks::TaskKind::Dub, &slots), None);
        assert_eq!(stop_target(19, tasks::TaskKind::Bgm, &slots), None);
        assert_eq!(stop_target(29, tasks::TaskKind::Separation, &slots), None);
        assert_eq!(stop_target(39, tasks::TaskKind::Song, &slots), None);
        // 槽位为空时也不能派发（例如任务已经收尾）
        assert_eq!(
            stop_target(10, tasks::TaskKind::Dub, &TaskSlots::default()),
            None
        );
    }

    /// 质检摘要的三种情形要分开说：全失败**不能**说成"平均 0%"或"全部一致"
    /// （复核抓到过：0 句评上分时旧文案会同时给出这两个错误结论）。
    #[test]
    fn eval_summary_note_handles_all_failed_and_all_clean() {
        let all_failed = EvalSummary {
            model: "qwen3-asr".into(),
            percent: 0.0,
            scored: 0,
            asr_failed: 5,
            asr_error: None,
            worst: Vec::new(),
            scores: Vec::new(),
            persist_warning: None,
            report_path: None,
        };
        let note = eval_summary_note(&all_failed);
        assert!(note.contains("未能评分"), "{note}");
        assert!(note.contains('5') && note.contains("转写"), "{note}");
        assert!(!note.contains("平均"), "没评上分不该给平均分：{note}");
        assert!(!note.contains("全部一致"), "全失败不是全部一致：{note}");

        let clean = EvalSummary {
            model: "qwen3-asr".into(),
            percent: 100.0,
            scored: 3,
            asr_failed: 0,
            asr_error: None,
            worst: Vec::new(),
            scores: vec![
                (0, 100.0, Some("fun-asr".into())),
                (1, 100.0, Some("fun-asr".into())),
                (2, 100.0, Some("fun-asr".into())),
            ],
            persist_warning: None,
            report_path: None,
        };
        let note = eval_summary_note(&clean);
        assert!(note.contains("100.0%") && note.contains("3 句"), "{note}");
        assert!(note.contains("全部一致"), "{note}");

        let with_worst = EvalSummary {
            model: "audio8-asr".into(),
            percent: 96.4,
            scored: 57,
            asr_failed: 2,
            asr_error: None,
            scores: vec![
                (0, 100.0, Some("fun-asr".into())),
                (11, 92.3, Some("fun-asr".into())),
            ],
            persist_warning: Some("分数未写入工程：磁盘空间不足（需要 0.1 MB）".into()),
            report_path: Some(std::path::PathBuf::from("/tmp/示例工程/qa-report.md")),
            worst: vec![EvalIssue {
                index: 11,
                percent: 92.3,
                snippet: "…质检用【应为 例，读到 力】…".into(),
            }],
        };
        let note = eval_summary_note(&with_worst);
        assert!(note.contains("96.4%") && note.contains("57 句"), "{note}");
        assert!(note.contains("2 句转写失败"), "部分失败也要报出来：{note}");
        assert!(
            note.contains("第 12 句") && note.contains("92.3%") && note.contains("应为"),
            "最差句要给序号、分数与差异片段：{note}"
        );
        // 状态行念的必须是**这次实际用的**模型：非默认名要出现，且不能出现默认名
        assert!(note.contains("回读 audio8-asr"), "要念出实际模型：{note}");
        assert!(
            !note.contains("qwen3-asr"),
            "状态行不能写死默认模型名：{note}"
        );
        assert!(
            eval_summary_note(&clean).contains("回读 qwen3-asr"),
            "默认模型也要念出来"
        );

        let oom = EvalSummary {
            model: "qwen3-asr".into(),
            percent: 0.0,
            scored: 0,
            asr_failed: 1,
            asr_error: aw_core::memory_shortfall_note(
                r#"{"error":{"message":"cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}"#,
            ),
            worst: Vec::new(),
            scores: Vec::new(),
            persist_warning: None,
            report_path: None,
        };
        let note = eval_summary_note(&oom);
        assert!(note.contains("转写服务说明"), "{note}");
        assert!(
            note.contains("释放模型内存") && note.contains("q4_0"),
            "{note}"
        );
        assert!(
            note.contains("3.31 GiB") && note.contains("3.84 GiB"),
            "{note}"
        );
    }

    /// 句子行上的质检标签：低于阈值加 ⚠（阈值是启发式，不是质量门槛）。
    #[test]
    fn eval_label_marks_low_scores() {
        assert_eq!(eval_label(99.9), "可懂度 99.9%");
        assert_eq!(eval_label(95.0), "可懂度 95.0%", "正好等于阈值不加警告");
        assert_eq!(eval_label(94.9), "⚠ 可懂度 94.9%");
        assert_eq!(eval_label(0.0), "⚠ 可懂度 0.0%");
    }

    fn test_sentence_row(index: usize) -> Sentence {
        Sentence {
            index: index as i32,
            no: index as i32 + 1,
            text: format!("第 {} 句", index + 1).into(),
            status: "已合成".into(),
            duration: 1.0,
            start: index as f32,
            duration_label: "1.0s".into(),
            start_label: format!("0:0{index}").into(),
            eval_label: String::new().into(),
            error_detail: String::new().into(),
            oom: false,
            qa_enabled: false,
            qa_sorted: false,
            qa_scroll_y: 0.0,
        }
    }

    // ---------- 边合成边校听：全篇试听计划（listen_plan，streaming-preview §3 Phase 1） ----------

    /// listen_plan 用例的行构造：与 test_sentence_row 同源，只是状态可指定。
    fn plan_row(index: usize, status: &str) -> Sentence {
        let mut row = test_sentence_row(index);
        row.status = status.into();
        row
    }

    /// 工程里的逐句 wav 路径（与 listen_plan 的拼法一致）。
    fn sentence_wav(dir: &Path, index: usize) -> PathBuf {
        dir.join(format!("sentences/{index:03}.wav"))
    }

    /// 运行中、前缀非空：只连播已完成的前缀，文案点明下一句还没合成。
    #[test]
    fn listen_plan_running_plays_completed_prefix_only() {
        let dir = temp_dir("listen-running-prefix");
        let rows = vec![
            plan_row(0, "已合成"),
            plan_row(1, "已合成"),
            plan_row(2, "合成中"),
            plan_row(3, "待合成"),
        ];
        let plan = listen_plan(&rows, &dir, None, true);
        assert_eq!(
            plan.paths,
            vec![sentence_wav(&dir, 0), sentence_wav(&dir, 1)],
            "只播已完成的连续前缀"
        );
        assert_eq!(plan.duration, 2.0);
        let note = &plan.note;
        assert!(note.contains("合成中"), "{note}");
        assert!(note.contains("连播已完成的 2 句"), "{note}");
        assert!(note.contains("第 3 句还没合成"), "{note}");
    }

    /// 运行中、前缀空：还没有可播的句子，等第 1 句完成；第 2 句完成也不能跳着播。
    #[test]
    fn listen_plan_running_with_empty_prefix_explains_wait() {
        let dir = temp_dir("listen-running-empty");
        let rows = vec![plan_row(0, "待合成"), plan_row(1, "已合成")];
        let plan = listen_plan(&rows, &dir, None, true);
        assert!(plan.paths.is_empty(), "空前缀：paths 必须为空");
        let note = &plan.note;
        assert!(note.contains("还没有已合成的句子"), "{note}");
        assert!(note.contains("等第 1 句完成再试听"), "{note}");
    }

    /// 连续前缀遇到空洞即停（独立断言）：1 完成、2 未完成、3 完成 → 只播第 1 句。
    #[test]
    fn listen_plan_stops_at_first_hole_instead_of_skipping() {
        let dir = temp_dir("listen-hole");
        let rows = vec![
            plan_row(0, "已合成"),
            plan_row(1, "待合成"),
            plan_row(2, "已合成"),
        ];
        let plan = listen_plan(&rows, &dir, None, false);
        assert_eq!(
            plan.paths,
            vec![sentence_wav(&dir, 0)],
            "空洞后面的完成句不得入队（不静默跳过）"
        );
        assert_eq!(plan.duration, 1.0);
        let note = &plan.note;
        assert!(note.contains("已完成的 1 句连播"), "{note}");
        assert!(note.contains("第 2 句未合成"), "{note}");
    }

    /// 空闲、全部完成：保持原文案；乱序屏幕行也要回到工程 index 顺序。
    #[test]
    fn listen_plan_idle_all_done_keeps_original_note_and_sorts_by_index() {
        let dir = temp_dir("listen-all-done");
        // 质检排序会把屏幕行排乱：计划必须按工程 index 复原
        let rows = vec![
            plan_row(2, "已合成"),
            plan_row(0, "已合成"),
            plan_row(1, "已合成"),
        ];
        let plan = listen_plan(&rows, &dir, None, false);
        assert_eq!(
            plan.paths,
            vec![
                sentence_wav(&dir, 0),
                sentence_wav(&dir, 1),
                sentence_wav(&dir, 2),
            ]
        );
        assert_eq!(plan.duration, 3.0);
        assert_eq!(plan.note, "试听全篇（3 句连播，未拼间隙）");
    }

    /// 成品 final.wav 存在：只播成品、原文案「试听全篇成品」，句子行一概不看。
    #[test]
    fn listen_plan_prefers_existing_final_wav() {
        let dir = temp_dir("listen-assembled");
        let final_wav = dir.join("out/final.wav");
        std::fs::create_dir_all(final_wav.parent().unwrap()).unwrap();
        std::fs::write(&final_wav, b"x").unwrap();
        let assembled = AssembledInfo {
            wav: final_wav.clone(),
            duration: 12.5,
        };
        let rows = vec![plan_row(0, "待合成")];
        let plan = listen_plan(&rows, &dir, Some(&assembled), false);
        assert_eq!(plan.paths, vec![final_wav]);
        assert_eq!(plan.duration, 12.5);
        assert_eq!(plan.note, "试听全篇成品");
    }

    /// 成品记录还在但 final.wav 不在磁盘：不算成品，回落到前缀计划。
    #[test]
    fn listen_plan_missing_final_wav_falls_back_to_prefix() {
        let dir = temp_dir("listen-assembled-missing");
        let assembled = AssembledInfo {
            wav: dir.join("out/final.wav"), // 不落盘
            duration: 12.5,
        };
        let rows = vec![plan_row(0, "已合成"), plan_row(1, "已合成")];
        let plan = listen_plan(&rows, &dir, Some(&assembled), false);
        assert_eq!(
            plan.paths,
            vec![sentence_wav(&dir, 0), sentence_wav(&dir, 1)]
        );
        assert_eq!(plan.note, "试听全篇（2 句连播，未拼间隙）");
    }

    /// 排序只重排 UI 行，最差在前；恢复时必须回到工程真实 index 顺序。
    #[test]
    fn eval_sort_orders_worst_first_and_restores_project_order() {
        let rows = vec![
            test_sentence_row(0),
            test_sentence_row(1),
            test_sentence_row(2),
        ];
        let scores = HashMap::from([(0, 99.0), (1, 80.0), (2, 90.0)]);
        let sorted = sort_rows_for_eval(rows, &scores);
        assert_eq!(
            sorted.iter().map(|row| row.index).collect::<Vec<_>>(),
            vec![1, 2, 0]
        );

        // 分数失效后走这个函数（clear_eval_scores 的同一实现），回到工程原序。
        let restored = restore_project_order(sorted);
        assert_eq!(
            restored.iter().map(|row| row.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    /// 跳转不能拿显示行号当工程句号：排序后再按 index 找才是同一句。
    #[test]
    fn worst_sentence_lookup_uses_project_index_after_sort() {
        let rows = vec![
            test_sentence_row(0),
            test_sentence_row(1),
            test_sentence_row(2),
        ];
        let scores = HashMap::from([(0, 99.0), (1, 80.0), (2, 90.0)]);
        let sorted = sort_rows_for_eval(rows, &scores);
        assert_eq!(row_position_in_slice(&sorted, 1), Some(0));
        assert_eq!(row_position_in_slice(&sorted, 0), Some(2));
    }

    /// 排序后 worker 消息仍按工程 index 回填；时间轴也仍按工程顺序重算。
    #[test]
    fn sorted_rows_still_map_status_and_time_by_project_index() {
        let mut rows = vec![
            test_sentence_row(0),
            test_sentence_row(1),
            test_sentence_row(2),
        ];
        rows[0].duration = 1.0;
        rows[1].duration = 2.0;
        rows[2].duration = 3.0;
        let scores = HashMap::from([(0, 90.0), (1, 80.0), (2, 95.0)]);
        let model: Rc<VecModel<Sentence>> =
            Rc::new(VecModel::from(sort_rows_for_eval(rows, &scores)));

        set_status_by_project_index(&model, 0, "running");
        apply_eval_labels(&model, &scores);
        recompute_total(&model);

        let by_index = |index: usize| {
            model
                .row_data(row_position(&model, index).expect("index should map to a row"))
                .expect("row should exist")
        };
        assert_eq!(by_index(0).status, "合成中");
        assert_eq!(by_index(2).eval_label, "可懂度 95.0%");
        assert_eq!(by_index(0).start, 0.0);
        assert_eq!(by_index(1).start, 1.0);
        assert_eq!(by_index(2).start, 3.0);
    }

    /// 排序视角下，句级消息必须写回**它那一句**，而不是显示行第 N 行。
    ///
    /// 复核实测：把 `apply_sentence_msg` 里的 `set_status_by_project_index` 换回
    /// `set_status(rows, project_index, …)`，全量测试仍全绿——这条路径原本没有任何隔离。
    /// 阳性对照：改回按行号写，本用例必须变红。
    #[test]
    fn sentence_message_lands_on_project_index_row() {
        let rows = vec![
            test_sentence_row(0),
            test_sentence_row(1),
            test_sentence_row(2),
        ];
        let scores = HashMap::from([(0, 90.0), (1, 80.0), (2, 95.0)]);
        let model: Rc<VecModel<Sentence>> =
            Rc::new(VecModel::from(sort_rows_for_eval(rows, &scores)));
        let by_index = |index: usize| {
            model
                .row_data(row_position(&model, index).expect("index should map to a row"))
                .expect("row should exist")
        };
        // 前提：排序后第 0 行是工程第 1 句（否则"写错行"与本用例分不开）
        assert_eq!(model.row_data(0).unwrap().index, 1);
        assert_eq!(by_index(1).status, "已合成");

        apply_sentence_msg(&model, 0, "error", Some(2.5));

        assert_eq!(by_index(0).status, "失败", "必须写回工程第 0 句");
        assert_eq!(by_index(0).duration, 2.5);
        assert_eq!(
            by_index(1).status,
            "已合成",
            "第 0 行是工程第 1 句，不能被顺手改掉"
        );
    }

    /// `Msg::EvalDone` 的"自动选中"必须看**工程当前完整分数集**（与「跳到最差句」同源），
    /// 不能只看本轮有差异的句子：ASR 失败保留旧分的那句才是真正该先看的。
    ///
    /// 阳性对照：把判据缩成"只有前两行参与"（等价于 `summary.worst` 只看本轮差异），
    /// 本用例转红。
    #[test]
    fn eval_done_selection_uses_full_scores_not_round_worst() {
        let rows = vec![
            test_sentence_row(0),
            test_sentence_row(1),
            test_sentence_row(2),
        ];
        // 本轮只评上 0/1 两句；第 2 句 ASR 失败，保留旧分 10%
        let scores = HashMap::from([(0, 92.0), (1, 88.0), (2, 10.0)]);
        // 前提：两条推导确实分叉——本轮差异句里最差是第 1 句，完整分数集里最差是第 2 句
        let round_only = HashMap::from([(0, 92.0), (1, 88.0)]);
        assert_eq!(lowest_scored_index(&rows, &round_only), Some(1));
        assert_ne!(
            lowest_scored_index(&rows, &round_only),
            lowest_scored_index(&rows, &scores),
            "本用例必须落在两条推导分叉的那一侧，否则钉不住同源收敛"
        );

        let (index, note) = eval_done_selection(&rows, &scores);
        assert_eq!(index, Some(2), "自动选中要看当前完整分数集");
        assert_eq!(note, "·已选中第 3 句");
        // 与「跳到最差句」的判据同源
        assert_eq!(qa_action_view(&rows, &scores, false).worst_index, index);

        // 一句都没分 → 不清空选中，交给调用方（文案为空）
        assert_eq!(
            eval_done_selection(&rows, &HashMap::new()),
            (None, String::new())
        );
    }

    /// 真正的失效入口也要回原序，不只是排序函数本身。
    #[test]
    fn eval_invalidation_clears_scores_and_restores_project_order() {
        let rows = vec![
            test_sentence_row(0),
            test_sentence_row(1),
            test_sentence_row(2),
        ];
        let scores = HashMap::from([(0, 99.0), (1, 80.0), (2, 90.0)]);
        let model: Rc<VecModel<Sentence>> =
            Rc::new(VecModel::from(sort_rows_for_eval(rows, &scores)));
        let state = UiState {
            qa_sorted: std::cell::Cell::new(true),
            ..UiState::default()
        };
        // 走台账的统一写入点：分数与来源一起灌
        replace_eval_ledger(
            &state,
            &[
                (0, 99.0, Some("fun-asr".into())),
                (1, 80.0, Some("fun-asr".into())),
                (2, 90.0, Some("fun-asr".into())),
            ],
        );

        reset_eval_view(&model, &state);

        assert!(!state.qa_sorted.get());
        assert!(state.eval_scores.borrow().is_empty());
        assert!(
            state.eval_models.borrow().is_empty(),
            "失效要连来源一起清，否则会留下'有来源、没分数'的孤儿"
        );
        assert_eq!(
            rows_as_vec(&model)
                .iter()
                .map(|row| row.index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    /// 排序前若没有选中句子，不能把 -1 误当成第 0 行。
    #[test]
    fn qa_sort_does_not_remap_missing_selection_to_first_row() {
        let rows = vec![test_sentence_row(0), test_sentence_row(1)];
        assert_eq!(selected_project_index(-1, &rows), None);
        assert_eq!(selected_project_index(1, &rows), Some(1));
    }

    /// 排序按钮和跳转按钮共用一个可用性判据：只要当前分数有效就可用，
    /// 不依赖任何易失的“最差句缓存”（重启或单句失效后也走这条）。
    #[test]
    fn qa_actions_recover_from_live_scores_without_cached_worst() {
        let rows = vec![test_sentence_row(0), test_sentence_row(1)];
        let scores = HashMap::from([(0, 80.0), (1, 90.0)]);
        // 模拟重启/单句重录：没有旧 summary 缓存，只从存活分数算最差句。
        let view = qa_action_view(&rows, &scores, false);
        assert_eq!(view.worst_index, Some(0));
        assert_eq!(view.worst_row, Some(0));
        assert!(view.enabled);

        // 修掉最低分句后，剩余分数仍能让用户继续排、继续跳。
        let mut remaining = scores;
        remaining.remove(&0);
        let view = qa_action_view(&rows, &remaining, false);
        assert_eq!(view.worst_index, Some(1));
        assert_eq!(view.worst_row, Some(1));
        assert!(view.enabled);

        assert!(!qa_action_view(&rows, &HashMap::new(), false).enabled);
        assert!(!qa_action_view(&rows, &remaining, true).enabled);
    }

    /// 同分时跳到工程 index 最小的那句，排序也必须保持最小 index 在前。
    #[test]
    fn qa_ties_pick_the_smallest_project_index() {
        let rows = vec![
            test_sentence_row(2),
            test_sentence_row(1),
            test_sentence_row(0),
        ];
        let scores = HashMap::from([(0, 80.0), (1, 80.0), (2, 90.0)]);
        let sorted = sort_rows_for_eval(rows, &scores);
        assert_eq!(
            sorted.iter().map(|row| row.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(lowest_scored_index(&sorted, &scores), Some(0));
        let view = qa_action_view(&sorted, &scores, false);
        assert_eq!(view.worst_index, Some(0));
        assert_eq!(view.worst_row, Some(0));
    }

    /// 分数写不进工程时要如实说（分数有效但没落盘），别让用户以为下次打开还在。
    #[test]
    fn eval_summary_note_reports_persist_warning() {
        let summary = EvalSummary {
            model: "audio8-asr".into(),
            percent: 96.4,
            scored: 57,
            asr_failed: 0,
            asr_error: None,
            worst: vec![EvalIssue {
                index: 11,
                percent: 92.3,
                snippet: "…【应为 例，读到 力】…".into(),
            }],
            scores: vec![(11, 92.3, Some("fun-asr".into()))],
            persist_warning: Some("分数未写入工程：磁盘空间不足（需要 0.1 MB）".into()),
            report_path: Some(std::path::PathBuf::from("/tmp/示例工程/qa-report.md")),
        };
        let note = eval_summary_note(&summary);
        assert!(note.contains("92.3%"), "{note}");
        assert!(
            note.contains("分数未写入工程") && note.contains("磁盘空间不足"),
            "要如实报出没落盘：{note}"
        );
        assert!(
            note.contains("报告") && note.contains("qa-report.md"),
            "写成功时要告诉用户报告在哪：{note}"
        );
    }

    /// 从工程取质检台账：只接受「已合成」的句子（失败句即使文件里留着旧分也不贴），
    /// 并且**带上这份分的来源模型**——`None`（旧记录）要原样保留，不能顺手填成当前模型。
    /// 沿用旧分时必须带上**它当年的来源**，不能盖成本轮模型。
    ///
    /// 复核实测：这段原先内联在 worker 循环里，改成 `Some(model.clone())` 之后整仓
    /// **一条都不红**（零覆盖）——而失败的形态正是本批要消灭的：部分 ASR 失败 + 换了
    /// 回读模型时，界面会对那几句说「现有的分数就是 <本轮模型> 测的」，冒充匹配而且是假的。
    #[test]
    fn carried_over_scores_keep_their_original_source_model() {
        let mut prj = Project::new(
            "第一句。第二句。第三句。",
            "audio8-tts",
            GAP_MS,
            BASE_SEED,
            None,
            DEFAULT_PUNCTUATION,
            MAX_CHARS,
            |t| t.to_string(),
        );
        for s in prj.sentences.iter_mut() {
            s.status = "done".into();
        }
        // 第 0 句：本轮没评上（不在 already_scored 里），旧分是 fun-asr 当年测的
        prj.sentences[0].eval_percent = Some(70.0);
        prj.sentences[0].eval_model = Some("fun-asr".into());
        // 第 1 句：本轮评上了，来源是本轮的 audio8-asr
        prj.sentences[1].eval_percent = Some(88.0);
        prj.sentences[1].eval_model = Some("audio8-asr".into());
        // 第 2 句：没测过 → 不该被沿用
        prj.sentences[2].eval_percent = None;

        // 第 3 句：**失败句但文件里还留着旧分**（脏数据）→ 也不许被沿用。
        // 这条钉的是函数里"只认 done"那道 guard —— 复核实测：把它删掉整仓仍然全绿
        // （因为我这条用例原先把三句都设成了 done，压根没走到那个分支）。
        // 失败形态：失败句的遗留旧分被带进台账 → 会被算进"现有 N 句的分数是谁测的"，
        // 还会在那一行失败句上显示分数。
        prj.sentences.push(aw_core::dub::Sentence {
            index: 3,
            text: "第四句。".into(),
            spoken: "第四句。".into(),
            seed: BASE_SEED,
            duration: Some(1.0),
            start: None,
            status: "error: 模型没加载".into(),
            eval_percent: Some(33.0),
            eval_model: Some("fun-asr".into()),
        });

        let already = vec![(1usize, 88.0, Some("audio8-asr".to_string()))];
        let carried = carried_over_scores(&prj, &already);

        assert_eq!(
            carried.len(),
            1,
            "只有第 0 句该被沿用（第 3 句是失败句的遗留旧分，不该贴）：{carried:?}"
        );
        assert_eq!(carried[0].0, 0);
        assert!(
            !carried.iter().any(|(i, _, _)| *i == 3),
            "失败句的遗留旧分不许被沿用：{carried:?}"
        );
        assert_eq!(carried[0].1, 70.0);
        assert_eq!(
            carried[0].2.as_deref(),
            Some("fun-asr"),
            "沿用旧分要带它**当年的来源**"
        );
        assert_ne!(
            carried[0].2.as_deref(),
            Some("audio8-asr"),
            "不能把旧分冒充成本轮模型测的"
        );
    }

    #[test]
    fn eval_ledger_from_project_keeps_only_done_sentences_and_their_source() {
        let mut prj = Project::new(
            "第一句。第二句。第三句。",
            "audio8-tts",
            GAP_MS,
            BASE_SEED,
            None,
            DEFAULT_PUNCTUATION,
            MAX_CHARS,
            |t| t.to_string(),
        );
        prj.sentences[0].status = "done".into();
        prj.sentences[0].eval_percent = Some(99.0);
        prj.sentences[0].eval_model = Some("fun-asr".into());
        prj.sentences[1].status = "error: 模型没加载".into();
        prj.sentences[1].eval_percent = Some(50.0); // 脏数据：失败句不该贴出来
        prj.sentences[1].eval_model = Some("fun-asr".into());
        prj.sentences[2].status = "done".into();
        prj.sentences[2].eval_percent = Some(88.0);
        prj.sentences[2].eval_model = None; // 有分但无来源（本字段引入前的记录）

        let ledger = eval_ledger_from_project(&prj);
        assert_eq!(
            ledger,
            vec![(0, 99.0, Some("fun-asr".to_string())), (2, 88.0, None)],
            "失败句的旧分数不能贴；有分的那两句要连来源一起带出来"
        );
        assert!(
            ledger.iter().all(|(index, _, _)| *index != 1),
            "失败句（status=error）不该进台账"
        );
        assert_eq!(
            ledger[1].2, None,
            "旧工程的「来源未知」要原样保留——回灌时不能被填成当前模型"
        );
    }

    /// ② 分数与来源模型**同一次写入**：只写一半就会造出"分在、来源丢"的假未知。
    #[test]
    fn record_eval_score_writes_the_score_and_its_source_together() {
        let mut prj = Project::new(
            "第一句。第二句。",
            "audio8-tts",
            GAP_MS,
            BASE_SEED,
            None,
            DEFAULT_PUNCTUATION,
            MAX_CHARS,
            |t| t.to_string(),
        );
        record_eval_score(&mut prj, 1, 88.5, "fun-asr");
        assert_eq!(prj.sentences[1].eval_percent, Some(88.5));
        assert_eq!(prj.sentences[1].eval_model.as_deref(), Some("fun-asr"));
        assert_eq!(prj.sentences[0].eval_percent, None, "只写被点名的那一句");
        assert_eq!(prj.sentences[0].eval_model, None);
    }

    /// ② 「报告念的模型」与「工程里记的来源」必须来自**同一个入参**。
    ///
    /// 本仓反复抓的形态就是两处各写一份：报告写 A、工程记 B，用户看到两个说法。
    /// 钉两件事 —— ① app 侧写 `eval_percent` 只允许在 `record_eval_score` 里一处；
    /// ② 报告调用点传的必须是那个 `model` 变量（不是字面量、也不是另算一份）。
    #[test]
    fn qa_report_and_the_project_tag_read_the_same_run_model() {
        let src = include_str!("main.rs");
        let production = src.split("mod tests {").next().unwrap_or(src);
        let writes = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("eval_percent = Some("))
            .count();
        assert_eq!(
            writes, 1,
            "分数只允许在 `record_eval_score` 里一处写（现在 {writes} 处）——\
             分数与来源模型必须同进同出"
        );
        // 报告调用点：折掉空白再比，免得被缩进变化骗过
        let squashed = production.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            squashed.contains("qa_report_markdown( &project_name, &model,"),
            "报告必须念本次实际用的 model，不能写死模型名或另算一份"
        );
    }

    /// 台账的唯一写入点：分数与来源必须一起改。
    ///
    /// 分两个 map 是为了不动 `eval_scores` 的既有消费者（排序 / 最差句 / 行标签），
    /// 代价就是这条约束。单独改一份会造出"分数在、来源丢"，
    /// 而那种组合会被 `ledger_score_sources` 判成"来源未知"——界面开始说谎。
    #[test]
    fn eval_ledger_is_written_from_one_place() {
        let src = include_str!("main.rs");
        let production = src.split("mod tests {").next().unwrap_or(src);
        let writes = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("eval_models.borrow_mut()"))
            .count();
        assert_eq!(
            writes, 3,
            "`eval_models` 只允许在 replace / clear / drop 三个 helper 里写（现在 {writes} 处）"
        );
        for f in [
            "fn replace_eval_ledger(",
            "fn clear_eval_ledger(",
            "fn drop_eval_ledger(",
        ] {
            assert!(production.contains(f), "台账入口必须存在：{f}");
        }
    }

    /// ① 换过回读模型：必须**指名道姓**说出"这些分是谁测的"和当前模型，并给下一步。
    ///
    /// 这条就是本批的由来：以前只有一句"现有分数是上一个模型测的"，说不出是哪个。
    #[test]
    fn qa_source_note_names_both_models_and_the_next_step_when_the_model_changed() {
        let sources = ScoreSources {
            models: vec!["fun-asr".into()],
            unknown: 0,
            scored: 12,
        };
        let note = qa_source_note(&sources, "audio8-asr");
        assert!(note.contains("fun-asr"), "要说清是哪个模型测的：{note}");
        assert!(note.contains("audio8-asr"), "要说出当前模型：{note}");
        assert!(note.contains("12 句"), "要说清涉及几句：{note}");
        assert!(note.contains("重新质检"), "要给下一步：{note}");
        assert!(
            !note.contains("上一个模型"),
            "不能再是那句没有数据支撑的笼统说法：{note}"
        );
    }

    /// ③ 同一个模型重测 / 换了又换回来：来源一致就**不误报**。
    #[test]
    fn qa_source_note_is_quiet_when_the_source_is_the_current_model() {
        let note = qa_source_note(
            &ScoreSources {
                models: vec!["audio8-asr".into()],
                unknown: 0,
                scored: 7,
            },
            "audio8-asr",
        );
        assert!(note.contains("就是 audio8-asr 测的"), "{note}");
        assert!(
            !note.contains("建议重新质检"),
            "来源一致就不该劝重测：{note}"
        );
    }

    /// ④ 旧工程（没有 `eval_model`）= **来源未知**：既不能冒充"匹配"，也不能冒充"没测过"。
    ///
    /// 从**真实 JSON 形状**走一遍：反序列化旧工程 → 建台账 → 投影 → 文案。
    /// 不直接构造 `ScoreSources`，否则"旧工程读出来就是 None"这一段会被跳过。
    #[test]
    fn legacy_scores_are_unknown_source_not_matching_and_not_untested() {
        let legacy = r#"{
            "model": "audio8-tts",
            "gap_ms": 200,
            "base_seed": 1,
            "sentences": [
                {"index": 0, "text": "甲。", "spoken": "甲。", "seed": 1,
                 "status": "done", "eval_percent": 96.5}
            ]
        }"#;
        let prj: Project = serde_json::from_str(legacy).expect("旧工程要能读");
        assert_eq!(
            prj.sentences[0].eval_model, None,
            "旧工程没有这个字段 → 来源未知，不能凭空造一个模型名"
        );

        let state = UiState::default();
        replace_eval_ledger(&state, &eval_ledger_from_project(&prj));
        let sources = ledger_score_sources(&state);
        assert_eq!(
            sources,
            ScoreSources {
                models: vec![],
                unknown: 1,
                scored: 1
            },
            "有分但无来源 = 来源未知（既不是没测过、也不是匹配）"
        );

        let note = qa_source_note(&sources, "audio8-asr");
        assert!(note.contains("来源未知"), "{note}");
        assert!(!note.contains("没测过"), "来源未知 ≠ 没测过：{note}");
        assert!(
            !note.contains("就是 audio8-asr 测的"),
            "不能冒充匹配：{note}"
        );
        assert!(note.contains("建议重新质检"), "要给下一步：{note}");
    }

    /// 「没测过」与「来源未知」是两回事：前者 `eval_percent == None`，压根不进台账，
    /// 因此**不该有任何来源提示**（否则会把"还没跑过质检"说成"来源不明的旧记录"）。
    #[test]
    fn untested_sentences_produce_no_source_note_at_all() {
        let state = UiState::default();
        assert_eq!(ledger_score_sources(&state), ScoreSources::default());
        assert_eq!(
            qa_source_note(&ledger_score_sources(&state), "audio8-asr"),
            "",
            "一句都没测过时不该冒出来源说明"
        );
    }

    /// 混合来源（一部分本轮重测、一部分 ASR 失败沿用旧分）：两边都要说，不能只报一边。
    #[test]
    fn qa_source_note_reports_the_mixed_case() {
        let state = UiState::default();
        replace_eval_ledger(
            &state,
            &[
                (0, 99.0, Some("audio8-asr".into())),
                (1, 80.0, None), // 本轮没测到，沿用来源未知的旧分
            ],
        );
        let note = qa_source_note(&ledger_score_sources(&state), "fun-asr");
        assert!(note.contains("audio8-asr"), "要有已知来源：{note}");
        assert!(note.contains("来源未知"), "要有未知那部分：{note}");
        assert!(note.contains("fun-asr"), "当前模型也要出现：{note}");
        assert!(note.contains("重新质检"), "{note}");
    }

    /// ⑤ `eval_model` 的 serde 名漂移要能红。
    ///
    /// 参照 `server_json_keys_match_the_serde_field_names`：改名会**静默**退化成
    /// `None`（=来源未知），界面只会说"来源未知"，谁也不会发现是字段名写错了。
    #[test]
    fn eval_model_key_matches_the_serde_field_name() {
        let raw = r#"{
            "model": "audio8-tts",
            "gap_ms": 200,
            "base_seed": 1,
            "sentences": [
                {"index": 0, "text": "甲。", "spoken": "甲。", "seed": 1,
                 "status": "done", "eval_percent": 96.5, "eval_model": "fun-asr"}
            ]
        }"#;
        let prj: Project = serde_json::from_str(raw).expect("工程要能反序列化");
        assert_eq!(prj.sentences[0].eval_percent, Some(96.5));
        assert_eq!(
            prj.sentences[0].eval_model.as_deref(),
            Some("fun-asr"),
            "eval_model 必须按这个名字解析（改名就静默变成'来源未知'）"
        );
        // 落盘也要用同一个键名：roundtrip 一遍，不只看读的方向
        let again: Project = serde_json::from_str(&serde_json::to_string(&prj).unwrap())
            .expect("roundtrip 要能读回");
        assert_eq!(again.sentences[0].eval_model.as_deref(), Some("fun-asr"));
    }

    /// UI 只在 `done`（新 wav 已落盘）作废质检分数；running/error 时音频没变，分数仍成立。
    /// 这条与 aw-core 的"音频真的换了才清"是同一个语义，两边必须一致（复核抓过不一致）。
    #[test]
    fn redoing_a_sentence_invalidates_the_score() {
        assert!(
            sentence_message_invalidates_score("running"),
            "开始重做就作废"
        );
        assert!(sentence_message_invalidates_score("done"));
        assert!(sentence_message_invalidates_score("error"));
        assert!(sentence_message_invalidates_score("error: oom: 内存不足"));
        assert!(
            !sentence_message_invalidates_score("pending"),
            "没开始做就不动"
        );
    }

    /// 质检报告：逐句对照 + 汇总；竖线/换行要转义（否则表格破版）。
    #[test]
    fn qa_report_markdown_lists_every_sentence() {
        let rows = vec![
            EvalRow {
                index: 0,
                reference: "第一句测试。".into(),
                hypothesis: "第一句测试。".into(),
                percent: 100.0,
                snippet: String::new(),
            },
            EvalRow {
                index: 11,
                reference: "含 | 竖线与\n换行".into(),
                hypothesis: "含 | 竖线与\n换行（读错）".into(),
                percent: 92.3,
                snippet: "…【应为 例，读到 力】…".into(),
            },
        ];
        // 故意用**非默认**名：写死 qwen3-asr 的实现会在这里红
        let md = qa_report_markdown("示例工程", "fun-asr", &rows, 96.4, 2, 0);
        assert!(md.contains("# 质检报告 · 示例工程"), "{md}");
        assert!(
            md.contains("回读模型：fun-asr"),
            "要写明实际用的回读模型：{md}"
        );
        assert!(!md.contains("qwen3-asr"), "报告不能写死默认模型名：{md}");
        assert!(md.contains("平均可懂度：96.4%"), "{md}");
        assert!(
            md.contains("| 1 | 100.0% | 第一句测试。"),
            "逐句都要在表里：{md}"
        );
        assert!(md.contains("| 12 | 92.3% |"), "序号按 1 基展示：{md}");
        assert!(md.contains("【应为 例，读到 力】"), "差异片段要带上：{md}");
        assert!(
            !md.contains("含 | 竖线"),
            "单元格里的竖线必须转义，否则表格破版：{md}"
        );
        assert!(!md.contains("与\n换行"), "单元格里的换行也要清掉：{md}");
    }

    /// 一句都没评上分时，报告不能写"平均 0%"（与摘要同一口径）。
    #[test]
    fn qa_report_markdown_says_when_nothing_scored() {
        let md = qa_report_markdown("示例工程", "audio8-asr", &[], 0.0, 0, 5);
        assert!(md.contains("回读模型：audio8-asr"), "{md}");
        assert!(md.contains("未能评分"), "{md}");
        assert!(!md.contains("平均可懂度：0.0%"), "{md}");
        assert!(md.contains("转写失败 5"), "{md}");
        assert!(
            md.contains("句数：5"),
            "总句数要含转写失败的句子，不能写成 0：{md}"
        );
    }

    /// 真机（默认 ignored）：**质检阶段走真实 worker**，覆盖 GUI 用的那条命令通道
    /// （Cmd::RunEval → TaskStarted → EvalProgress… → EvalDone(report_path) → qa-report.md 落盘）。
    ///
    /// 合成准备故意直接用 aw-core（与 app 同一条链路）：走 `Cmd::Run` 会落到用户的
    /// `~/Documents/音频作坊/projects/<工程名>` 下，测试不该往用户真实数据目录里写东西。
    #[test]
    #[ignore = "需要本机 audiocpp_server + audio8-tts + audio8-asr"]
    fn worker_eval_writes_report_end_to_end() {
        let dir = std::env::temp_dir().join(format!("aw-worker-eval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1) 先用 aw-core 合成两句（app 用同一条链路），存成工程
        let base = std::env::var("AW_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
        let client = Client::new(&base);
        assert!(client.healthy(), "服务不可用：{base}");
        let mut project = Project::new(
            "第一句测试。第二句测试。",
            "audio8-tts",
            200,
            831001,
            None,
            aw_core::DEFAULT_PUNCTUATION,
            80,
            |t| t.to_string(),
        );
        let failed = project
            .synthesize(&client, &dir, None, |_, _| {})
            .expect("合成调用本身不应失败");
        assert_eq!(failed, 0, "不该有失败句");
        project.save(&dir).unwrap();

        // 2) 驱动 worker（与 GUI 同一条命令通道）
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: std::env::temp_dir(),
                cancel: cancel::CancelRegistry::new(),
            })
        });
        cmd_tx
            // 故意用一个**非默认**的回读模型：报告与状态行必须念这个，而不是写死的 qwen3-asr
            .send(Cmd::RunEval {
                revision: 0,
                task_id: 7,
                dir: dir.clone(),
                model: "audio8-asr".into(),
            })
            .unwrap();

        let mut progress_seen = 0usize;
        let report_path = loop {
            let m = msg_rx.recv().expect("worker 应有消息");
            match m.msg {
                Msg::EvalProgress { task_id: 7, .. } => progress_seen += 1,
                Msg::EvalDone {
                    task_id: 7,
                    summary,
                } => break summary.report_path,
                Msg::EvalFailed { task_id: 7, error } => panic!("质检失败：{error}"),
                _ => {}
            }
        };
        drop(cmd_tx);
        handle.join().unwrap();

        assert!(progress_seen >= 2, "应逐句报进度，实得 {progress_seen}");
        let path = report_path.expect("EvalDone 应带 qa-report.md 路径");
        let md = std::fs::read_to_string(&path).expect("报告应已落盘");
        assert!(md.contains("# 质检报告"), "{md}");
        assert!(md.contains("第一句测试。"), "报告里要有逐句参考文本：{md}");
        assert!(
            md.contains("回读模型：audio8-asr"),
            "报告要写实际用的模型：{md}"
        );
        assert!(!md.contains("qwen3-asr"), "报告不能写死默认模型名：{md}");
        eprintln!("报告：{}\n{md}", path.display());

        // 工程里的分数也落了盘（跨会话留存那条）
        let on_disk = Project::load(&dir).unwrap();
        assert!(
            on_disk.sentences.iter().all(|s| s.eval_percent.is_some()),
            "每句都应写入 eval_percent"
        );
        // ② 盘上那份分的**来源模型**必须与报告念的是同一个。
        // 本次刻意用 audio8-asr（非默认的 qwen3-asr）：写死默认名的实现会在这里红，
        // 而"报告与工程各写一份"的实现也会在这里红——两边必须同源。
        let sources: Vec<_> = on_disk
            .sentences
            .iter()
            .map(|s| s.eval_model.clone())
            .collect();
        assert!(
            sources.iter().all(|m| m.as_deref() == Some("audio8-asr")),
            "每句的 eval_model 都该是本次实际用的 audio8-asr，实得 {sources:?}"
        );
        assert!(
            md.contains("回读模型：audio8-asr"),
            "报告与工程标记同源：报告里的模型必须也是 audio8-asr：{md}"
        );
    }

    /// 16kHz / 单声道 / 16bit / 0.1s 静音：手搓一个最小合法 wav
    /// （主 crate 没有 hound 依赖，这里只为了给真机探针一个能读的输入）。
    fn tiny_silent_wav() -> Vec<u8> {
        let frames: u32 = 1600;
        let data_len = frames * 2;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&16_000u32.to_le_bytes());
        out.extend_from_slice(&32_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(44 + data_len as usize, 0);
        out
    }

    /// 真机（默认 ignored）：把**活的** 503 喂给真实的失败判据，看提示能不能照着做。
    ///
    /// 内存宽裕时 `qwen3-asr` 也装得上 —— 那本次就没触发，如实打印并返回，
    /// 不假装验证过（`RULE_可达性.md` 第 3 条：条件不满足走 detect-and-return）。
    #[test]
    #[ignore = "需要本机 audiocpp_server；内存紧时才会真实触发 insufficient_memory"]
    fn live_memory_shortfall_becomes_an_actionable_hint() {
        let base = std::env::var("AW_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
        let client = Client::new(&base);
        assert!(client.healthy(), "服务不可用：{base}");
        let wav = std::env::temp_dir().join(format!("aw-oom-probe-{}.wav", std::process::id()));
        std::fs::write(&wav, tiny_silent_wav()).unwrap();

        let candidates = asr_candidate_weights(read_server_config().as_ref());
        match client.asr_with("qwen3-asr", &wav) {
            Ok(text) => eprintln!("本次没触发：qwen3-asr 装得下（回读={text:?}），跳过降档验证"),
            Err(e) => match classify_asr_failure("qwen3-asr", &e, &candidates) {
                EvalAsrFailure::Fatal(msg) => {
                    eprintln!("原始 503：{e}");
                    eprintln!("组装出的提示：{msg}");
                    assert!(msg.contains("qwen3-asr"), "{msg}");
                    assert!(
                        msg.contains("GiB") || msg.contains("MiB"),
                        "要带需要/可用数字：{msg}"
                    );
                    assert!(
                        msg.contains("audio8-asr") || msg.contains("fun-asr"),
                        "要点名更小的候选：{msg}"
                    );
                    assert!(msg.contains("不会自动换"), "要写明不自动换：{msg}");
                }
                EvalAsrFailure::Counted => {
                    eprintln!("本次没触发内存不足（判成单句失败），服务回的是：{e}")
                }
            },
        }
    }

    /// 一句都没评上分时**也要**把"落盘告警/报告路径"带出来——旧写法在 scored==0 分支提前 return，
    /// 把报告路径和"报告没写进去"的告警一起吞了（复核指出）。
    #[test]
    fn eval_summary_note_keeps_warnings_when_nothing_scored() {
        let summary = EvalSummary {
            model: "audio8-asr".into(),
            percent: 0.0,
            scored: 0,
            asr_failed: 5,
            asr_error: None,
            worst: Vec::new(),
            scores: Vec::new(),
            persist_warning: Some("质检报告未写入：磁盘空间不足".into()),
            report_path: Some(std::path::PathBuf::from("/tmp/示例工程/qa-report.md")),
        };
        let note = eval_summary_note(&summary);
        assert!(note.contains("未能评分"), "{note}");
        assert!(note.contains("质检报告未写入"), "告警不能被吞：{note}");
        assert!(note.contains("qa-report.md"), "报告路径不能被吞：{note}");
    }

    /// 真机（默认 ignored）：**配音主链路走真实 worker**，覆盖 GUI 用的那条命令通道
    /// （Cmd::Run → ProjectLoaded → Sentence(done)… → RunDone → Cmd::Assemble → Assembled → wav+srt 落盘）。
    ///
    /// 与 eval 那条真机测试不同：这里不必先手工合成再塞工程——worker 自己按工程名拼路径，
    /// 只要把 `projects_root` 注入临时目录，就能真的跑一次"开始配音"而不碰
    /// `~/Documents/音频作坊/projects`（注入缝见 `WorkerCtx::projects_root`）。
    #[test]
    #[ignore = "需要本机 audiocpp_server + audio8-tts"]
    fn worker_dub_writes_final_and_srt() {
        let root = std::env::temp_dir().join(format!("aw-worker-dub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let base = std::env::var("AW_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
        assert!(Client::new(&base).healthy(), "服务不可用：{base}");

        // 1) 起 worker（与 GUI 同一条命令通道），工程根目录指向临时目录
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: root,
                cancel: cancel::CancelRegistry::new(),
            })
        });

        // 2) 点"开始配音"
        cmd_tx
            .send(Cmd::Run {
                revision: 1,
                task_id: 9,
                script: "第一句测试。第二句测试。".into(),
                model: "audio8-tts".into(),
                voice_ref: None,
                voice_ref_text: None,
                project_name: "worker-dub-e2e".into(),
                gap_ms: GAP_MS,
                auto_normalize: true,
                dict: empty_dict(),
                retry_failed: false,
            })
            .unwrap();

        let mut loaded_reused = None;
        let mut done_durations = Vec::new();
        let run_done = loop {
            let m = msg_rx.recv().expect("worker 应有消息");
            match m.msg {
                Msg::ProjectLoaded { reused, .. } => loaded_reused = Some(reused),
                Msg::Sentence {
                    index,
                    status,
                    duration,
                } if status == "done" => done_durations.push((index, duration)),
                Msg::RunDone {
                    failed,
                    stopped,
                    reused,
                    error,
                } => break (failed, stopped, reused, error),
                Msg::Fatal(e) => panic!("合成中止：{e}"),
                _ => {}
            }
        };
        assert_eq!(loaded_reused, Some(0), "全新工程不该复用旧句");
        assert_eq!(run_done, (0, false, 0, None), "实得 {run_done:?}");
        assert_eq!(
            done_durations.len(),
            2,
            "两句都要报终态：{done_durations:?}"
        );
        assert!(
            done_durations
                .iter()
                .all(|(_, d)| matches!(d, Some(d) if *d > 0.0)),
            "done 必须带真实时长：{done_durations:?}"
        );

        // 3) 点"导出"：拼装成品 + SRT
        cmd_tx
            .send(Cmd::Assemble {
                revision: 1,
                gap_ms: GAP_MS,
            })
            .unwrap();
        let (wav, srt, duration, done, skipped) = loop {
            let m = msg_rx.recv().expect("worker 应有消息");
            match m.msg {
                Msg::Assembled {
                    wav,
                    srt,
                    duration,
                    done,
                    skipped,
                } => break (wav, srt, duration, done, skipped),
                Msg::AssembleFailed(e) => panic!("拼装失败：{e}"),
                _ => {}
            }
        };
        drop(cmd_tx);
        handle.join().unwrap();

        assert_eq!((done, skipped), (2, 0), "两句都该进成品");
        assert!(wav.exists(), "成品不存在：{}", wav.display());
        assert!(srt.exists(), "SRT 不存在：{}", srt.display());
        let bytes = std::fs::read(&wav).unwrap();
        let measured = aw_core::dub::wav_duration(&bytes).expect("成品应是可解析的 wav");
        assert!(measured > 0.0, "成品时长为 0");
        assert!(
            (measured - duration).abs() < 0.05,
            "回报时长与文件对不上：{measured} vs {duration}"
        );
        let subs = std::fs::read_to_string(&srt).unwrap();
        assert_eq!(
            subs.matches(" --> ").count(),
            2,
            "SRT 该有两条字幕：\n{subs}"
        );
        eprintln!(
            "成品：{}（{measured:.2}s）\nSRT：{}\n{subs}",
            wav.display(),
            srt.display()
        );
    }
    /// 手写一段最小立体声 PCM wav（48kHz / 16bit）：分离的输入要是**确定的**，
    /// 而这个 crate 没有 wav 写入依赖（读时长走 aw_core::dub::wav_duration）——
    /// RIFF 头只有十几行，比为一个测试引依赖划算。
    fn write_test_tone_wav(path: &std::path::Path, seconds: f64) -> f64 {
        let rate = 48_000u32;
        let frames = (rate as f64 * seconds).round() as u32;
        let mut data = Vec::with_capacity(frames as usize * 4);
        for i in 0..frames {
            let t = i as f32 / rate as f32;
            let l = (t * 440.0 * std::f32::consts::TAU).sin() * 0.4;
            let r = (t * 660.0 * std::f32::consts::TAU).sin() * 0.4;
            data.extend_from_slice(&((l * i16::MAX as f32) as i16).to_le_bytes());
            data.extend_from_slice(&((r * i16::MAX as f32) as i16).to_le_bytes());
        }
        let mut out = Vec::with_capacity(data.len() + 44);
        let byte_rate = rate * 4; // 2 声道 × 16bit
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // fmt 块长度
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&2u16.to_le_bytes()); // 声道数
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes()); // 块对齐
        out.extend_from_slice(&16u16.to_le_bytes()); // 位深
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).unwrap();
        frames as f64 / rate as f64
    }

    /// 从 RIFF 头读采样率（不引解码器：就是 fmt 块第 4 个字段）。
    fn wav_header_sample_rate(bytes: &[u8]) -> u32 {
        assert!(bytes.len() > 44, "太短，不是完整 wav");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // fmt 块从 12 开始：12..16 = "fmt "，+4 = 长度，+8 = 格式，+10 = 声道，+12 = 采样率
        assert_eq!(&bytes[12..16], b"fmt ");
        u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]])
    }

    /// 造一条分离历史（主仓接线单测用；两轨文件由用例自己放）。
    fn sep_history_entry_fixture(
        created_at_ms: u64,
        vocals: &str,
        accompaniment: &str,
    ) -> sep_history::SeparationHistoryEntry {
        sep_history::SeparationHistoryEntry {
            created_at_ms,
            input_path: format!("/tmp/in-{created_at_ms}.wav"),
            vocals_file: vocals.into(),
            accompaniment_file: accompaniment.into(),
            duration_secs: None,
            sample_rate_hz: None,
        }
    }

    /// `refresh_separation_history` 的纯投影：**时间倒序**、最多 5 条；坏索引不能被显示成
    /// "没有历史"（否则用户以为历史丢了）。
    ///
    /// 阳性对照：把排序改成升序、或把 `take(5)` 放宽/去掉、或把 `Err` 分支写成"没有历史"，
    /// 本用例都会转红。
    #[test]
    fn separation_history_view_is_newest_first_and_caps_at_five() {
        let root = temp_dir("sep-history-view");
        let stems = root.join("stems");
        std::fs::create_dir_all(&stems).unwrap();
        for i in 1..=7u64 {
            let entry = sep_history_entry_fixture(i, &format!("v{i}.wav"), &format!("a{i}.wav"));
            sep_history::append_entry(&stems, &entry).unwrap();
        }

        let (status, items) = separation_history_view(&stems);
        assert_eq!(items.len(), 5, "只投影最近 5 条");
        assert_eq!(status, "最近 5 条（时间倒序）");
        let times: Vec<u64> = items.iter().map(|item| item.entry.created_at_ms).collect();
        assert_eq!(times, vec![7, 6, 5, 4, 3], "最新的必须排在最前");
        // 两轨文件不存在不该让条目消失，只是不可用
        assert!(!items[0].tracks.available());
        assert!(items[0].tracks.note.contains("文件不在了"));

        // 空索引（文件存在但 entries 为空）也要说"还没有"，不是"最近 0 条"
        std::fs::write(
            sep_history::history_path(&stems),
            br#"{"version":1,"entries":[]}"#,
        )
        .unwrap();
        let (status, items) = separation_history_view(&stems);
        assert_eq!(status, "还没有分离历史");
        assert!(items.is_empty());

        // 坏索引：保留损坏原因，不能被当成"没有历史"
        std::fs::write(
            sep_history::history_path(&stems),
            br#"{"version":1,"entries":"broken"}"#,
        )
        .unwrap();
        let (status, items) = separation_history_view(&stems);
        assert!(status.contains("读不出来"), "{status}");
        assert!(items.is_empty());

        // 目录都不在 = 没有历史（新工程）
        let (status, items) = separation_history_view(&root.join("nope"));
        assert_eq!(status, "还没有分离历史");
        assert!(items.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    /// 两轨**不在同一个工程目录**时必须拒绝（拿另一个工程的两轨去试听/导出 = 放错音频）。
    ///
    /// 阳性对照：去掉 `accompaniment.parent() != Some(stems_dir)` 这一句，本用例转红
    /// （用例特意在 stems_a 里也放了一份同名 a.wav，好让"少一道检查"真的能溜过去）。
    #[test]
    fn separation_tracks_check_rejects_cross_project_dirs() {
        let root = temp_dir("sep-cross");
        let stems_a = root.join("a/stems");
        let stems_b = root.join("b/stems");
        std::fs::create_dir_all(&stems_a).unwrap();
        std::fs::create_dir_all(&stems_b).unwrap();
        std::fs::write(stems_a.join("v.wav"), b"x").unwrap();
        std::fs::write(stems_a.join("a.wav"), b"x").unwrap();
        std::fs::write(stems_b.join("v.wav"), b"x").unwrap();
        std::fs::write(stems_b.join("a.wav"), b"x").unwrap();

        assert_eq!(
            separation_tracks_check(&stems_a.join("v.wav"), &stems_a.join("a.wav")),
            Ok(())
        );

        let err =
            separation_tracks_check(&stems_a.join("v.wav"), &stems_b.join("a.wav")).unwrap_err();
        assert!(err.contains("不在同一个工程目录"), "{err}");

        // 同目录但文件被删掉 → 如实说"文件不在了"，不能当成可用
        let err =
            separation_tracks_check(&stems_a.join("gone.wav"), &stems_a.join("a.wav")).unwrap_err();
        assert!(err.contains("文件不在了"), "{err}");

        let _ = std::fs::remove_dir_all(root);
    }

    /// 回看一条历史要写进界面的值：可用时给出两轨与"可试听/导出"，不可用时如实报原因、
    /// 且**不能留下任何可试听的两轨**（否则会放到上一条历史的音频）。
    ///
    /// 阳性对照：把不可用分支的 `tracks` 改成 `Some(...)`（忘了清旧两轨），本用例转红。
    #[test]
    fn sep_history_view_for_available_and_missing_items() {
        let root = temp_dir("sep-view-item");
        let stems = root.join("stems");
        std::fs::create_dir_all(&stems).unwrap();
        std::fs::write(stems.join("v.wav"), b"x").unwrap();
        std::fs::write(stems.join("a.wav"), b"x").unwrap();

        let entry = sep_history_entry_fixture(7, "v.wav", "a.wav");
        let item = SepHistoryItem {
            tracks: sep_history::resolve_tracks(&stems, &entry),
            entry,
        };
        let view = sep_history_view_for(&item);
        assert!(view.has_result, "回看就要亮成品区");
        assert_eq!(view.progress, 1.0);
        assert!(view.result_available);
        assert_eq!(
            view.tracks.as_ref().map(|(vocals, _)| vocals.clone()),
            Some(stems.join("v.wav"))
        );
        assert_eq!(view.vocals_label, "人声 · v.wav");
        assert_eq!(view.accompaniment_label, "伴奏 · a.wav");
        assert!(
            view.input_summary.starts_with("已回看："),
            "{}",
            view.input_summary
        );
        assert!(view.input_path.ends_with("in-7.wav"), "{}", view.input_path);
        assert!(
            view.sep_status.contains("可试听/导出"),
            "{}",
            view.sep_status
        );
        assert!(
            view.main_status.contains("已切到分离历史"),
            "{}",
            view.main_status
        );

        // 两轨都不在 → 不给出路径，状态行带原因
        let missing = sep_history_entry_fixture(8, "gone_v.wav", "gone_a.wav");
        let item = SepHistoryItem {
            tracks: sep_history::resolve_tracks(&stems, &missing),
            entry: missing,
        };
        let view = sep_history_view_for(&item);
        assert!(!view.result_available);
        assert!(view.tracks.is_none(), "不可用时不能留下可试听的两轨");
        assert!(
            view.sep_status.contains("文件不在了"),
            "{}",
            view.sep_status
        );
        assert!(view.main_status.contains("不可用"), "{}", view.main_status);

        let _ = std::fs::remove_dir_all(root);
    }

    /// 真机（默认 ignored）：**人声分离走 worker 的命令通道**，覆盖统一队列的两个关键语义——
    /// ① 排队中被取消的任务在 TaskStarted 之后直接终态、**不加载模型**（硬取消的收益就在这里）；
    /// ② 正常一条要跑出两轨落盘，且产物采样率标签与输入一致（relabel 那条修复的口径）。
    ///
    /// 模型走本机缓存（`~/Library/Caches/dev.StemSplitter.stem-splitter-core`，约 200MB）；
    /// 没缓存过的机器会先下载，这也是它标 ignored 的原因。
    #[test]
    #[ignore = "需要本机 htdemucs 模型（首次约 200MB）"]
    fn worker_separation_queues_cancels_and_writes_two_tracks() {
        let root = std::env::temp_dir().join(format!("aw-worker-sep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("tone.wav");
        let input_seconds = write_test_tone_wav(&input, 3.0);

        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        // 与 UI 共享的取消登记表：测试就是"另一个持表人"，模拟用户在排队时点了停止
        let cancel = cancel::CancelRegistry::new();
        let worker_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: std::env::temp_dir(),
                cancel: worker_cancel,
            })
        });

        // ① 排队中取消：登记在发送之前（UI 侧是"排队中点停止"同一时刻）
        let out_cancelled = root.join("cancelled-stems");
        cancel.cancel(11);
        cmd_tx
            .send(Cmd::RunSeparation {
                revision: 0,
                task_id: 11,
                input: input.clone(),
                out_dir: out_cancelled.clone(),
                stem: "tone".into(),
                model_dir: None,
                chunk_seconds: Some(5),
            })
            .unwrap();

        let mut started = false;
        let mut progress_after_start = 0usize;
        loop {
            let m = msg_rx.recv().expect("worker 应有消息");
            match m.msg {
                Msg::TaskStarted { task_id: 11 } => started = true,
                Msg::SeparationProgress { task_id: 11, .. } => progress_after_start += 1,
                Msg::SeparationStopped { task_id: 11 } => break,
                Msg::SeparationDone { task_id: 11, .. } => panic!("取消过的任务不该真的跑"),
                Msg::SeparationFailed { task_id: 11, error } => {
                    panic!("取消过的任务不该失败：{error}")
                }
                _ => {}
            }
        }
        assert!(
            started,
            "终态前必须有一次 TaskStarted（队列靠它把条目抬成运行中）"
        );
        assert_eq!(
            progress_after_start, 0,
            "排队中取消的任务不该产生进度（产出进度就说明已经加载模型开始跑了）"
        );
        assert!(
            !out_cancelled.exists(),
            "取消的任务不该留下输出目录：{}",
            out_cancelled.display()
        );

        // ② 正常一条：同一 worker 上的第二条，走出两轨
        let out_dir = root.join("stems");
        cmd_tx
            .send(Cmd::RunSeparation {
                revision: 0,
                task_id: 12,
                input: input.clone(),
                out_dir: out_dir.clone(),
                stem: "tone".into(),
                model_dir: None,
                chunk_seconds: Some(5),
            })
            .unwrap();

        let mut started_12 = false;
        let mut progress_seen = 0usize;
        let (vocals, accompaniment) = loop {
            let m = msg_rx.recv().expect("worker 应有消息");
            match m.msg {
                Msg::TaskStarted { task_id: 12 } => started_12 = true,
                Msg::SeparationProgress { task_id: 12, .. } => progress_seen += 1,
                Msg::SeparationDone {
                    task_id: 12,
                    vocals,
                    accompaniment,
                    ..
                } => break (vocals, accompaniment),
                Msg::SeparationStopped { task_id: 12 } => panic!("没登记取消，不该停"),
                Msg::SeparationFailed { task_id: 12, error } => panic!("分离失败：{error}"),
                _ => {}
            }
        };
        drop(cmd_tx);
        handle.join().unwrap();

        assert!(started_12, "正常那条也要有 TaskStarted");
        assert!(
            progress_seen > 0,
            "真跑应该有进度回调（实得 {progress_seen}）"
        );
        assert!(vocals.exists(), "人声轨不存在：{}", vocals.display());
        assert!(
            accompaniment.exists(),
            "伴奏轨不存在：{}",
            accompaniment.display()
        );

        let v_bytes = std::fs::read(&vocals).unwrap();
        let a_bytes = std::fs::read(&accompaniment).unwrap();
        let v_secs = aw_core::dub::wav_duration(&v_bytes).expect("人声轨应是可解析 wav");
        let a_secs = aw_core::dub::wav_duration(&a_bytes).expect("伴奏轨应是可解析 wav");
        assert!(
            (v_secs - input_seconds).abs() < 0.25,
            "人声轨时长应贴住输入（{input_seconds:.3}s），实得 {v_secs:.3}s"
        );
        assert!(
            (a_secs - input_seconds).abs() < 0.25,
            "伴奏轨时长应贴住输入（{input_seconds:.3}s），实得 {a_secs:.3}s"
        );
        assert_eq!(
            wav_header_sample_rate(&v_bytes),
            48_000,
            "人声轨采样率标签必须是输入的 48k（relabel 那条修复）"
        );
        assert_eq!(
            wav_header_sample_rate(&a_bytes),
            48_000,
            "伴奏轨采样率标签必须是输入的 48k（relabel 那条修复）"
        );
        assert_ne!(
            v_bytes, a_bytes,
            "两轨内容相同说明根本没分离（拿输入复制了两份）"
        );
        eprintln!(
            "人声轨：{}（{v_secs:.3}s，48kHz）\n伴奏轨：{}（{a_secs:.3}s，48kHz）\n进度回调 {progress_seen} 次",
            vocals.display(),
            accompaniment.display()
        );
    }
    /// BGM 的默认描述与两个档位：settings.json 的默认值必须与 ui/app.slint 的
    /// prop 默认值一致，否则"没设置过的用户"启动后看到的界面与落盘值就是两套。
    /// 用 include_str! 把 Slint 文件读进来断言，改一边不改另一边就会红。
    #[test]
    fn bgm_defaults_match_the_slint_props() {
        let app = include_str!("../ui/app.slint");
        assert!(
            app.contains(&format!("bgm-prompt: \"{DEFAULT_BGM_PROMPT}\"")),
            "engine/app.slint 与 DEFAULT_BGM_PROMPT 不一致：{DEFAULT_BGM_PROMPT}"
        );
        assert!(
            app.contains("bgm-duck-index: 1") && app.contains("bgm-standalone-index: 1"),
            "两个档位的默认值也要与 BgmSettings::default 一致"
        );
        let d = BgmSettings::default();
        assert_eq!(d.prompt, DEFAULT_BGM_PROMPT);
        assert_eq!(d.duck_index, 1);
        assert_eq!(d.standalone_index, 1);
    }

    /// 配音成品**损坏**时要按"没有配音成品"处理：worker 会走独立生成并写下
    /// standalone 摘要，导出侧必须用同一个判定，否则刚生成的 BGM 立刻被判过期
    /// （复核给的反例）。
    #[test]
    fn usable_voice_requires_a_readable_positive_duration() {
        let dir = std::env::temp_dir().join(format!("aw-voice-usable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();

        // 没有文件
        assert_eq!(usable_voice_seconds(&dir), None);

        // 有文件但读不出时长（损坏 / 半截）→ 按没有配音成品处理
        std::fs::write(dir.join("out/final.wav"), b"not a wav").unwrap();
        assert_eq!(usable_voice_seconds(&dir), None, "损坏的成品不算可用");

        // 真的 wav（3 秒）→ 可用，并返回时长
        write_test_tone_wav(&dir.join("out/final.wav"), 3.0);
        let secs = usable_voice_seconds(&dir).expect("合法 wav 应该可用");
        assert!((secs - 3.0).abs() < 0.05, "时长要对得上：{secs}");

        // 0 帧的 wav 同样不可用
        write_test_tone_wav(&dir.join("out/final.wav"), 0.0);
        assert_eq!(usable_voice_seconds(&dir), None, "0 帧不算可用");

        // 采样率 0 的畸形 wav：时长会算成 inf，只判 `> 0.0` 会把它当可用
        write_test_tone_wav(&dir.join("out/final.wav"), 1.0);
        let path = dir.join("out/final.wav");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[24..28].copy_from_slice(&0u32.to_le_bytes()); // fmt 块的采样率字段
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            usable_voice_seconds(&dir),
            None,
            "采样率 0（时长 inf）不算可用——否则后面会在算段数时炸"
        );
    }

    /// 反例回归（复核给的）：`out/final.wav` 损坏时 worker 走独立生成、清单里写的是
    /// standalone 摘要；导出侧也必须用"有没有**可用**配音成品"来定模式，两边摘要才一致。
    /// 只判断文件存在的话，这里算出来的是 mixed 摘要，刚生成的 BGM 立刻被判过期。
    #[test]
    fn broken_voice_makes_both_sides_use_standalone_mode() {
        let dir = std::env::temp_dir().join(format!("aw-broken-voice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::create_dir_all(dir.join("bgm")).unwrap();
        std::fs::write(dir.join("out/final.wav"), b"broken").unwrap();
        std::fs::write(dir.join("bgm/bgm.wav"), b"bgm").unwrap();

        let prompt = "口播背景";
        // worker 侧：没有可用配音成品 → 独立生成 → 写 standalone 摘要
        let digest = export::bgm_options_digest(prompt, export::BgmMode::Standalone, 30.0, 0.22);
        export::write_result_manifest(&dir, &digest).unwrap();

        // 导出侧：同一个判定 → 同一个模式 → 摘要必须一致
        let ctx = export::BgmContext::new(
            true,
            usable_voice_seconds(&dir).is_some(),
            prompt,
            0.22,
            30.0,
        );
        assert!(
            export::bgm_result_is_current(&dir, &ctx.options_digest),
            "两边模式判定必须同源，否则刚生成的 BGM 会被判过期"
        );
        assert!(matches!(
            export::stem_state(&dir, export::Stem::Bgm, &ctx),
            export::StemState::Ready(_)
        ));
    }

    /// 设置文件里某一段坏了（例如 `bgm.duck_index` 被手改成字符串）：
    /// **只丢那一段**，host/port 必须保住——整份解析失败等于"设置全没了"。
    #[test]
    fn broken_bgm_section_does_not_wipe_other_settings() {
        let v: serde_json::Value = serde_json::from_str(
            "{\"host\":\"10.0.0.9\",\"port\":9000,\"bgm\":{\"duck_index\":\"中\"}}",
        )
        .unwrap();
        let host: Option<String> = json_field(&v, "host");
        let port: Option<u16> = json_field(&v, "port");
        let bgm: Option<BgmSettings> = json_field(&v, "bgm");
        assert_eq!(
            host.as_deref(),
            Some("10.0.0.9"),
            "坏的是 bgm，不该连 host 一起丢"
        );
        assert_eq!(port, Some(9000));
        let bgm = bgm.unwrap_or_default();
        assert_eq!(bgm.prompt, DEFAULT_BGM_PROMPT, "坏掉的那段回落默认");
        assert_eq!(bgm.duck_index, 1);
    }

    /// settings.json 的 BGM 段要能存能读；老文件没有这一段时回落默认值（不能读失败）。
    #[test]
    fn settings_roundtrip_keeps_bgm_inputs() {
        let mut s = AppSettings {
            bgm: BgmSettings {
                prompt: "我的口播背景，钢琴，无人声".into(),
                duck_index: 2,
                standalone_index: 0,
            },
            ..Default::default()
        };
        let raw = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.bgm.prompt, "我的口播背景，钢琴，无人声");
        assert_eq!(back.bgm.duck_index, 2);
        assert_eq!(back.bgm.standalone_index, 0);

        // 老版本（没有 bgm 段）也能读，回落默认
        let legacy: AppSettings =
            serde_json::from_str("{\"host\":\"127.0.0.1\",\"port\":8080}").unwrap();
        assert_eq!(legacy.bgm.prompt, DEFAULT_BGM_PROMPT);
        assert_eq!(legacy.bgm.duck_index, 1);

        // 段里只写了半截（例如以后加字段）也不能整份读失败
        let partial: AppSettings =
            serde_json::from_str("{\"bgm\":{\"prompt\":\"只写了描述\"}}").unwrap();
        assert_eq!(partial.bgm.prompt, "只写了描述");
        assert_eq!(partial.bgm.duck_index, 1, "缺的字段才回落默认");

        s.bgm.prompt = "改过了".into();
        assert_ne!(s.bgm.prompt, DEFAULT_BGM_PROMPT);
    }

    /// 质检回读的候选**来自服务清单**，不是硬编的三个名字：清单加一个 task=asr
    /// 的模型，下拉里就要多一个（本用例故意放第 4 个，钉住"不是写死的三个"）。
    #[test]
    fn asr_models_come_from_the_manifest_not_a_hardcoded_list() {
        let m = |id: &str, task: &str, path: &str| ServerModel {
            id: id.into(),
            task: task.into(),
            family: "f".into(),
            path: path.into(),
            ..Default::default()
        };
        let cfg = ServerConfig {
            host: None,
            port: None,
            min_free_memory_mb: None,
            models: vec![
                m("audio8-tts", "tts", "/models/tts/x.gguf"),
                m("qwen3-asr", "asr", "/models/asr/q.gguf"),
                m("audio8-asr", "asr", "/models/asr/a.gguf"),
                m("sortformer-diar", "diar", "/models/diar/s.gguf"),
                m("fun-asr", "asr", "/models/asr/f.gguf"),
                m("whisper-tiny", "asr", "/models/asr/w.gguf"),
            ],
        };
        assert_eq!(
            asr_models_from(Some(&cfg)),
            vec!["qwen3-asr", "audio8-asr", "fun-asr", "whisper-tiny"],
            "只收 task==asr，且保持清单顺序"
        );
        assert!(asr_models_from(None).is_empty(), "没有清单 = 没有候选");
    }

    /// 生效模型 = 设置 > 默认；空串/空白不算选择（回退默认而不是拿空串去请求）。
    #[test]
    fn effective_asr_model_defaults_then_honours_the_choice() {
        let none = AppSettings::default();
        assert_eq!(effective_asr_model(&none), "qwen3-asr");
        let blank = AppSettings {
            asr_model: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(effective_asr_model(&blank), "qwen3-asr", "空白不算选择");
        let picked = AppSettings {
            asr_model: Some("  audio8-asr  ".into()),
            ..Default::default()
        };
        assert_eq!(effective_asr_model(&picked), "audio8-asr", "去掉两侧空白");
        // 清单里没有也照用：换模型会改质检口径，不能静默替换
        let gone = AppSettings {
            asr_model: Some("removed-asr".into()),
            ..Default::default()
        };
        assert_eq!(effective_asr_model(&gone), "removed-asr");
    }

    /// 下拉的显示名、下标、id 必须来自同一份推导：当前模型不在清单里时也要留在
    /// 第 0 项（否则清单一变，用户没动过下拉，生效模型却被静默换掉）。
    #[test]
    fn asr_picker_view_keeps_the_current_model_visible() {
        let models: Vec<String> = ["qwen3-asr", "audio8-asr"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let (ids, labels, index) = asr_picker_view(&models, "audio8-asr");
        assert_eq!(index, 1);
        assert_eq!(labels[index as usize], "audio8-asr");
        assert_eq!(
            ids[index as usize], "audio8-asr",
            "下标取回的 id 必须就是显示的那个"
        );

        let (ids, labels, index) = asr_picker_view(&models, "gone-asr");
        assert_eq!(index, 0);
        assert_eq!(ids[0], "gone-asr", "不在清单里也照用它");
        assert!(labels[0].contains("不在当前清单"), "{}", labels[0]);
        assert_eq!(labels.len(), 3);

        // 选第 2 项（清单第 2 个）拿到的就是它自己的 id——显示与取值同源
        let (ids, labels, _) = asr_picker_view(&models, "qwen3-asr");
        assert_eq!(
            (ids[1].as_str(), labels[1].as_str()),
            ("audio8-asr", "audio8-asr")
        );
    }

    /// 报告与 ASR 请求都必须用**本次实际用的模型**，不能退回写死的字面量。
    ///
    /// 用源码级守卫而不是只测纯函数：`qa_report_markdown` 的入参谁都能传对，
    /// 真正会漂移的是**调用点**（"回显与真实行为不同源"这类问题被复核抓到过多次）；
    /// 而真机 e2e 是 `#[ignore]` 的，跑不到就等于没保护。
    #[test]
    fn report_and_request_read_the_run_model_not_a_literal() {
        let src = src_lf(include_str!("main.rs"));
        // 这个"针"必须拼出来：直接写完整字面量的话，它会命中**本用例自己的源码**，
        // 断言恒真、改坏也不红（第一次写就踩了这个坑，阳性对照抓出来的）。
        let request = format!("client.asr_with({}, &wav)", "&model");
        assert!(
            src.contains(&request),
            "ASR 请求必须用这次选中的模型，不能退回 client.asr()"
        );
        assert!(
            src.contains(
                "qa_report_markdown(\n                    &project_name,\n                    &model,"
            ),
            "报告必须用本次实际用的模型"
        );
        // 报告调用点里不能再出现写死的默认模型名（正是这条被复核驳回的形态）
        assert!(
            !src.contains("\"qwen3-asr\",\n                    &rows"),
            "报告路径不得写死模型名"
        );
    }

    /// 下拉旁的说明：候选数来自清单、当前值不在清单要明说、换模型口径会变要明说。
    #[test]
    fn asr_model_note_is_honest_about_where_candidates_come_from() {
        let models: Vec<String> = ["qwen3-asr", "audio8-asr", "fun-asr"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let note = asr_model_note(&models, "qwen3-asr", true);
        assert!(note.contains("3 个 ASR 模型"), "候选数要来自清单：{note}");
        assert!(note.contains("说话人分离"), "要如实说明能力差异：{note}");
        assert!(!note.contains("不在清单里"), "在清单里就不该这么写：{note}");

        let note = asr_model_note(&models, "gone-asr", false);
        assert!(
            note.contains("gone-asr") && note.contains("不在清单里"),
            "当前值不在清单时要说清（照用不换，但服务可能加载不了）：{note}"
        );

        let note = asr_model_note(&[], "qwen3-asr", false);
        assert!(note.contains("没读到服务清单"), "清单读不到要说清：{note}");
    }

    /// 内存不足 → 整轮立刻收尾（带可执行提示）；其它 ASR 失败 → 只记这一句。
    ///
    /// 直接喂真机 503 body，不起服务、不进退避等待：worker 执行的是这条判据的结论。
    #[test]
    fn memory_shortfall_is_fatal_while_other_asr_errors_are_counted() {
        let body = r#"{"error":{"message":"cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}"#;
        let candidates = vec![
            ("qwen3-asr".to_string(), Some(3_400_000_000u64)),
            ("audio8-asr".to_string(), Some(459_000_000)),
        ];
        let err = aw_core::ClientError::Server(503, body.to_string());
        match classify_asr_failure("qwen3-asr", &err, &candidates) {
            EvalAsrFailure::Fatal(msg) => {
                assert!(msg.contains("qwen3-asr"), "{msg}");
                assert!(
                    msg.contains("3.84 GiB") && msg.contains("4.31 GiB"),
                    "{msg}"
                );
                assert!(msg.contains("audio8-asr"), "{msg}");
            }
            other => panic!("内存不足必须整轮收尾，实得 {other:?}"),
        }

        // 模型忙碌也是 503 —— 那是"这一句没测到"，不能当成装不下
        let busy = r#"{"error":{"message":"model 'qwen3-asr' is busy","type":"model_busy"}}"#;
        assert_eq!(
            classify_asr_failure(
                "qwen3-asr",
                &aw_core::ClientError::Server(503, busy.to_string()),
                &candidates
            ),
            EvalAsrFailure::Counted
        );
        // 传输层失败（服务没起来）同样只记一句：换模型也救不了
        assert_eq!(
            classify_asr_failure(
                "qwen3-asr",
                &aw_core::ClientError::Http("连接被拒绝".into()),
                &candidates
            ),
            EvalAsrFailure::Counted
        );
    }

    /// 内存不足的提示必须**可执行**：模型名 + 需要/可用数字 + 更小的候选 + 不自动换。
    #[test]
    fn memory_shortfall_hint_names_the_model_numbers_and_smaller_candidates() {
        // 真机 503 body 解析出来的数字
        let mem = aw_core::InsufficientMemory {
            message: "cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)".into(),
            model: Some("qwen3-asr".into()),
            estimated_mib: Some(3389),
            headroom_mib: Some(1024),
            available_mib: Some(3932),
        };
        let candidates = vec![
            ("qwen3-asr".to_string(), Some(3_400_000_000u64)),
            ("audio8-asr".to_string(), Some(459_000_000)),
            ("fun-asr".to_string(), Some(1_200_000_000)),
        ];
        let hint = memory_shortfall_hint("qwen3-asr", Some(&mem), &candidates);
        assert!(hint.contains("qwen3-asr"), "要说清是哪个模型：{hint}");
        assert!(
            hint.contains("4.31 GiB"),
            "要说需要多少（估算+余量）：{hint}"
        );
        assert!(
            hint.contains("3.31 GiB") && hint.contains("1.00 GiB"),
            "拆开也要给：{hint}"
        );
        assert!(hint.contains("3.84 GiB"), "要说当时可用多少：{hint}");
        assert!(
            hint.contains("audio8-asr") && hint.contains("fun-asr"),
            "要列出更小的候选：{hint}"
        );
        assert!(
            !hint.contains("qwen3-asr（权重"),
            "当前模型不该出现在降档候选里：{hint}"
        );
        assert!(hint.contains("不会自动换"), "必须写明不自动换：{hint}");
        assert!(hint.contains("质检回读模型"), "要指出去哪儿改：{hint}");
        // 没有更小的可选时不能留空话
        let hint = memory_shortfall_hint("qwen3-asr", Some(&mem), &candidates[..1]);
        assert!(hint.contains("没有更小的 ASR 模型可选"), "{hint}");
        // 数字解析不出来也要给出动作，而不是只报错
        let no_numbers = aw_core::InsufficientMemory {
            message: "cannot load model 'qwen3-asr'（这条没有可解析的数字）".into(),
            model: Some("qwen3-asr".into()),
            estimated_mib: None,
            headroom_mib: None,
            available_mib: None,
        };
        let hint = memory_shortfall_hint("qwen3-asr", Some(&no_numbers), &candidates);
        assert!(hint.contains("没给出可解析"), "{hint}");
        assert!(hint.contains("audio8-asr"), "数字缺失也要给候选：{hint}");
    }

    /// 候选按磁盘权重升序、剔除当前模型；权重读不到的排在后面（不假装知道谁更小）。
    #[test]
    fn smaller_asr_candidates_orders_by_weight_and_drops_the_current() {
        let c = |id: &str, size: Option<u64>| (id.to_string(), size);
        let candidates = vec![
            c("qwen3-asr", Some(3_400_000_000)),
            c("fun-asr", Some(1_200_000_000)),
            c("mystery-asr", None),
            c("audio8-asr", Some(459_000_000)),
            c("bigger-asr", Some(9_000_000_000)),
        ];
        let got = smaller_asr_candidates("qwen3-asr", &candidates);
        assert_eq!(got.len(), 2, "更大/权重未知/当前的都要剔掉：{got:?}");
        assert!(got[0].starts_with("audio8-asr"), "小的排前面：{got:?}");
        assert!(got[1].starts_with("fun-asr"), "{got:?}");
        assert!(
            got[0].contains("437.7MB") && got[1].contains("1144.4MB"),
            "要给出权重数字（bytes→MB，1024 进制）：{got:?}"
        );
        assert!(
            !got.iter().any(|g| g.contains("9000")),
            "比当前更大的不当降档候选：{got:?}"
        );

        // 当前模型的权重读不到 → 不筛大小（不假装知道谁更小），但不能把"权重未知"的
        // 候选排到"知道更小"的前面去
        let unknown_current = smaller_asr_candidates("mystery-asr", &candidates);
        assert_eq!(
            unknown_current.len(),
            3,
            "不知道当前多大就不按大小筛（只受最多 3 条的上限约束）：{unknown_current:?}"
        );
        assert!(
            unknown_current.last().unwrap().starts_with("qwen3-asr"),
            "有数字的在前（即便比当前大也不装懂）：{unknown_current:?}"
        );
    }

    /// 设计模型选择也要跨重启：persist 的落盘值与读回后的生效值一致。
    #[test]
    fn design_model_choice_survives_a_settings_roundtrip() {
        let dir = temp_dir("design-model-settings");
        let path = dir.join("settings.json");
        let store = std::sync::Mutex::new(AppSettings::default());
        let saved = persist_design_model_at(&store, &path, " qwen3-tts-voicedesign ").unwrap();
        assert_eq!(saved, "qwen3-tts-voicedesign");
        let back = load_settings_at(&path);
        assert_eq!(back.design_model.as_deref(), Some("qwen3-tts-voicedesign"));
    }

    /// 下载源 / 并发也要跨重启：落盘 -> 读回 -> 生效值。
    ///
    /// 断言的生效值走 `effective_download_mirror` / `effective_download_concurrency`
    /// 两个**唯一入口**，不直接读字段——这样"存进去的东西和真正用的东西"被同一条
    /// 链路钉住（见 LESSON_同一语义两处实现必然漂移）。
    #[test]
    fn download_source_and_concurrency_survive_a_settings_roundtrip() {
        let dir = temp_dir("dl-settings");
        let path = dir.join("settings.json");

        // 缺省：官方 + 2 并发
        let none = AppSettings::default();
        assert_eq!(effective_download_mirror(&none), "");
        assert_eq!(effective_download_concurrency(&none), 2);

        // 写入镜像（带尾斜杠，落盘前应归一）
        let st = AppSettings {
            download_mirror: download_mirror::normalize_prefix("https://hf-mirror.com///"),
            download_concurrency: Some(3),
            ..AppSettings::default()
        };
        save_settings_at(&path, &st).unwrap();

        let back = load_settings_at(&path);
        assert_eq!(effective_download_mirror(&back), "https://hf-mirror.com");
        assert_eq!(effective_download_concurrency(&back), 3);
        assert_eq!(
            download_mirror::rewrite_url(
                "https://huggingface.co/a/b.gguf",
                &effective_download_mirror(&back)
            ),
            "https://hf-mirror.com/a/b.gguf",
            "读回来的镜像必须真的作用到 URL 上，不只是回显"
        );

        // 越界 / 0 的配置被归一化到 1..=4（不是当成缺省 2）
        let st = AppSettings {
            download_concurrency: Some(0),
            ..AppSettings::default()
        };
        save_settings_at(&path, &st).unwrap();
        assert_eq!(effective_download_concurrency(&load_settings_at(&path)), 1);
        let st = AppSettings {
            download_concurrency: Some(99),
            ..AppSettings::default()
        };
        save_settings_at(&path, &st).unwrap();
        assert_eq!(effective_download_concurrency(&load_settings_at(&path)), 4);

        // 老 settings.json（没有这两个键）照读，不炸也不丢别的字段
        std::fs::write(&path, r#"{"host":"10.0.0.9","port":8080}"#).unwrap();
        let old = load_settings_at(&path);
        assert_eq!(old.host.as_deref(), Some("10.0.0.9"));
        assert_eq!(effective_download_mirror(&old), "");
        assert_eq!(effective_download_concurrency(&old), 2);
    }

    /// 回显与真实行为同源：`source_note` 说的是哪条源，`rewrite_url` 就真的走去哪条源。
    #[test]
    fn source_note_and_rewrite_agree_on_the_effective_source() {
        let official = "https://huggingface.co/org/repo/resolve/main/m.gguf";
        assert!(download_mirror::source_note("").contains("官方"));
        assert_eq!(download_mirror::rewrite_url(official, ""), official);

        let note = download_mirror::source_note("https://hf-mirror.com/");
        assert!(note.contains("镜像 https://hf-mirror.com"), "note={note}");
        assert_eq!(
            download_mirror::rewrite_url(official, "https://hf-mirror.com/"),
            "https://hf-mirror.com/org/repo/resolve/main/m.gguf"
        );
    }

    /// 并发数只允许从 `download::effective_concurrency` 推导，不许在别处写死。
    ///
    /// 这条防的是"界面回显 2 并发、队列其实起了 4 个"（或反过来）。
    #[test]
    fn concurrency_is_derived_from_one_place() {
        let src = include_str!("main.rs");
        let production = src.split("mod tests {").next().unwrap_or(src);
        let calls = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("download::effective_concurrency("))
            .count();
        assert_eq!(
            calls, 1,
            "并发数只允许在 `effective_download_concurrency` 里一处归一化（现在 {calls} 处）"
        );
        assert!(
            production.contains("Some(concurrency),"),
            "队列线程数必须来自 `effective_download_concurrency()`，不能另写一个常量"
        );
    }

    /// 「真正决定这次走哪条源」的改写只允许一处：`download_mirror_rewrite`。
    ///
    /// 防的是本仓反复抓的形态：界面上写"当前源是镜像"，实际请求另一处悄悄用官方
    /// （或反过来）。队列的入队回调必须传 `download_mirror_rewrite`。
    ///
    /// **刻意豁免**：`AW_UI_STATE=download-source` 那个只读演示态要摆"改写前/后"两行
    /// 对照——`download_source_demo_lines()` 整个函数体被排除（它只 `eprintln!`，
    /// 不参与真实请求）。豁免是**按函数名**给的，不是按文件给的：在别的任何地方
    /// 再加一处改写，计数立刻超标。
    ///
    /// 注意针的写法：`rewrite_url` 这个名字在**本用例自己的源码**里也会出现，
    /// 所以只能统计"调用点"，不能统计裸名字（否则断言会被自己的源码喂饱、恒真——本仓踩过）。
    #[test]
    fn rewrite_url_has_exactly_one_production_call_site() {
        let src = src_lf(include_str!("main.rs"));
        let production = src.split("mod tests {").next().unwrap_or(&src);
        let demo_start = production
            .find("fn download_source_demo_lines()")
            .expect("演示对照函数必须存在（守卫按这个函数名给豁免）");
        let after = &production[demo_start..];
        let demo_end = after
            .find("\n}\n")
            .map(|i| demo_start + i)
            .expect("演示对照函数应有结束花括号");
        let mut needle = production[..demo_start].to_string();
        needle.push_str(&production[demo_end..]);
        // 排除注释行（守卫自己的 doc 注释里就会出现这个名字）
        let calls = needle
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("download_mirror::rewrite_url("))
            .count();
        assert_eq!(
            calls, 1,
            "改写 URL 只允许在 `download_mirror_rewrite` 里一处（现在 {calls} 处）：\
             两处实现必然漂移，界面说镜像、实际走官方就是这么来的"
        );
        assert!(
            production.contains("download_mirror_rewrite,"),
            "入队回调必须传 `download_mirror_rewrite`，不能另写改写逻辑"
        );
    }

    /// 选择要真的落盘、重启读回；拒绝写入时不得改动盘上已有的值。
    #[test]
    fn asr_model_choice_survives_a_settings_roundtrip() {
        let dir = temp_dir("asr-model-settings");
        let path = dir.join("settings.json");
        let store = std::sync::Mutex::new(AppSettings::default());
        assert_eq!(
            effective_asr_model(&store.lock().unwrap()),
            "qwen3-asr",
            "没选过 = 默认"
        );

        assert_eq!(
            persist_asr_model_at(&store, &path, " audio8-asr ").unwrap(),
            "audio8-asr"
        );
        let back = load_settings_at(&path);
        assert_eq!(back.asr_model.as_deref(), Some("audio8-asr"), "重启读回");
        assert_eq!(effective_asr_model(&back), "audio8-asr");

        assert_eq!(
            persist_asr_model_at(&store, &path, "fun-asr").unwrap(),
            "fun-asr"
        );
        assert_eq!(
            load_settings_at(&path).asr_model.as_deref(),
            Some("fun-asr")
        );

        assert!(persist_asr_model_at(&store, &path, "   ").is_err());
        assert_eq!(
            load_settings_at(&path).asr_model.as_deref(),
            Some("fun-asr"),
            "拒绝写入时盘上的值不能被改掉"
        );

        // 老版本文件（没有这一段）读得回来，且回落默认
        let legacy_path = dir.join("legacy.json");
        std::fs::write(&legacy_path, "{\"host\":\"127.0.0.1\",\"port\":8080}").unwrap();
        let legacy = load_settings_at(&legacy_path);
        assert_eq!(legacy.asr_model, None);
        assert_eq!(effective_asr_model(&legacy), "qwen3-asr");
    }

    /// "这套 BGM 结果还算不算当前"：`has_result && !stale`。
    /// 改描述后只置 stale（结果还在、能试听），光看 has_result 会把旧结果当当前导出；
    /// 从磁盘恢复的那套一律 stale（重开后判不出上次用的描述）。
    #[test]
    fn bgm_result_is_exportable_only_when_current() {
        assert!(bgm_result_exportable(true, false), "会话内刚混完的才算当前");
        assert!(
            !bgm_result_exportable(true, true),
            "描述改过/磁盘恢复的不能导"
        );
        assert!(!bgm_result_exportable(false, false), "没有结果当然不能导");
        assert!(!bgm_result_exportable(false, true));
    }

    /// 导出的互斥判据：两个写者（UI 侧在飞的拼装/混音/导出、批量 worker 每篇的拼装）
    /// 任何一个在飞都不许导——`assemble` 先发布 final.wav 再写 final.srt，混音也是先写
    /// voice/mixed 再落盘，中间导出去就会配错成对产物。
    #[test]
    fn export_refuses_while_any_publisher_is_in_flight() {
        assert_eq!(export_refusal(false, false), None, "都空闲才允许");
        let by_batch = export_refusal(false, true).expect("批量在跑要拒绝");
        assert!(by_batch.contains("批量任务正在跑"), "{by_batch}");
        assert!(by_batch.contains("成对产物"), "要说清为什么等：{by_batch}");
        let by_busy = export_refusal(true, false).expect("拼装/混音在飞要拒绝");
        assert!(by_busy.contains("正在进行"), "{by_busy}");
        // 两个都在飞时以批量那条为准（先判的那条）
        assert_eq!(
            export_refusal(true, true).map(|s| s.contains("批量任务正在跑")),
            Some(true)
        );
    }

    /// 生产代码（`main.rs` 去掉测试模块）——扫源码的守卫只看这一部分。
    ///
    /// 不切掉测试模块，守卫里的字面量（`"picker::pick_"` 之类）会被自己命中，
    /// 断言恒真——本仓已经踩过这个坑（`LESSON_系列_测试断言与e2e.md`
    /// 「扫自己源码的守卫：断言里的『针』会被它自己的源码命中」）。
    /// 扫自己源码的守卫要读**归一化成 LF** 的文本。
    ///
    /// Windows 上 git checkout 会把工作区写成 CRLF（本仓库没有 .gitattributes 强制 LF），
    /// 而 `include_str!` 读的正是工作区那份 —— 于是所有按 `\n` 写的模式都匹配不上，
    /// 守卫不是"变红"而是 `.expect(...)` 直接 panic。2026-09-18 三平台 CI 实测：5 个守卫
    /// 在 Windows 上就是这么挂的。
    fn src_lf(s: &str) -> String {
        s.replace("\r\n", "\n")
    }

    /// 从 `needle` 处取长度约 `len` 的源码窗口。
    ///
    /// 两个 Windows 坑：`include_str!` 在 CRLF 检出下字节偏移与 LF 不同；窗口末端
    /// `at + len` 还可能落在多字节中文字符中间（CI 35551806143 实测 panic：
    /// end byte index is not a char boundary）。约定：调用方先 `src_lf` 归一（CRLF→LF），
    /// 本函数再把末端收缩到 char boundary。找不到 needle 时 panic 出可读信息——
    /// 守卫要能红，而不是静默扫错窗口。
    fn source_window<'a>(src: &'a str, needle: &str, len: usize) -> &'a str {
        let at = src.find(needle).unwrap_or_else(|| {
            panic!(
                "source_window：源码里找不到 needle `{needle}`（守卫锚点漂了，改源码须同步改用例）"
            )
        });
        let mut end = (at + len).min(src.len());
        while end > at && !src.is_char_boundary(end) {
            end -= 1;
        }
        &src[at..end]
    }

    fn production_source() -> String {
        // 本守卫族的扫描范围 = main.rs 一个文件（`include_str!("main.rs")` 只读本文件）：
        // engine_supervisor.rs / picker.rs / update.rs 等模块都不在范围内。
        // **打开器 `open_external_url*` 若移出本文件，必须同步把这里扩成扫对应文件
        // 或整个 src/，否则这些守卫会失明**（2026-09-20 审查 M2）。
        let src = src_lf(include_str!("main.rs"));
        let cut = src
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("测试模块的起点变了：守卫会退化成扫全文件（字面量自命中、断言恒真）");
        src[..cut].to_string()
    }

    /// `source_window` 自身的回归：CRLF + 多字节中文 + 窗口长度故意切在字符中间。
    ///
    /// 阳性对照：fixture 里 naive 的 `&src[at..(at + len).min(src.len())]` 末端落在
    /// '条' 中间、必然 panic（Windows CI 35551806143 实测挂的就是这种）；helper 必须
    /// 收缩到 char boundary 并返回合法 UTF-8 窗口。
    #[test]
    fn source_window_survives_crlf_and_mid_char_cut() {
        let raw = "fn demo() {\r\n    // 音频条\r\n    let x = 1;\r\n}\r\n";
        let src = src_lf(raw);
        let needle = "音频";
        let at = src.find(needle).expect("fixture 里 needle 必须在");
        let len = 7; // needle 占 6 字节，+1 正好切进 '条'（3 字节）中间
        let naive_end = (at + len).min(src.len());
        assert!(
            !src.is_char_boundary(naive_end),
            "fixture 必须真的把末端切在多字节字符中间（否则阳性对照空转）"
        );
        let win = source_window(&src, needle, len);
        assert_eq!(win, "音频", "末端必须收缩到 '条' 之前的 char boundary");
        assert!(
            src[at + win.len()..].starts_with('条'),
            "窗口之后必须是完整字符：末端落在 char boundary 上"
        );
    }

    /// 找不到 needle 时 helper 必须 panic 且带可读信息——守卫宁可红，不能静默扫错窗口。
    #[test]
    #[should_panic(expected = "找不到 needle")]
    fn source_window_panics_with_readable_message_when_needle_missing() {
        let src = src_lf("fn a() {}\r\nfn b() {}\r\n");
        source_window(&src, "根本不存在的锚点", 40);
    }

    /// 选择器的三态必须在**每个调用点显式分流**：`Cancelled`（用户关窗）与
    /// `Unavailable`（命令不存在 / 权限被拒 / 非 0 退出）绝不能合并回一个"没选到"。
    ///
    /// 这条读源码钉住"不许再塌成 `Option`"：每个 `picker::pick_*` 调用点都必须同时
    /// 出现 `Outcome::Cancelled` 与 `Outcome::Unavailable(..)` 两个分支。
    ///
    /// 阳性对照（实测过）：把 `spawn_file_pick` 那处写回
    /// `let Some(path) = ... else { 已取消 }`（即两种"没选到"共用一条路），
    /// `unavailable` 计数比调用点少 1、这条立刻红。
    #[test]
    fn every_picker_call_site_handles_cancel_and_unavailable_separately() {
        let src = production_source();
        let calls = src.matches("picker::pick_").count();
        let cancelled = src.matches("picker::Outcome::Cancelled =>").count();
        let unavailable = src.matches("picker::Outcome::Unavailable(").count();
        // 光有分支还不够：**分支的正文也必须交给用户可执行的说明**。
        // 阳性对照：把「音频」那处的 `trouble.note()` 换成一句"取消了选择音频"
        // （分支还在、文案却撒谎）→ 下面这一条立刻红。
        let notes = src.matches("trouble.note()").count();
        assert_eq!(
            calls, 9,
            "选择器调用点数量变了（{calls}）——加/删调用点时同步更新这条守卫"
        );
        assert_eq!(
            cancelled, calls,
            "有调用点没显式处理「用户取消」（{cancelled} 个分支 vs {calls} 个调用点）"
        );
        assert_eq!(
            unavailable, calls,
            "有调用点没显式处理「选择器不可用」（{unavailable} 个分支 vs {calls} 个调用点）——\
             那正是本批要修的静默失败"
        );
        assert_eq!(
            notes, calls,
            "有「选择器不可用」分支没把可执行说明（含安装/授权指引）交出去\
             （{notes} 处 trouble.note() vs {calls} 个调用点）"
        );
    }

    /// 参考音频入口必须全部走 prepare：四个入口（开始合成 / 批量提交 / 音色试听 /
    /// 单句重录）的**请求实际使用 ≤15s 的路径**。出现次数 = prepare 定义 +
    /// 三个 UI 入口 + load_resumable 输入收敛 + migrate helper（Redo 走它）= 6 处。
    ///
    /// 阳性对照（实测过）：删掉任意一个 UI 入口的 prepare → 计数变 5 红；
    /// 新加一个克隆请求入口而不同步加 prepare → 计数不变但发送点前的窗口扫不到 → 红。
    #[test]
    fn every_clone_request_entrance_uses_prepared_reference() {
        let src = production_source();
        assert_eq!(
            src.matches("prepare_reference_for_clone(").count(),
            6,
            "prepare 定义 + 三个 UI 入口 + load_resumable 输入收敛 + migrate helper；\
             加/删入口必须同步加/删"
        );
        // 每个入口都必须把 voice_ref_text 一起交给 prepare：裁剪副本的旁车文本
        // 只能从这里生成（漏传 → 旁车缺失 → 回到「84 字音频配 1077 字文本」的错配）。
        assert!(
            src.matches("prepare_reference_for_clone(&path, voice_ref_text.as_deref())")
                .count()
                >= 3,
            "三个 UI 入口必须把 voice_ref_text 一起交给 prepare（旁车文本来源）"
        );
        for anchor in [
            ".send(Cmd::Run {",
            ".send(Cmd::RunBatch {",
            ".send(Cmd::PreviewVoice {",
        ] {
            let at = src
                .find(anchor)
                .unwrap_or_else(|| panic!("找不到发送点 {anchor}"));
            // 窗口起点收缩到 char boundary（与 source_window 同一原则：
            // 裸字节切片会切进多字节字符，见 `LESSON_CRLF` 那条教训）
            let mut start = at.saturating_sub(4096);
            while start > 0 && !src.is_char_boundary(start) {
                start -= 1;
            }
            let before = &src[start..at];
            assert!(
                before.contains("prepare_reference_for_clone("),
                "{anchor} 之前必须已经过 prepare（在发起前收敛到 ≤15s，不收敛会把引擎进程打死）"
            );
        }
        // Redo 的请求在 worker 侧按工程 voice_ref 发出：迁移必须在分支里、
        // make_client 之前（那里才是不发任何请求的拦截点）。
        let redo_at = src
            .find("Cmd::Redo { revision, index } =>")
            .expect("找不到 Redo 分支");
        let client_at = src[redo_at..]
            .find("make_client()")
            .expect("Redo 分支里应有 make_client");
        let arm = &src[redo_at..redo_at + client_at];
        assert!(
            arm.contains("migrate_overlong_voice_ref("),
            "Cmd::Redo 分支必须在 make_client 之前把工程 voice_ref 收敛到 ≤15s（migrate helper）"
        );
    }

    /// 系统对话框**只能**由 `picker` 模块拉起：`main.rs` 里不许再出现
    /// osascript / powershell / zenity——多一处就多一份"取消与起不来混在一起"的机会。
    #[test]
    fn dialogs_are_only_launched_from_the_picker_module() {
        let src = production_source();
        for program in ["\"osascript\"", "\"zenity\"", "\"powershell\""] {
            assert!(
                !src.contains(&format!("Command::new({program})")),
                "main.rs 里不该再直接拉起系统对话框程序 {program}；\
                 平台差异与三态判定都在 src/picker.rs 一处"
            );
        }
        // 曾经把"起不来 / 取消 / 非 0 退出"一锅端的两个函数不能再出现
        for gone in ["fn pick_output", "fn pick_folder_with_prompt"] {
            assert!(
                !src.contains(gone),
                "{gone}… 已被 picker 模块取代，不该复活"
            );
        }
    }

    /// Windows 打开外部 URL 不得再经 cmd/start：cmd.exe /C 会把 URL 里的 `&` 等
    /// 元字符解析成第二条命令（命令注入 + 常见 URL 截断）。只允许 `ShellExecuteW`
    /// （`open_external_url_windows`）。
    ///
    /// 匹配**大小写不敏感**且覆盖 `cmd.exe` 形态：Windows 上 `cmd`/`CMD`/
    /// `cmd.exe`/`CMD.EXE` 是同一个注入面，只钉小写 `"cmd"` 的精确串会被
    /// 大小写变体绕过（2026-09-20 审查 M1）。
    ///
    /// 阳性对照（实测过）：把 Windows 分支临时写回
    /// `("cmd", vec!["/C", "start", "", url])` → 这条立刻红。
    #[test]
    fn windows_url_opener_must_not_go_through_cmd() {
        let src = production_source();
        // 先统一小写再匹配，一次拦住大小写变体；`"cmd.exe"` 是 `"cmd"` 的
        // 显式可执行名形态，两者都要钉死。
        let lower = src.to_lowercase();
        for forbidden in [
            "command::new(\"cmd",
            "\"cmd.exe\"",
            "\"cmd\"",
            "vec![\"/c\", \"start\"",
        ] {
            assert!(
                !lower.contains(forbidden),
                "main.rs 生产代码里出现 {forbidden}——Windows 打开发布页必须走 \
                 ShellExecuteW（open_external_url_windows），不得经 cmd"
            );
        }
    }

    /// 打开发布页的 URL 校验反向用例：空串、非 http(s)、含控制字符（`\n`/`\r`/`\0`）
    /// 都必须在**拉起系统打开器之前**被拒绝。这些输入全在进程拉起之前被拒，
    /// 所以测试里可以放心直调 `open_external_url`（绝不会真的开浏览器）。
    ///
    /// 阳性对照（实测过）：删掉 `release_url_is_openable` 里的控制字符检查后，
    /// `https://example.com/a\nb` 会带着换行穿过校验 → 这条立刻红。
    #[test]
    fn open_external_url_rejects_malformed_urls_before_launching() {
        for bad in [
            "",
            "   ",
            "ftp://example.com/a",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "//example.com/a",
            "https://example.com/a\nb",
            "https://example.com/a\rb",
            "https://example.com/a\u{0}b",
            "https://example.com/a\tb",
        ] {
            assert!(
                !release_url_is_openable(bad),
                "校验必须拒绝（纯判据，不触达任何进程拉起）：{bad:?}"
            );
            assert!(
                open_external_url(bad).is_err(),
                "必须拒绝（且不能拉起任何进程）：{bad:?}"
            );
        }
    }

    /// 合法 http(s) 且无控制字符的地址应通过校验；`&` 是合法 URL 字符，
    /// 在 ShellExecuteW 路径上不得被误拒（cmd 路线里它才是元字符）。
    #[test]
    fn release_url_validation_accepts_clean_http_urls() {
        for ok in [
            "https://example.com/releases/v0.2.0",
            "http://example.com/x?a=1&b=2",
            "  https://example.com/  ",
            "https://github.com/gqf2008/audio-workshop/releases/latest",
        ] {
            assert!(
                release_url_is_openable(ok),
                "合法发布页地址应通过校验：{ok:?}"
            );
        }
    }

    /// 真机（默认 ignored）：**批量配音走 worker 的命令通道**——N 篇稿子依次成片，
    /// 每篇落到自己的工程目录，第二次跑同一批时已合成的句子要被复用。
    ///
    /// 批量比单篇多两件必须钉住的事：① 每篇都有自己的 task_id（任务中心靠它对条目）
    /// 与自己的工程目录（不能互相覆盖）；② 重跑同一批要复用已合成的句子——
    /// 不然"批量重跑"就等于把算力再烧一遍。
    /// 一批跑完，测试里要看的东西（写成结构体而不是四元组：clippy 的
    /// type_complexity 不是在挑刺——四个匿名字段的元组确实读到调用点就没人认得了）
    struct BatchRunResult {
        summary: (usize, usize, usize, bool),
        started: Vec<u32>,
        outputs: Vec<(PathBuf, PathBuf, usize)>,
        progress_events: usize,
    }

    #[test]
    #[ignore = "需要本机 audiocpp_server + audio8-tts"]
    fn worker_batch_runs_two_scripts_end_to_end() {
        let root = std::env::temp_dir().join(format!("aw-worker-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let handle = std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop: Arc::new(AtomicBool::new(false)),
                sep_stop: Arc::new(AtomicBool::new(false)),
                eval_stop: Arc::new(AtomicBool::new(false)),
                projects_root: root,
                cancel: cancel::CancelRegistry::new(),
            })
        });

        // 跑一批（两篇），返回 (汇总, 见过的 task_id, 每篇产物, 句级进度回调次数)
        //
        // 进度回调次数是"这次到底合成没有"的证据：`synthesize` 对 status == "done"
        // 的句子是 `continue`（先跳过、后回调），所以重跑同一批时回调次数必须是 0——
        // 那才是断点续作，而不是"又烧了一遍算力、只是结果一样"。
        let run_batch = |first_task_id: u32| -> BatchRunResult {
            cmd_tx
                .send(Cmd::RunBatch {
                    revision: 1,
                    model: "audio8-tts".into(),
                    voice_ref: None,
                    voice_ref_text: None,
                    gap_ms: GAP_MS,
                    auto_normalize: true,
                    dict: empty_dict(),
                    items: vec![
                        BatchCmdItem {
                            task_id: first_task_id,
                            name: "批量甲".into(),
                            script: "第一句。第二句。".into(),
                        },
                        BatchCmdItem {
                            task_id: first_task_id + 1,
                            name: "批量乙".into(),
                            script: "第三句。第四句。".into(),
                        },
                    ],
                })
                .unwrap();
            let mut out: Vec<(PathBuf, PathBuf, usize)> = Vec::new();
            let mut started: Vec<u32> = Vec::new();
            let mut progress_events = 0usize;
            let summary = loop {
                let m = msg_rx.recv().expect("worker 应有消息");
                match m.msg {
                    Msg::BatchItemStarted { task_id, .. } => started.push(task_id),
                    Msg::BatchItemProgress { .. } => progress_events += 1,
                    Msg::BatchItemDone {
                        task_id,
                        wav,
                        srt,
                        skipped,
                        error,
                        reused,
                        ..
                    } => {
                        assert!(!skipped, "没登记取消，不该跳过");
                        assert!(error.is_none(), "批量某一篇失败：{error:?}");
                        assert_eq!(
                            task_id,
                            first_task_id + out.len() as u32,
                            "消息要带对各自的 task_id"
                        );
                        out.push((wav.expect("成品路径"), srt.expect("字幕路径"), reused));
                    }
                    Msg::BatchDone {
                        done,
                        failed,
                        skipped,
                        stopped,
                    } => break (done, failed, skipped, stopped),
                    _ => {}
                }
            };
            BatchRunResult {
                summary,
                started,
                outputs: out,
                progress_events,
            }
        };

        let first = run_batch(21);
        assert_eq!(first.summary, (2, 0, 0, false), "两篇都要成");
        assert_eq!(
            first.started,
            vec![21, 22],
            "每一篇都要有自己的 TaskStarted"
        );
        assert!(
            first.progress_events >= 4,
            "第一次跑每篇两句都要有进度回调（实得 {} 次）",
            first.progress_events
        );
        assert_eq!(first.outputs.len(), 2);
        for (wav, srt, reused) in &first.outputs {
            assert_eq!(*reused, 0, "第一次跑不该有复用");
            assert!(wav.exists(), "成品不存在：{}", wav.display());
            assert!(srt.exists(), "字幕不存在：{}", srt.display());
            let secs = aw_core::dub::wav_duration(&std::fs::read(wav).unwrap()).unwrap();
            assert!(secs > 0.0, "成品时长为 0：{}", wav.display());
        }
        assert_ne!(
            first.outputs[0].0, first.outputs[1].0,
            "两篇要落两个工程目录"
        );

        // 再跑同一批：已经合成好的句子必须被跳过（断点续作在批量里的同一条保证）
        let second = run_batch(31);
        assert_eq!(second.summary, (2, 0, 0, false));
        assert_eq!(
            second.progress_events, 0,
            "重跑没有该重做的句子：零进度回调才说明一句都没再合成"
        );
        for (wav, _, reused) in &second.outputs {
            assert_eq!(
                *reused,
                0,
                "稿件没变时是「整篇原样载入」，不走逐句继承：{}",
                wav.display()
            );
        }

        drop(cmd_tx);
        handle.join().unwrap();
    }
}
