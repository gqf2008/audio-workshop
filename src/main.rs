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
//!   <工程名>.wav / <工程名>.srt          导出（复制自 out/）

mod batch;
mod cancel;
mod export;
mod player;
mod tasks;
mod templates;
mod versions;

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
    assemble_bgm, bgm_only_artifacts, generate_segments_stoppable, generate_song, mix_project,
    BgmArtifacts, BgmOptions, BgmRun, Client, Project, SongModel, SongOptions, DEFAULT_PUNCTUATION,
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
        project_name: String,
        /// 句间静音（毫秒）：只影响拼装出来的时间轴，不影响逐句音频
        gap_ms: u64,
        /// 文本兜底（数字/年份规范化）开关：影响 spoken 文本 → 变了要重录
        auto_normalize: bool,
    },
    /// 单句重录（换 seed 重合成该句）
    Redo { revision: u64, index: usize },
    /// 批量配音（M4-P1）：按顺序把 N 篇稿子跑成 N 个工程。
    ///
    /// 与单篇共用同一条链路（load_resumable → synthesize → assemble）与同一份
    /// `projects_root`；每条自带 task_id，所以任务中心里是 N 条独立的配音任务、
    /// 排队中的那几条也能单独取消（取消登记表按 task_id 定位）。
    RunBatch {
        revision: u64,
        model: String,
        voice_ref: Option<String>,
        /// 与单篇同一套：句间静音 + 文本兜底开关（批量产物也受它们影响）
        gap_ms: u64,
        auto_normalize: bool,
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
        text: String,
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
    /// 一轮合成结束：失败句数、是否被停止
    RunDone {
        failed: usize,
        stopped: bool,
        reused: usize,
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
    /// 全局设置里「选择模型目录」的结果
    ModelDirPicked {
        path: Option<String>,
    },
    /// 人声分离进度 / 终态（都带 task_id 以便与当前任务对齐）
    SeparationProgress {
        task_id: u32,
        percent: f32,
        note: String,
    },
    SeparationDone {
        task_id: u32,
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
    /// 批量导出跑完了（后台线程回报；不是 worker 任务，不进任务台账）
    BatchExportDone {
        /// 导出到哪个目录（消息自带，别在用的时候再读一次界面值——那个值可能已经变了）
        dir: PathBuf,
        outcome: export::BatchExportOutcome,
    },
    /// 批量：系统文件框选完的多篇稿件（取消 = 空表）
    BatchScriptsPicked {
        paths: Vec<PathBuf>,
    },
    /// 选择待分离音频的结果
    SeparationInputPicked {
        path: Option<String>,
    },
    /// 音色试听失败（保留音色名，便于在状态栏说清是哪个音色挂了）
    VoicePreviewFailed {
        label: String,
        error: String,
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
    /// 平均可懂度百分比（只算转写成功的句子）
    percent: f64,
    scored: usize,
    /// ASR 转写失败的句数（服务端错误等）——不混进平均分，但要报出来
    asr_failed: usize,
    /// 最差 N 句（按可懂度升序；只含有差异的句子）
    worst: Vec<EvalIssue>,
    /// 全部评上的分数（句 index → 可懂度%），给句子行展示用
    scores: Vec<(usize, f64)>,
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
    /// 模型目录：本机模型文件的存放位置（默认 <应用工作目录>/models，用户可选）
    #[serde(default)]
    model_dir: Option<String>,
    /// BGM 生成的输入（描述 / 压低档位 / 独立生成时长档位）。
    ///
    /// 以前这些只活在 UI 内存里：重启回默认值，用户写的描述白写；跨会话也没法判断
    /// "磁盘上那套 BGM 还算不算当前结果"，只能一律当作过期。落进 settings.json 之后
    /// 两件事一起解决（判据见 src/export.rs 的产物清单）。
    #[serde(default)]
    bgm: BgmSettings,
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

/// 默认模型目录：应用工作目录下的 models/（打包后即应用目录下的 models/）。
fn default_model_dir() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    // cwd 是 `/`（Finder 双击启动的常见情况）时退回可执行文件所在目录：
    // 否则默认值成了 `/models`，界面只说"目录不存在"，看不出根因。
    let base = match cwd {
        Some(d) if d != Path::new("/") => d,
        _ => exe_dir.unwrap_or_else(|| PathBuf::from(".")),
    };
    base.join("models")
}

/// 当前生效的模型目录（设置 > 默认）。
fn model_dir() -> PathBuf {
    settings_snapshot()
        .model_dir
        .map(PathBuf::from)
        .unwrap_or_else(default_model_dir)
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
fn models_under_dir(cfg: &Option<ServerConfig>, dir: &Path) -> usize {
    let Some(cfg) = cfg else { return 0 };
    cfg.models
        .iter()
        .filter(|m| !m.path.is_empty() && Path::new(&m.path).starts_with(dir))
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
        speed,
        gap_ms,
        auto_normalize,
    })
}

/// 当前工程的输入（用来和模板比对，算出"应用后要作废什么"）。
fn project_inputs_from_ui(ui: &MainWindow) -> templates::ProjectInputs {
    let model = current_model_name(ui).unwrap_or_default();
    templates::ProjectInputs {
        model,
        voice_ref: non_empty(ui.get_voice_ref_path().to_string()),
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
    let Some(raw) = std::fs::read_to_string(settings_path()).ok() else {
        return AppSettings::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return AppSettings::default();
    };
    AppSettings {
        host: json_field(&v, "host"),
        port: json_field(&v, "port"),
        model_dir: json_field(&v, "model_dir"),
        bgm: json_field(&v, "bgm").unwrap_or_default(),
    }
}

/// 取一个字段；缺了或类型不对都返回 None（交给该字段自己的默认值）。
fn json_field<T: serde::de::DeserializeOwned>(v: &serde_json::Value, key: &str) -> Option<T> {
    v.get(key)
        .and_then(|x| serde_json::from_value(x.clone()).ok())
}

fn save_settings(s: &AppSettings) -> std::io::Result<()> {
    let path = settings_path();
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

#[derive(serde::Deserialize)]
struct ServerConfig {
    host: Option<String>,
    port: Option<u16>,
    models: Vec<ServerModel>,
}

#[derive(serde::Deserialize)]
struct ServerModel {
    id: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    family: String,
    #[serde(default)]
    path: String,
}

/// 读取 server.json：音色 = task=="tts" 的模型；地址取 AW_SERVER，否则 host:port。
/// 文件缺失/解析失败返回空清单 + 原因说明（不 panic：服务没配时界面也可打开）。
fn discover_engine() -> (Vec<Voice>, Option<String>, String) {
    let cfg_path = config_path();
    let raw = std::fs::read_to_string(&cfg_path).ok();
    let over = settings_snapshot();
    let cfg = raw.as_deref().and_then(|r| {
        serde_json::from_str::<ServerConfig>(r)
            .map_err(|e| e.to_string())
            .ok()
    });
    // 与界面回显共用同一个解析入口（见 resolve_base 的注释）
    let env_base = std::env::var("AW_SERVER").ok();
    let (base_url, _) = resolve_base(&over, &cfg, env_base.as_deref());
    let base = has_endpoint_source(&over, &cfg, env_base.as_deref()).then_some(base_url);
    let Some(raw) = raw else {
        return (Vec::new(), base, format!("没找到 {}", cfg_path.display()));
    };
    let cfg: ServerConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return (Vec::new(), base, format!("server.json 解析失败: {e}")),
    };
    let voices = cfg
        .models
        .iter()
        .filter(|m| m.task == "tts")
        .map(|m| Voice {
            name: m.id.clone().into(),
            engine: format!("{} · 本地", m.family).into(),
            note: short_path(&m.path).into(),
            license: "仅自用".into(),
        })
        .collect();
    (voices, base, String::new())
}

/// 清单里的模型总数（读不到清单就是 0，不算错误）。
fn config_summary() -> usize {
    read_server_config().map(|c| c.models.len()).unwrap_or(0)
}

/// 读模型清单（读不到就是"没有清单"）。
fn read_server_config() -> Option<ServerConfig> {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<ServerConfig>(&raw).ok())
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
    let names: Vec<SharedString> = voices.iter().map(|v| v.name.clone()).collect();
    ui.set_voice_names(ModelRc::from(Rc::new(VecModel::from(names))));
    ui.set_voices(ModelRc::from(Rc::new(VecModel::from(voices))));

    let pick = keep
        .and_then(|name| {
            (0..ui.get_voice_names().row_count()).find(|&i| {
                ui.get_voice_names()
                    .row_data(i)
                    .map(|n| n == name.as_str())
                    .unwrap_or(false)
            })
        })
        .or_else(|| {
            (0..ui.get_voice_names().row_count()).find(|&i| {
                ui.get_voice_names()
                    .row_data(i)
                    .map(|n| n == "audio8-tts")
                    .unwrap_or(false)
            })
        })
        .map(|i| i as i32)
        .unwrap_or(-1);
    let changed = pick != ui.get_voice_index();
    ui.set_voice_index(pick);
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

/// 系统目录选择框：阻塞式原生对话框，必须放后台线程，结果回消息通道。
///
/// macOS 用 osascript 的 choose folder；Windows 用 PowerShell 的
/// FolderBrowserDialog；Linux 用 zenity。用户取消 → None，不静默失败。
fn spawn_folder_pick(msg_tx: Sender<WorkerMsg>, revision: u64) {
    std::thread::spawn(move || {
        let path = pick_folder_blocking();
        let _ = msg_tx.send(WorkerMsg {
            revision,
            msg: Msg::ModelDirPicked { path },
        });
    });
}

/// 选一段待分离音频（系统文件框，后台线程 + 消息回传）。
fn spawn_file_pick(msg_tx: Sender<WorkerMsg>) {
    std::thread::spawn(move || {
        let path = pick_audio_blocking();
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::SeparationInputPicked { path },
        });
    });
}

fn pick_audio_blocking() -> Option<String> {
    #[cfg(target_os = "macos")]
    let out = std::process::Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (choose file with prompt \"选择要分离的音频\")",
        ])
        .output()
        .ok()?;

    #[cfg(target_os = "windows")]
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms | Out-Null; \
             $d = New-Object System.Windows.Forms.OpenFileDialog; \
             $d.Filter = '音频|*.wav;*.mp3;*.flac;*.m4a;*.ogg'; \
             if ($d.ShowDialog() -eq \"OK\") { Write-Output $d.FileName }",
        ])
        .output()
        .ok()?;

    #[cfg(all(unix, not(target_os = "macos")))]
    let out = std::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--title=选择要分离的音频",
            "--file-filter=音频 | *.wav *.mp3 *.flac *.m4a *.ogg",
        ])
        .output()
        .ok()?;

    pick_output_to_path(out)
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

/// 批量导入：系统多选文件框，阻塞式，必须放后台线程（与目录/单文件选择器同款）。
fn spawn_scripts_pick(msg_tx: Sender<WorkerMsg>) {
    std::thread::spawn(move || {
        let paths = pick_scripts_blocking();
        let _ = msg_tx.send(WorkerMsg {
            revision: 0,
            msg: Msg::BatchScriptsPicked { paths },
        });
    });
}

/// 多选稿件对话框的输出（按平台各一份实现；见 `pick_scripts_blocking`）。
#[cfg(target_os = "macos")]
fn scripts_dialog_output() -> Option<std::process::Output> {
    std::process::Command::new("osascript")
        .args([
            "-e",
            "set fs to choose file with prompt \"选择稿件（可多选）\" with multiple selections allowed",
            "-e",
            "set out to \"\"",
            "-e",
            "repeat with f in fs",
            "-e",
            "set out to out & (POSIX path of f) & linefeed",
            "-e",
            "end repeat",
            "-e",
            "return out",
        ])
        .output()
        .ok()
}

#[cfg(target_os = "windows")]
fn scripts_dialog_output() -> Option<std::process::Output> {
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms | Out-Null; \
             $d = New-Object System.Windows.Forms.OpenFileDialog; \
             $d.Multiselect = $true; \
             $d.Filter = '文本稿件|*.txt;*.md|所有文件|*.*'; \
             if ($d.ShowDialog() -eq \"OK\") { $d.FileNames | ForEach-Object { Write-Output $_ } }",
        ])
        .output()
        .ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn scripts_dialog_output() -> Option<std::process::Output> {
    std::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--multiple",
            "--separator=\n",
            "--title=选择稿件（可多选）",
        ])
        .output()
        .ok()
}

/// 多选稿件（一次导入 N 篇是 P1 的入口）。
///
/// 故意**不**在系统对话框里按扩展名过滤：稿件可能是 .txt / .md / 无扩展名，让用户在
/// 对话框里"看不到自己的文件"比进来之后告诉他"这个文件不是 UTF-8 文本"更差。
/// 过滤与跳过理由都在 `batch::import_scripts`，一处判定。
fn pick_scripts_blocking() -> Vec<PathBuf> {
    match scripts_dialog_output() {
        Some(out) => parse_picked_paths(out.status.success(), &out.stdout),
        None => Vec::new(),
    }
}

/// 多选对话框的输出 → 路径表。
///
/// 取消（退出码非 0）必须是**空表**而不是"一个空路径"：后者会让导入把一次取消
/// 记成"跳过了 1 篇（读不到）"。空路径同样过滤掉。
fn parse_picked_paths(success: bool, stdout: &[u8]) -> Vec<PathBuf> {
    if !success {
        return Vec::new();
    }
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn pick_folder_blocking() -> Option<String> {
    #[cfg(target_os = "macos")]
    let out = std::process::Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"选择模型目录\")",
        ])
        .output()
        .ok()?;

    #[cfg(target_os = "windows")]
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms | Out-Null; \
             $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
             if ($d.ShowDialog() -eq \"OK\") { Write-Output $d.SelectedPath }",
        ])
        .output()
        .ok()?;

    #[cfg(all(unix, not(target_os = "macos")))]
    let out = std::process::Command::new("zenity")
        .args(["--file-selection", "--directory", "--title=选择模型目录"])
        .output()
        .ok()?;

    pick_output_to_path(out)
}

/// 系统选择框的输出 → 路径（取消时退出码非 0，返回 None，不静默失败）。
fn pick_output_to_path(out: std::process::Output) -> Option<String> {
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        None
    } else {
        Some(raw.trim_end_matches('/').to_string())
    }
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
                text,
            } => {
                let msg = match make_client() {
                    Ok(client) => {
                        // instruction 与配音链路一致（aw_core 合成恒定传 DEFAULT_INSTRUCTION）：
                        // 试听听到的语气必须等于成品，否则用户按试听选音色会被误导。
                        match client.synth(
                            &model,
                            &text,
                            Some(BASE_SEED),
                            voice_ref.as_deref(),
                            Some(aw_core::DEFAULT_INSTRUCTION),
                        ) {
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
            Cmd::Run {
                revision,
                task_id,
                script,
                model,
                voice_ref,
                project_name,
                gap_ms,
                auto_normalize,
            } => {
                if task_take_started(&ctx, task_id) {
                    // 排队期间被停掉：不载入、不合成，按"用户停止"收尾
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::RunDone {
                            failed: 0,
                            stopped: true,
                            reused: 0,
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
                    gap_ms,
                    auto_normalize,
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
                let tx = ctx.tx.clone();
                let started = RefCell::new(std::collections::HashSet::new());
                let run = project.synthesize_stoppable(
                    &client,
                    &dir,
                    None,
                    None,
                    Some(&ctx.stop),
                    |idx, note| {
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
            Cmd::RunBatch {
                revision,
                model,
                voice_ref,
                gap_ms,
                auto_normalize,
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
                        gap_ms,
                        auto_normalize,
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
                let mut scores: Vec<(usize, f64)> = Vec::new();
                let mut rows: Vec<EvalRow> = Vec::new();
                let mut sum = 0.0f64;
                let mut scored = 0usize;
                let mut asr_failed = 0usize;
                let mut stopped = false;
                for (n, idx) in done.iter().enumerate() {
                    // 协作式停止：一句转写完再停（ASR 调用本身中断不了）
                    if ctx.eval_stop.load(Ordering::Relaxed) {
                        stopped = true;
                        break;
                    }
                    let wav = dir.join(format!("sentences/{idx:03}.wav"));
                    match client.asr(&wav) {
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
                            scores.push((*idx, score.percent));
                            rows.push(EvalRow {
                                index: *idx,
                                reference: reference.clone(),
                                hypothesis: hypothesis.clone(),
                                percent: score.percent,
                                snippet: snippet.clone(),
                            });
                            // 顺手写进工程：质检结果要能跨会话留存（否则每次开都要重跑 N 句 ASR）
                            if let Some(sen) =
                                project.sentences.iter_mut().find(|s| s.index == *idx)
                            {
                                sen.eval_percent = Some(score.percent);
                            }
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
                            // 单句转写失败不致命：记数并在汇总里如实报出来，不混进平均分。
                            // 这句的旧分数（如果有）**保留**——它描述的是磁盘上那段音频，
                            // 而这次只是没测到；保留的分数要一起回给 UI，否则 UI 与磁盘不一致。
                            asr_failed += 1;
                            let _ = e;
                        }
                    }
                    let _ = ctx.tx.send(WorkerMsg {
                        revision: 0,
                        msg: Msg::EvalProgress {
                            task_id,
                            done: n + 1,
                            total,
                        },
                    });
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
                for sen in &project.sentences {
                    if let Some(percent) = sen.eval_percent {
                        if sen.status == "done" && !scores.iter().any(|(i, _)| *i == sen.index) {
                            scores.push((sen.index, percent));
                        }
                    }
                }
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
                    "qwen3-asr",
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
                            percent,
                            scored,
                            asr_failed,
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
                let model = match model.as_str() {
                    "ace-step" => SongModel::AceStep,
                    _ => SongModel::Yue2,
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
                    None,
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
            ("done", d)
        } else if note.starts_with("error") {
            ("error", None)
        } else {
            ("running", None)
        };
        let _ = tx.send(WorkerMsg {
            revision,
            msg: Msg::Sentence {
                index: idx,
                status: status.into(),
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

/// 工程目录：~/Documents/音频作坊/projects/<stem>/
fn projects_root() -> PathBuf {
    documents_dir().join(WORKSHOP_DIR).join("projects")
}

fn project_dir(stem: &str) -> PathBuf {
    projects_root().join(stem)
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
) -> bool {
    saved.voice_ref.as_ref() == voice_ref.as_ref()
        && saved.voice_ref_hash.as_ref() == voice_ref_hash.as_ref()
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
        std::fs::File::open(&temp.0)
            .and_then(|f| f.sync_all())
            .map_err(|e| {
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
    gap_ms: u64,
    auto_normalize: bool,
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
                aw_core::normalize(t, &Default::default())
            } else {
                // 关掉兜底 = 原文照念（数字/年份交给引擎自己处理）
                t.to_string()
            }
        },
    );
    project.auto_normalize = auto_normalize;
    // 参考音哈希**不在这里算**：`load_resumable` 已经算过一份（算两遍纯属浪费），
    // 版本留档那边由 `project_from_ui` 自己补。这里只管"按输入造工程"。
    project
}

fn load_resumable(
    dir: &Path,
    script: &str,
    model: &str,
    voice_ref: Option<String>,
    gap_ms: u64,
    auto_normalize: bool,
) -> Result<LoadedProject, String> {
    let voice_ref_hash = match voice_ref.as_deref() {
        Some(path) => Some(sha256_file(Path::new(path))?),
        None => None,
    };
    // 损坏的工程在这里必须**中止**：`.ok()` 会把它当成"没有工程"，已合成句全变待合成，
    // 随后第一次落盘还会覆盖掉损坏文件（现场丢失）。见 Project::load_if_present。
    let saved = Project::load_if_present(dir)?;
    if let Some(saved) = saved.as_ref() {
        // 兜底开关与模型/音色同类：它变了，spoken 文本就变，旧音频不能算数。
        // 停顿不进这个条件——它只影响拼装，改了不必重录（下面就直接改字段）。
        if saved.model == model
            && saved.auto_normalize == auto_normalize
            && voice_ref_matches(saved, &voice_ref, &voice_ref_hash)
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
    let mut project =
        new_project_from_inputs(script, model, voice_ref.clone(), gap_ms, auto_normalize);
    // 用上面已经算好的哈希（少读一次参考音文件）
    project.voice_ref_hash = voice_ref_hash.clone();
    let reused = if let Some(saved) = saved.as_ref() {
        // 与快路径同一条判据：开关变了就不能逐句继承（旧音频念的是另一套文本）
        if saved.model == model
            && saved.auto_normalize == auto_normalize
            && voice_ref_matches(saved, &voice_ref, &voice_ref_hash)
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
    /// 人声分离：当前输入路径（用于 stale 判定）与两轨产物
    sep_input: RefCell<Option<String>>,
    sep_tracks: RefCell<Option<(PathBuf, PathBuf)>>,
    sep_task: std::cell::Cell<Option<u32>>,
    /// 质检任务的 id（配音页那一条）
    eval_task: std::cell::Cell<Option<u32>>,
    /// 质检分数（句 index → 可懂度%）。重新合成/改稿后要清掉——分数会失效
    eval_scores: RefCell<HashMap<usize, f64>>,
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
    /// 「数字/年份规范化」开关上次被 tick 看到的值。PixelSwitch 没有回调，
    /// 靠它发现"用户切换了"→ 作废工程（spoken 文本会变，旧音频不能算数）。
    auto_normalize_seen: std::cell::Cell<bool>,
    /// 任务中心里"已排队 / 已运行 N"的上次刷新时刻：40ms 的 tick 不能每次都重建模型。
    last_task_refresh: std::cell::Cell<Option<Instant>>,
    /// 截图/演示态（`AW_UI_STATE=tasks`）。演示任务只是给任务中心摆样子、没有对应的
    /// worker 命令，所以**不能参与**"有没有任务在飞"的判断——否则演示态下点开始配音
    /// 会被这些假任务挡住（复核指出）。只有 debug 构建会置位。
    demo_tasks: std::cell::Cell<bool>,
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;

    let rows: Rc<VecModel<Sentence>> = Rc::new(VecModel::default());

    // ── 引擎发现 → 模型清单（默认优先 audio8-tts）──
    let (_, base, discover_note) = discover_engine();
    if !discover_note.is_empty() {
        ui.set_status_text(format!("引擎发现: {discover_note}").into());
    }
    apply_engine_discovery(&ui, None);

    ui.set_export_dir(export_dir().into());
    apply_bgm_settings(&ui);
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
    wire_voice_panel(&ui, &cmd_tx, &state);
    wire_global_settings(&ui, &msg_tx_ui, &cmd_tx, &state);
    wire_sentence_actions(&ui, &rows, &cmd_tx, &player, &state);
    wire_run(&ui, &rows, &cmd_tx, &player, &stop, &state);
    wire_export(&ui, &cmd_tx, &msg_tx_ui, &state);
    wire_bgm(&ui, &cmd_tx, &state, &player, &stop);
    wire_song(&ui, &cmd_tx, &state, &player);
    wire_keys(&ui, &rows, &player, &state);
    wire_task_center(&ui, &state, &stop, &sep_stop, &eval_stop);
    wire_batch(&ui, &cmd_tx, &msg_tx_ui, &state, &stop);
    wire_separation(&ui, &msg_tx_ui, &cmd_tx, &state, &player, &sep_stop);
    wire_quality_check(&ui, &cmd_tx, &state, &eval_stop);

    // 启动就把"任务 · 空闲"画上（状态栏 chip 与任务中心都读同一份台账）
    refresh_tasks(&ui, &state);
    #[cfg(debug_assertions)]
    seed_shot_tasks(&ui, &state);
    seed_shot_batch(&ui, &state);
    #[cfg(debug_assertions)]
    seed_shot_bgm_artifacts(&ui, &state);

    // 产截图 / 演示用初始态（仅 debug；release 无此旁路）
    apply_shot_state(&ui);

    // ── 主循环 ──
    let timer = Timer::default();
    {
        let weak = ui.as_weak();
        let rows = rows.clone();
        let msg_rx = Rc::new(RefCell::new(msg_rx));
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(TICK_MS),
            move || {
                if let Some(ui) = weak.upgrade() {
                    tick(&ui, &rows, &msg_rx, &player, &state);
                }
            },
        );
    }

    ui.run()
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
fn apply_shot_state(ui: &MainWindow) {
    let Ok(state) = std::env::var("AW_UI_STATE") else {
        return;
    };
    match state.as_str() {
        "selected" => {
            ui.set_selected(1);
            ui.set_status_text("已选中第 2 句 · 试听 / 重录就在行下方".into());
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
        "voice" => {
            ui.set_dub_voice(true);
            refresh_voice_labels(ui);
            ui.set_status_text("音色：内置默认 / 参考音频克隆；换音色不是换模型".into());
        }
        "design" => {
            ui.set_scene(4);
            ui.set_status_text("音色设计：参考音频克隆可用；文本生成音色未接入".into());
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
fn apply_shot_state(_ui: &MainWindow) {}

fn apply_project_to_rows(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    project: &Project,
) -> usize {
    let mut done = 0usize;
    for (i, sentence) in project.sentences.iter().enumerate() {
        if i >= rows.row_count() {
            break;
        }
        if sentence.status == "done" {
            done += 1;
            set_status(rows, i, "done");
            if let Some(d) = sentence.duration {
                set_row_duration(rows, i, d as f32);
            }
        } else if sentence.status.starts_with("error") {
            set_status(rows, i, "error");
        } else {
            set_status(rows, i, "pending");
        }
    }
    recompute_total(rows);
    ui.set_done_count(done as i32);
    ui.set_progress(done as f32 / rows.row_count().max(1) as f32);
    ui.set_has_result(done > 0);
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
    ui.set_voice_ref_path(project.voice_ref.clone().unwrap_or_default().into());
    refresh_voice_labels(ui);
    // 工程里记着的两个输入回灌界面：兜底开关（决定怎么念）与句间停顿（决定拼装）
    ui.set_auto_normalize(project.auto_normalize);
    state.auto_normalize_seen.set(project.auto_normalize);
    ui.set_gap_ms_text(project.gap_ms.to_string().into());
    let done = apply_project_to_rows(ui, rows, &project);
    // 质检分数是句级持久化的：启动就把它们贴回行上（否则"重开还能看到"要等下一次合成）
    *state.eval_scores.borrow_mut() = scores_from_project(&project);
    apply_eval_labels(rows, &state.eval_scores.borrow());
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

/// 稿件变了（改稿/载入示例/清空/重新切句）：句子序号与内容都会变，质检分数一律作废。
///
/// 注意**不**放进 `invalidate_worker_project`：工程改名不改句子，分数仍然有效。
fn clear_eval_scores(state: &Rc<UiState>) {
    if !state.eval_scores.borrow().is_empty() {
        state.eval_scores.borrow_mut().clear();
    }
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
        clear_eval_scores(&state1);
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
        clear_eval_scores(&state2);
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
        clear_eval_scores(&state3);
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
        clear_eval_scores(&state4);
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
    matches!(status, "running" | "done" | "error")
}

/// 从工程里取质检分数：**只接受状态是「已合成」的句子**。
///
/// 失败/待合成的句子即使文件里还留着旧分数也不贴出来——那种分数描述的不是当前这句
/// 可用的音频（复核建议：回灌要按状态过滤）。
fn scores_from_project(project: &Project) -> HashMap<usize, f64> {
    project
        .sentences
        .iter()
        .filter(|s| s.status == "done")
        .filter_map(|s| s.eval_percent.map(|p| (s.index, p)))
        .collect()
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
        let label = scores.get(&i).map(|p| eval_label(*p)).unwrap_or_default();
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
    // ……再追加"东西有没有落盘"。**两种分支都要走到这里**：一句都没评上分时报告照样写了，
    // 写失败/分数没落盘也得让用户看见（复核指出旧写法在 scored==0 时提前 return，把这些吞了）。
    if let Some(warn) = &summary.persist_warning {
        note.push_str(&format!("·（{warn}）"));
    }
    if let Some(path) = &summary.report_path {
        note.push_str(&format!("·报告 {}", file_label(path)));
    }
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
        let voice_ref = non_empty(ui.get_voice_ref_path().to_string());

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
            format!("批量已提交：{total} 篇按顺序跑（任务中心能看到每一条）").into(),
        );
        refresh_tasks(&ui, &st);
        refresh_batch_rows(&ui, &st);
        refresh_batch_summary(&ui, &st, "已提交，按顺序跑");
        if tx
            .send(Cmd::RunBatch {
                revision: st.project_revision.get(),
                model,
                voice_ref,
                gap_ms: gap_ms_from_ui(&ui),
                auto_normalize: ui.get_auto_normalize(),
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
fn project_from_ui(ui: &MainWindow) -> Project {
    let voice_ref = non_empty(ui.get_voice_ref_path().to_string());
    let mut project = new_project_from_inputs(
        &ui.get_script_text(),
        &current_model_name(ui).unwrap_or_default(),
        voice_ref.clone(),
        gap_ms_from_ui(ui),
        ui.get_auto_normalize(),
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
fn rollback_with_inheritance(dir: &Path, id: &str) -> Result<(Project, usize), String> {
    let inherited = match Project::load_if_present(dir) {
        Ok(p) => p,
        Err(e) => return Err(format!("当前工程读不出来，回滚已中止（不会覆盖现场）：{e}")),
    };
    let snapshot = versions::load_for_rollback(dir, id)?;
    let mut restored = project_from_version(&snapshot);
    let reused = match inherited.as_ref() {
        Some(saved) => reuse_done_sentences(&mut restored, saved, dir)?,
        None => 0,
    };
    versions::commit_rollback(dir, &restored)?;
    Ok((restored, reused))
}

/// 版本快照 → 可继续合成的工程。
///
/// 快照里的句子状态是留档那一刻的（全 pending），这里**按稿件与设置重建**，
/// 句级状态交给调用方用"按文本继承"补——保持"状态由合成决定"这条不变式。
fn project_from_version(snapshot: &Project) -> Project {
    let script: String = snapshot.sentences.iter().map(|s| s.text.as_str()).collect();
    let mut project = new_project_from_inputs(
        &script,
        &snapshot.model,
        snapshot.voice_ref.clone(),
        snapshot.gap_ms,
        snapshot.auto_normalize,
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
        let project = project_from_ui(&ui);
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
                let current = project_from_ui(&ui);
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
        match rollback_with_inheritance(&dir, &id) {
            Ok((project, reused)) => {
                // 界面回灌：稿件 + 引擎/音色/停顿/兜底，全部走与手工编辑同一条路径
                let script: String = project.sentences.iter().map(|s| s.text.as_str()).collect();
                ui.set_script_text(script.clone().into());
                // 句子行重算（与改稿同一条路径）：这里只有 ui，行模型由 wire_versions 传入
                rebuild(&ui, &rows, &script);
                ui.set_voice_ref_path(project.voice_ref.clone().unwrap_or_default().into());
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
                clear_eval_scores(&st);
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
        ui.set_voice_ref_path(t.voice_ref.clone().unwrap_or_default().into());
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
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：参考音暂不可改".into());
            return;
        }
        invalidate_worker_project(&tx2, &state2);
        reset_bgm(&ui, &state2);
        ui.set_has_result(false);
        refresh_voice_labels(&ui);
        ui.set_status_text("参考音已变更：请重新开始合成，旧工程音频暂不可导出".into());
    });
}

/// 配音页内「音色」区：试听当前音色、清除参考音（切回内置音色）。
///
/// 概念区分（用户纠偏）：**换音色 ≠ 换模型**。
/// 音色只有两种来源——模型内置默认音色 / 参考音频克隆出来的音色；
/// 模型（audio8-tts / index-tts2 / 0.1b / stream…）是**引擎参数**，走 `model-changed`。
/// 单一事实来源：`voice-ref-path` 为空 = 内置默认音色，非空 = 克隆音色（不设第二个 mode 状态）。
fn wire_voice_panel(ui: &MainWindow, cmd_tx: &Sender<Cmd>, state: &Rc<UiState>) {
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
        let voice_ref = non_empty(ui.get_voice_ref_path().to_string());
        let what = if voice_ref.is_some() {
            "克隆音色"
        } else {
            "内置默认音色"
        };
        // 试听本身也是一条 worker 命令：置 busy 让"提交即运行中"的互斥任务（配音/BGM）
        // 在试听期间也被挡住，否则它们会排在试听后面却显示成已在跑。
        ui.set_busy(true);
        ui.set_status_text(format!("正在合成试听（{what} · {model}）…").into());
        if tx
            .send(Cmd::PreviewVoice {
                revision: st.project_revision.get(),
                model,
                voice_ref,
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
        ui.set_voice_ref_path("".into());
        invalidate_worker_project(&tx, &st);
        reset_bgm(&ui, &st);
        ui.set_has_result(false);
        refresh_voice_labels(&ui);
        ui.set_status_text("已切回内置默认音色：请重新开始合成".into());
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
    ui.on_select_sentence(move |i| {
        if i < 0 {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        ui.set_selected(i);
        if let Some(row) = model1.row_data(i as usize) {
            ui.set_status_text(
                format!(
                    "已选中第 {} 句 · 起始 {} · 时长 {}",
                    i + 1,
                    row.start_label,
                    row.duration_label
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
        let idx = i as usize;
        if model3.row_data(idx).is_none() {
            return;
        }
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
        ui.set_selected(i);
        ui.set_busy(true);
        start_task(
            &ui,
            &state3,
            &state3.redo_task,
            tasks::TaskKind::Dub,
            format!("重录第 {} 句", idx + 1),
        );
        if tx3
            .send(Cmd::Redo {
                revision: state3.project_revision.get(),
                index: idx,
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
        ui.set_status_text(format!("单句重录中：第 {} 句（换 seed 重跑）", i + 1).into());
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
        let model_name = selected_model(&ui);
        if model_name.is_empty() {
            ui.set_status_text("没有可用音色：检查 server.json / audiocpp_server".into());
            return;
        }
        // 续作语义：已合成句保持原样（worker 跳过 done 句），其余重跑
        let resume = ui.get_done_count() > 0;
        if !resume {
            for i in 0..n {
                set_status(&model4, i, "待合成");
            }
        }
        let voice_ref = non_empty(ui.get_voice_ref_path().to_string());
        if let Some(path) = voice_ref.as_deref() {
            if !Path::new(path).is_file() {
                ui.set_status_text(
                    format!("参考音频不存在或不可读：{path}（修正后再开始合成）").into(),
                );
                return;
            }
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
                project_name: stem,
                gap_ms: gap_ms_from_ui(&ui),
                auto_normalize: ui.get_auto_normalize(),
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
        ui.set_status_text(
            if resume {
                format!(
                    "继续合成 · {model_name} · 已完成的 {} 句自动跳过",
                    ui.get_done_count()
                )
            } else {
                format!("合成中 · {model_name}")
            }
            .into(),
        );
    });

    let weak = ui.as_weak();
    let stop2 = Arc::clone(stop);
    ui.on_stop_run(move || {
        let Some(ui) = weak.upgrade() else { return };
        stop_dub_run(&ui, &stop2);
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

fn wire_song(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
    player: &Rc<player::Player>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state1 = state.clone();
    ui.on_song_generate(move || {
        let Some(ui) = weak.upgrade() else { return };
        // 歌曲不依赖配音工程的可变状态（只复用工程目录写 song/），所以运行中也能
        // 提交：排进同一条队列。本 Tab 只有一个结果槽位，同一条歌曲还没结束时不接第二条。
        if state1.song_task.get().is_some() {
            ui.set_status_text("已有一首歌在队列里：等它结束，或去任务中心看进度".into());
            return;
        }
        let lyrics = ui.get_song_lyrics().to_string();
        let style = ui.get_song_style().to_string();
        if lyrics.trim().is_empty() || style.trim().is_empty() {
            ui.set_status_text("先填歌词和歌曲风格".into());
            return;
        }
        ui.set_song_busy(true);
        let task_id = enqueue_task(
            &ui,
            &state1,
            &state1.song_task,
            tasks::TaskKind::Song,
            "音乐制作 · 生成歌曲",
        );
        ui.set_song_has_result(false);
        let song_note = queue_note(&state1, task_id);
        // 真的排在别人后面才给"取消排队"：空闲提交时 worker 立刻接手，没有可取消的窗口
        ui.set_song_queued(song_note.is_some());
        ui.set_song_status_text(
            match song_note {
                Some(note) => format!("{note} · 轮到它时自动开始"),
                None => "歌曲生成中（yue2 可能约 8 分钟，ACE-Step 120s 约 6.4 分钟）…".to_string(),
            }
            .into(),
        );
        let model = if ui.get_song_model_index() == 1 {
            "ace-step"
        } else {
            "yue2"
        };
        if tx
            .send(Cmd::RunSong {
                revision: state1.project_revision.get(),
                task_id,
                project_name: file_stem(&ui.get_project_name()),
                model: model.into(),
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
            ui.set_status_text(format!("已选中第 {} 句", next + 1).into());
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
            ui.set_status_text(format!("已选中第 {} 句", next + 1).into());
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

fn message_ignores_revision(msg: &Msg) -> bool {
    matches!(
        msg,
        Msg::TaskStarted { .. }
        | Msg::TaskStage { .. }
        | Msg::ServerHealth { .. }
        | Msg::ModelDirPicked { .. }
        | Msg::SeparationInputPicked { .. }
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
    )
}

fn tick(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    msg_rx: &Rc<RefCell<Receiver<WorkerMsg>>>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
) {
    // ── 工作线程消息 ──
    let mut run_finished: Option<(usize, bool, usize)> = None;
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
                // 工程里的质检分数回灌（跨会话留存：重开应用不用重跑 ASR）
                *state.eval_scores.borrow_mut() = scores_from_project(&project);
                apply_project_to_rows(ui, rows, &project);
                apply_eval_labels(rows, &state.eval_scores.borrow());
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
                    let had = state.eval_scores.borrow_mut().remove(&index).is_some();
                    if had {
                        apply_eval_labels(rows, &state.eval_scores.borrow());
                    }
                }
                if let Some(d) = duration {
                    set_row_duration(rows, index, d as f32);
                    recompute_total(rows);
                }
                set_status(rows, index, &status);
                if status == "done" || status == "error" {
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
            } => {
                ui.set_busy(false);
                // 任务台账：停下来 = 已停止；有失败句 = 失败；否则完成
                let (task_state, detail) = if stopped {
                    (tasks::TaskState::Stopped, "用户停止".to_string())
                } else if failed > 0 {
                    (tasks::TaskState::Failed, format!("{failed} 句失败"))
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
                run_finished = Some((failed, stopped, reused));
            }
            Msg::RedoDone { index, error } => {
                ui.set_busy(false);
                match error {
                    Some(error) => {
                        set_status(rows, index, "error");
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
            Msg::ModelDirPicked { path } => match path {
                Some(p) => {
                    ui.set_model_dir(p.clone().into());
                    ui.set_status_text(
                        format!("已选中模型目录：{p}（点「应用并重连」生效）").into(),
                    );
                }
                None => {
                    ui.set_status_text("取消了选择模型目录".into());
                }
            },
            Msg::ServerHealth { ok, detail } => {
                ui.set_server_status(detail.into());
                ui.set_server_ok(ok);
                // 服务刚被改地址 / 重启过时，后端可能从 metal 变 cuda，标签要跟着走
                refresh_backend_label(ui);
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
            Msg::SeparationInputPicked { path } => match path {
                Some(p) => {
                    set_separation_input(ui, state, p);
                }
                None => {
                    ui.set_status_text("取消了选择音频".into());
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
                vocals,
                accompaniment,
            } => {
                if state.sep_task.get() == Some(task_id) {
                    ui.set_sep_busy(false);
                    ui.set_sep_progress(1.0);
                    ui.set_sep_has_result(true);
                    ui.set_sep_vocals_label(format!("人声 · {}", file_label(&vocals)).into());
                    ui.set_sep_accompaniment_label(
                        format!("伴奏 · {}", file_label(&accompaniment)).into(),
                    );
                    let note = "两轨已生成 · 可分别试听和导出".to_string();
                    ui.set_sep_status_text(note.clone().into());
                    ui.set_status_text(note.clone().into());
                    finish_task(ui, state, &state.sep_task, tasks::TaskState::Done, note);
                    *state.sep_tracks.borrow_mut() = Some((vocals, accompaniment));
                }
            }
            Msg::SeparationStopped { task_id } => {
                if state.sep_task.get() == Some(task_id) {
                    ui.set_sep_busy(false);
                    ui.set_sep_progress(0.0);
                    ui.set_sep_has_result(false);
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
                    ui.set_sep_status_text(error.clone().into());
                    ui.set_status_text(format!("人声分离失败：{error}").into());
                    finish_task(ui, state, &state.sep_task, tasks::TaskState::Failed, error);
                }
            }
            Msg::EvalProgress {
                task_id,
                done,
                total,
            } => {
                if state.eval_task.get() == Some(task_id) {
                    let note = format!("质检中：第 {done}/{total} 句");
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
                // ASR 失败但保留的旧分），所以这里是整体替换，与磁盘一致。
                {
                    let mut scores = state.eval_scores.borrow_mut();
                    scores.clear();
                    for (idx, percent) in &summary.scores {
                        scores.insert(*idx, *percent);
                    }
                }
                apply_eval_labels(rows, &state.eval_scores.borrow());
                let mut note = eval_summary_note(&summary);
                // 质检是用户主动发起的"找问题"动作：跑完直接把最差那句选中，
                // 用户当场就能点旁边的「重录」（列表不滚动，长稿还要自己滚一下）
                if let Some(worst) = summary.worst.first() {
                    ui.set_selected(worst.index as i32);
                    note.push_str(&format!("·已选中第 {} 句", worst.index + 1));
                }
                // 一句都没评上分 = 这次质检没得出结论，不能标成绿色的"完成"
                let outcome = if summary.scored == 0 {
                    tasks::TaskState::Failed
                } else {
                    tasks::TaskState::Done
                };
                finish_task(ui, state, &state.eval_task, outcome, note.clone());
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
            Msg::BatchScriptsPicked { paths } => {
                // 用户在文件框里按了取消（= 没选到任何路径）：这是 no-op，不能把已经
                // 导入好的那份列表清掉——"取消"不该有破坏性副作用（复核提的 UX 残留）。
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
    if let Some((failed, stopped, reused)) = run_finished {
        ui.set_running(false);
        let n = rows.row_count() as i32;
        let done = ui.get_done_count();
        ui.set_progress(done as f32 / n.max(1) as f32);
        let reused_note = if reused > 0 {
            format!("（复用 {reused} 句）")
        } else {
            String::new()
        };
        ui.set_status_text(
            if stopped {
                format!("已停止：完成 {done}/{n} 句{reused_note}，可随时继续")
            } else if failed > 0 {
                format!(
                    "本轮完成 {done}/{n} 句{reused_note}（{failed} 句失败：重跑自动重试失败句）"
                )
            } else {
                format!("合成完成 {done}/{n} 句{reused_note}：可试听、可导出 WAV / SRT")
            }
            .into(),
        );
        if done > 0 {
            ui.set_has_result(true);
        }
    }

    // ── 配音成品就绪 → UI（BGM 的混音前置条件）──
    // 单一真相是 state.assembled（拼装成功时置、作废工程时清），每 tick 同步一次；
    // 值没变时 Slint 不会重绘。
    ui.set_dub_product_ready(state.assembled.borrow().is_some());

    // ── 试听结束：rodio 队列播空 → 复位 playing ──
    if ui.get_playing() && !player.is_playing() {
        ui.set_playing(false);
        ui.set_status_text("试听结束".into());
    }

    // ── 兜底规则开关（PixelSwitch 没有回调，只能比对上一次的值）──
    sync_auto_normalize_toggle(ui, state);

    // ── 任务中心的"已排队 / 已运行 N"走字（按秒节流）──
    maybe_refresh_task_times(ui, state);
}

fn stem_of(ui: &MainWindow) -> String {
    file_stem(&ui.get_project_name())
}

// ===========================================================================
// 试听
// ===========================================================================

/// 逐句试听：播 sentences/NNN.wav，播放头按该句时长推进。
fn play_sentence(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
    i: usize,
    duration: f32,
) {
    if rows
        .row_data(i)
        .map(|row| row.status.as_str() != "已合成")
        .unwrap_or(true)
    {
        ui.set_status_text("这句还没合成：先点「开始合成」".into());
        return;
    }
    let Some(dir) = state.project_dir.borrow().clone() else {
        ui.set_status_text("先跑一次合成".into());
        return;
    };
    let wav = dir.join(format!("sentences/{i:03}.wav"));
    if !wav.is_file() {
        ui.set_status_text("这句还没合成：先点「开始合成」".into());
        return;
    }
    match player.play_wav(&wav) {
        Ok(()) => {
            state.playing_total.set(duration.max(0.01));
            ui.set_playing(true);
            let text = rows
                .row_data(i)
                .map(|r| r.text.to_string())
                .unwrap_or_default();
            ui.set_status_text(format!("试听第 {} 句：{text}", i + 1).into());
        }
        Err(e) => ui.set_status_text(e.into()),
    }
}

/// 全篇试听：有成品播 final.wav，否则按序播全部已合成句。
fn play_all(
    ui: &MainWindow,
    rows: &Rc<VecModel<Sentence>>,
    player: &Rc<player::Player>,
    state: &Rc<UiState>,
) {
    let assembled = state.assembled.borrow();
    if let Some(info) = assembled.as_ref() {
        if info.wav.is_file() {
            match player.play_wav(&info.wav) {
                Ok(()) => {
                    state.playing_total.set(info.duration as f32);
                    ui.set_playing(true);
                    ui.set_status_text("试听全篇成品".into());
                }
                Err(e) => ui.set_status_text(e.into()),
            }
            return;
        }
    }
    drop(assembled);
    let Some(dir) = state.project_dir.borrow().clone() else {
        ui.set_status_text("先跑一次合成".into());
        return;
    };
    let done: Vec<(usize, f32)> = (0..rows.row_count())
        .filter_map(|i| {
            rows.row_data(i)
                .filter(|r| r.status.as_str() == "已合成")
                .map(|r| (i, r.duration))
        })
        .collect();
    if done.is_empty() {
        ui.set_status_text("还没有已合成的句子".into());
        return;
    }
    let paths: Vec<PathBuf> = done
        .iter()
        .map(|(i, _)| dir.join(format!("sentences/{i:03}.wav")))
        .collect();
    match player.play_many(&paths) {
        Ok(()) => {
            let total: f32 = done.iter().map(|(_, d)| d).sum();
            state.playing_total.set(total.max(0.01));
            ui.set_playing(true);
            ui.set_status_text(format!("试听全篇（{} 句连播，未拼间隙）", done.len()).into());
        }
        Err(e) => ui.set_status_text(e.into()),
    }
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
        };
        if let Err(e) = save_settings(&next) {
            ui.set_server_ok(false);
            ui.set_server_status(format!("设置保存失败：{e}").into());
            return;
        }
        if let Ok(mut g) = settings().lock() {
            *g = next;
        }
        apply_engine_discovery(&ui, Some((&tx, &st)));
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
fn refresh_voice_labels(ui: &MainWindow) {
    let idx = ui.get_voice_index();
    let engine = (idx >= 0)
        .then(|| ui.get_voice_names().row_data(idx as usize))
        .flatten()
        .map(|n| n.to_string());
    ui.set_engine_label(engine.clone().unwrap_or_default().into());

    let ref_trimmed = ui.get_voice_ref_path().trim().to_string();

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

    // 阻断原因只留当前最重要的一条
    let hint = if engine.is_none() {
        "没有可用引擎：先在本机 audio.cpp 服务里配置 tts 模型"
    } else if !ref_trimmed.is_empty() && !exists {
        "参考音频不存在或不可读：修正路径后再开始配音"
    } else {
        ""
    };
    ui.set_voice_hint(hint.into());
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
            no: i as i32 + 1,
            text: line.as_str().into(),
            status: "待合成".into(),
            duration,
            start,
            duration_label: format!("{duration:.1}s").into(),
            start_label: clock_label(start).into(),
            eval_label: "".into(),
        });
        start += duration;
    }
    rows
}

/// 切句（与 aw-core 同算法；UI 预览用，真实切句以 aw-core 为准）
fn split_for_preview(text: &str) -> Vec<String> {
    aw_core::split_sentences(text, DEFAULT_PUNCTUATION, MAX_CHARS)
}

/// 按当前各行时长重排起始时间与总时长（合成拿到真实时长后调用）
fn recompute_total(rows: &Rc<VecModel<Sentence>>) {
    let mut start = 0.0_f32;
    for i in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(i) else {
            continue;
        };
        row.start = start;
        row.start_label = clock_label(start).into();
        start += row.duration;
        rows.set_row_data(i, row);
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
    "音色设计：参考音频克隆可用；文本生成音色未接入",
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

/// 状态标签映射：内部状态（aw-core）→ 界面文案
fn set_status(rows: &Rc<VecModel<Sentence>>, i: usize, status: &str) {
    let label = match status {
        "done" => "已合成",
        "pending" => "待合成",
        "running" => "合成中",
        "error" => "失败",
        other => other,
    };
    let Some(mut row) = rows.row_data(i) else {
        return;
    };
    if row.status.as_str() == label {
        return;
    }
    row.status = SharedString::from(label);
    rows.set_row_data(i, row);
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
    state.sep_tracks.borrow_mut().take();
    ui.set_sep_has_result(false);
    ui.set_sep_progress(0.0);
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
        let id = enqueue_task(
            &ui,
            &st,
            &st.eval_task,
            tasks::TaskKind::Eval,
            "质检 · ASR 回读",
        );
        let note: String = match queue_note(&st, id) {
            Some(q) => format!("{q} · 轮到它时自动开始质检"),
            None => "质检中：正在逐句 ASR 回读（首次会加载 ASR 模型，约 8s）…".to_string(),
        };
        ui.set_status_text(note.into());
        if tx
            .send(Cmd::RunEval {
                revision: st.project_revision.get(),
                task_id: id,
                dir,
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
        ui.set_sep_has_result(false);
        ui.set_sep_progress(0.0);
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

    /// 「清单里有多少模型落在该目录下」是**路径组件前缀**判定：
    /// `/models-2/x.gguf` 不能被算进 `/models`（字符串前缀会误算）。
    #[test]
    fn models_under_dir_uses_path_prefix_not_string_prefix() {
        let model = |id: &str, path: &str| ServerModel {
            id: id.into(),
            task: "tts".into(),
            family: "f".into(),
            path: path.into(),
        };
        let cfg = Some(ServerConfig {
            host: None,
            port: None,
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
            models: vec![],
        });

        // 只覆盖端口 → host 回落清单
        let over_port = AppSettings {
            host: None,
            port: Some(9999),
            model_dir: None,
            bgm: Default::default(),
        };
        assert_eq!(
            resolve_base(&over_port, &cfg, None).0,
            "http://manifest-host:9999"
        );

        // 只覆盖 host → 端口回落清单
        let over_host = AppSettings {
            host: Some("10.0.0.1".into()),
            port: None,
            model_dir: None,
            bgm: Default::default(),
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
    /// 做法：工程目录设成只读，连续发两次同值 Assemble——两次都必须报"保存工程失败"。
    /// 如果实现是"先改内存再 save"，第二次会因为字段已相等而跳过 save、直接去拼装，
    /// 最后拼出与 project.json 记录不一致的成品。
    #[test]
    fn assemble_gap_save_failure_does_not_mutate_memory() {
        let root = std::env::temp_dir().join(format!("aw-assemble-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut project = saved_project("第一句。第二句。", None);
        save_done_project(&root, &mut project);

        let before = std::fs::metadata(&root).unwrap().permissions();
        let mut ro = before.clone();
        ro.set_readonly(true);
        std::fs::set_permissions(&root, ro).unwrap();

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
        // 恢复权限，别给 temp 清理留坑
        std::fs::set_permissions(&root, before).unwrap();
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
        let mut restored =
            new_project_from_inputs("第一句。第二句。", "audio8-tts", None, GAP_MS, true);
        assert!(
            restored.sentences.iter().all(|s| s.status == "pending"),
            "前提：重建成的新工程没有句级状态"
        );

        let reused = reuse_done_sentences(&mut restored, &inherited, &dir).unwrap();
        assert_eq!(reused, 2, "文本相同的两句都该继承到音频");
        versions::commit_rollback(&dir, &restored).unwrap();

        // 关键断言：下一次「开始合成」不会把这句当成待录
        let loaded =
            load_resumable(&dir, "第一句。第二句。", "audio8-tts", None, GAP_MS, true).unwrap();
        assert!(
            loaded.project.sentences.iter().all(|s| s.status == "done"),
            "回滚后同文本的句子必须仍是已合成（否则会全部重录）"
        );
    }

    /// **当前工程损坏时回滚必须中止、一个字节都不写**：损坏的 project.json 是唯一可人工
    /// 恢复的现场，回滚把它当成"没有当前工程"继续写盘就把它抹了（复核抓到的数据安全阻塞）。
    #[test]
    fn rollback_refuses_when_current_project_is_corrupt() {
        let dir = temp_dir("rollback-corrupt");
        let id = versions::save(
            &dir,
            "好版本",
            &new_project_from_inputs("第一句。", "audio8-tts", None, GAP_MS, true),
            1,
        )
        .unwrap();
        let corrupt = b"{ this is not json";
        std::fs::write(dir.join("project.json"), corrupt).unwrap();

        let err = rollback_with_inheritance(&dir, &id).unwrap_err();
        assert!(err.contains("不会覆盖现场"), "{err}");
        assert_eq!(
            std::fs::read(dir.join("project.json")).unwrap(),
            corrupt,
            "损坏的现场必须原样保留"
        );

        // 对照：工程文件正常时回滚照常完成
        new_project_from_inputs("第一句。", "audio8-tts", None, GAP_MS, true)
            .save(&dir)
            .unwrap();
        assert!(rollback_with_inheritance(&dir, &id).is_ok());
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
        let on = new_project_from_inputs("2024年第一句。", "audio8-tts", None, 750, true);
        assert_eq!(on.gap_ms, 750);
        assert!(on.auto_normalize);
        assert_ne!(
            on.sentences[0].spoken, on.sentences[0].text,
            "开兜底时 spoken 应被规范化（2024年 → 二零二四年）"
        );

        let off = new_project_from_inputs("2024年第一句。", "audio8-tts", None, 750, false);
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
            let err = template_from_inputs(blank, "audio8-tts", None, 1.0, 250, true).unwrap_err();
            assert!(err.contains("名字"), "{err}");
        }
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
            GAP_MS + 250,
            true,
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
            GAP_MS,
            true,
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
            GAP_MS,
            false,
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
            GAP_MS,
            true,
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

        let loaded =
            load_resumable(&dir, "丙句。甲句。丁句。", "audio8-tts", None, GAP_MS, true).unwrap();
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
            GAP_MS,
            true,
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
            GAP_MS,
            true,
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
            GAP_MS,
            true,
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

        let loaded =
            load_resumable(&dir, "第一句。第二句。", "index-tts2", None, GAP_MS, true).unwrap();
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
            GAP_MS,
            true,
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
            eval_label: "".into(),
            no: 1,
            text: "测试句。".into(),
            status: "合成中".into(),
            duration: 1.0,
            start: 0.0,
            duration_label: "1.0s".into(),
            start_label: "0:00".into(),
        }]));
        mark_running_rows_failed(&rows);
        assert_eq!(rows.row_data(0).unwrap().status, "失败");
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

    /// 工程损坏时开始合成必须**中止**而不是静默重建：以前 `Project::load(dir).ok()` 会把它
    /// 当成"没有工程"，已合成句全变待合成且没有一句解释；更糟的是随后落盘会覆盖掉损坏
    /// 文件——那是唯一可人工恢复的现场。这里同时守住"缺文件仍是全新工程"这条回归。
    #[test]
    fn corrupt_project_aborts_the_run_and_keeps_the_file() {
        let dir = temp_dir("corrupt-project");
        let broken = br#"{"sentences": [{"index": 1,"#;
        std::fs::write(dir.join("project.json"), broken).unwrap();

        let err =
            load_resumable(&dir, "第一句。第二句。", "audio8-tts", None, GAP_MS, true).unwrap_err();
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
        let loaded =
            load_resumable(&fresh, "第一句。第二句。", "audio8-tts", None, GAP_MS, true).unwrap();
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
    #[test]
    fn cancelled_queued_batch_items_are_skipped_without_work() {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (msg_tx, msg_rx) = channel::<WorkerMsg>();
        let cancel = cancel::CancelRegistry::new();
        cancel.cancel(41);
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
                gap_ms: GAP_MS,
                auto_normalize: true,
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
        cancel.cancel(42);

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
                gap_ms: GAP_MS,
                auto_normalize: true,
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
            Msg::BatchScriptsPicked { paths: Vec::new() },
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
        ];
        let names = [
            "BatchScriptsPicked",
            "BatchItemStarted",
            "BatchItemProgress",
            "BatchItemDone",
            "BatchDone",
            "BatchExportDone",
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
            percent: 0.0,
            scored: 0,
            asr_failed: 5,
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
            percent: 100.0,
            scored: 3,
            asr_failed: 0,
            worst: Vec::new(),
            scores: vec![(0, 100.0), (1, 100.0), (2, 100.0)],
            persist_warning: None,
            report_path: None,
        };
        let note = eval_summary_note(&clean);
        assert!(note.contains("100.0%") && note.contains("3 句"), "{note}");
        assert!(note.contains("全部一致"), "{note}");

        let with_worst = EvalSummary {
            percent: 96.4,
            scored: 57,
            asr_failed: 2,
            scores: vec![(0, 100.0), (11, 92.3)],
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
    }

    /// 句子行上的质检标签：低于阈值加 ⚠（阈值是启发式，不是质量门槛）。
    #[test]
    fn eval_label_marks_low_scores() {
        assert_eq!(eval_label(99.9), "可懂度 99.9%");
        assert_eq!(eval_label(95.0), "可懂度 95.0%", "正好等于阈值不加警告");
        assert_eq!(eval_label(94.9), "⚠ 可懂度 94.9%");
        assert_eq!(eval_label(0.0), "⚠ 可懂度 0.0%");
    }

    /// 分数写不进工程时要如实说（分数有效但没落盘），别让用户以为下次打开还在。
    #[test]
    fn eval_summary_note_reports_persist_warning() {
        let summary = EvalSummary {
            percent: 96.4,
            scored: 57,
            asr_failed: 0,
            worst: vec![EvalIssue {
                index: 11,
                percent: 92.3,
                snippet: "…【应为 例，读到 力】…".into(),
            }],
            scores: vec![(11, 92.3)],
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

    /// 从工程取质检分数：只接受「已合成」的句子（失败句即使文件里留着旧分也不贴）。
    #[test]
    fn scores_from_project_keeps_only_done_sentences() {
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
        prj.sentences[1].status = "error: 模型没加载".into();
        prj.sentences[1].eval_percent = Some(50.0); // 脏数据：失败句不该贴出来
        prj.sentences[2].status = "done".into();
        prj.sentences[2].eval_percent = None;

        let scores = scores_from_project(&prj);
        assert_eq!(scores.get(&0), Some(&99.0));
        assert_eq!(scores.get(&1), None, "失败句的旧分数不能贴");
        assert_eq!(scores.get(&2), None, "没测过的句子没有分数");
        assert_eq!(scores.len(), 1);
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
        let md = qa_report_markdown("示例工程", "qwen3-asr", &rows, 96.4, 2, 0);
        assert!(md.contains("# 质检报告 · 示例工程"), "{md}");
        assert!(md.contains("qwen3-asr"), "要写明回读模型：{md}");
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
        let md = qa_report_markdown("示例工程", "qwen3-asr", &[], 0.0, 0, 5);
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
    #[ignore = "需要本机 audiocpp_server + audio8-tts + qwen3-asr"]
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
            .synthesize(&client, &dir, None, None, |_, _| {})
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
            .send(Cmd::RunEval {
                revision: 0,
                task_id: 7,
                dir: dir.clone(),
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
        eprintln!("报告：{}\n{md}", path.display());

        // 工程里的分数也落了盘（跨会话留存那条）
        let on_disk = Project::load(&dir).unwrap();
        assert!(
            on_disk.sentences.iter().all(|s| s.eval_percent.is_some()),
            "每句都应写入 eval_percent"
        );
    }

    /// 一句都没评上分时**也要**把"落盘告警/报告路径"带出来——旧写法在 scored==0 分支提前 return，
    /// 把报告路径和"报告没写进去"的告警一起吞了（复核指出）。
    #[test]
    fn eval_summary_note_keeps_warnings_when_nothing_scored() {
        let summary = EvalSummary {
            percent: 0.0,
            scored: 0,
            asr_failed: 5,
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
                project_name: "worker-dub-e2e".into(),
                gap_ms: GAP_MS,
                auto_normalize: true,
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
                } => break (failed, stopped, reused),
                Msg::Fatal(e) => panic!("合成中止：{e}"),
                _ => {}
            }
        };
        assert_eq!(loaded_reused, Some(0), "全新工程不该复用旧句");
        assert_eq!(run_done, (0, false, 0), "实得 {run_done:?}");
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

    /// 多选文件框的输出解析：取消（退出码非 0）必须是空表，空行要滤掉——
    /// 否则一次"取消"会被批量导入记成"跳过了 1 篇（读不到）"。
    #[test]
    fn picked_paths_drop_cancellations_and_blank_lines() {
        let picked = "/a/第一集.txt\n\n/b/第二集.txt\n".as_bytes();
        assert_eq!(
            parse_picked_paths(true, picked),
            vec![
                PathBuf::from("/a/第一集.txt"),
                PathBuf::from("/b/第二集.txt")
            ]
        );
        assert!(
            parse_picked_paths(true, b"").is_empty(),
            "没选到文件就是空表"
        );
        assert!(
            parse_picked_paths(false, "/a/不该出现.txt\n".as_bytes()).is_empty(),
            "用户取消时不能把任何路径当成选中"
        );
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
                    gap_ms: GAP_MS,
                    auto_normalize: true,
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
