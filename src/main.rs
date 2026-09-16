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

mod player;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, Timer, TimerMode, VecModel};

use aw_core::{
    assemble_bgm, generate_segments, generate_song, mix_project, BgmArtifacts, BgmOptions, Client,
    Project, SongModel, SongOptions, DEFAULT_PUNCTUATION,
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
const DEFAULT_PROJECT: &str = "示例工程 · 频道口播";
/// 中文口播时长估算：秒/字（未合成句的展示用估值；合成后由真实时长覆盖）。
const SECS_PER_CHAR: f32 = 0.18;

// ── 工作线程消息 ──

enum Cmd {
    /// 开始/继续合成。script/model/voice_ref/project_name 取自界面当前值。
    Run {
        revision: u64,
        script: String,
        model: String,
        voice_ref: Option<String>,
        project_name: String,
    },
    /// 单句重录（换 seed 重合成该句）
    Redo { revision: u64, index: usize },
    /// 拼装成品 + SRT
    Assemble { revision: u64 },
    /// 启动时把已恢复工程交给 worker，保证重开后 Redo/Assemble 仍作用于同一工程。
    OpenProject {
        revision: u64,
        dir: PathBuf,
        project: Project,
    },
    /// UI 的稿件/工程名/模型/参考音已变；旧 current 立即作废，直到下一轮 Run。
    InvalidateProject,
    /// 基于当前配音工程生成并混合 BGM。
    RunBgm { revision: u64, prompt: String },
    /// 音色试听：用指定音色合成一句固定短句，只播不落工程、不改 current。
    PreviewVoice {
        revision: u64,
        model: String,
        voice_ref: Option<String>,
        text: String,
    },
    /// 歌曲彩蛋生成（独立于配音工程内容，只复用工程目录）。
    RunSong {
        revision: u64,
        project_name: String,
        model: String,
        lyrics: String,
        style: String,
    },
}

enum Msg {
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
    },
    BgmFailed(String),
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
    /// 音色试听失败（保留音色名，便于在状态栏说清是哪个音色挂了）
    VoicePreviewFailed {
        label: String,
        error: String,
    },
    SongDone {
        path: PathBuf,
        duration: f64,
    },
    SongFailed(String),
    /// 工作线程无法继续的错误
    Fatal(String),
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
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Documents")
        .join(WORKSHOP_DIR)
        .join("settings.json")
}

fn load_settings() -> AppSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
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
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/opt/audio.cpp/server.json")
}

/// 模型清单路径：AW_SERVER_CONFIG > 全局设置 > 默认路径
fn config_path() -> PathBuf {
    // 清单路径只在环境变量里可覆盖：UI 上不再暴露"模型清单文件"
    // （用户口径：全局设置里是**模型目录**，不是清单文件）。
    std::env::var("AW_SERVER_CONFIG")
        .map(PathBuf::from)
        .ok()
        .unwrap_or_else(default_config_path)
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

    if !out.status.success() {
        return None; // 取消时退出码非 0
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

// ===========================================================================
// 工作线程：持有 Project，顺序处理命令（合成是串行关键路径，无需并行）
// ===========================================================================

struct WorkerCtx {
    rx: Receiver<Cmd>,
    tx: Sender<WorkerMsg>,
    stop: Arc<AtomicBool>,
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
                script,
                model,
                voice_ref,
                project_name,
            } => {
                let dir = project_dir(&file_stem(&project_name));
                let loaded = match load_resumable(&dir, &script, &model, voice_ref) {
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
            Cmd::RunBgm { revision, prompt } => {
                let Some((current_revision, dir, _)) = current.as_ref() else {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::BgmFailed("先完成配音并载入工程，再生成 BGM".into()),
                    });
                    continue;
                };
                if *current_revision != revision {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::BgmFailed("工程版本已变更：先重新载入配音工程".into()),
                    });
                    continue;
                }
                let voice_path = dir.join("out/final.wav");
                let target_seconds = std::fs::read(&voice_path)
                    .ok()
                    .and_then(|bytes| aw_core::dub::wav_duration(&bytes).ok())
                    .ok_or_else(|| "还没有配音成品：先合成并拼装".to_string())
                    .and_then(|duration| {
                        if duration > 0.0 {
                            Ok(duration)
                        } else {
                            Err("配音成品时长为 0".to_string())
                        }
                    });
                let target_seconds = match target_seconds {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmFailed(e),
                        });
                        continue;
                    }
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
                    ..Default::default()
                };
                let tx = ctx.tx.clone();
                let segments =
                    match generate_segments(&client, dir, &options, |done, total, note| {
                        let _ = tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmProgress { done, total },
                        });
                        let _ = note;
                    }) {
                        Ok(n) => n,
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
                match mix_project(dir, &options) {
                    Ok(artifacts) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmDone {
                                artifacts,
                                segments,
                            },
                        });
                    }
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::BgmFailed(format!("BGM 混音失败: {e}")),
                        });
                    }
                }
            }
            Cmd::RunSong {
                revision,
                project_name,
                model,
                lyrics,
                style,
            } => {
                let dir = project_dir(&file_stem(&project_name)).join("song");
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    let _ = ctx.tx.send(WorkerMsg {
                        revision,
                        msg: Msg::SongFailed(e.to_string()),
                    });
                    continue;
                }
                let client = match make_client() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::SongFailed(e),
                        });
                        continue;
                    }
                };
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
                match generate_song(&client, &dir, "song", &options) {
                    Ok(path) => {
                        let duration = std::fs::read(&path)
                            .ok()
                            .and_then(|bytes| aw_core::dub::wav_duration(&bytes).ok())
                            .unwrap_or(0.0);
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::SongDone { path, duration },
                        });
                    }
                    Err(e) => {
                        let _ = ctx.tx.send(WorkerMsg {
                            revision,
                            msg: Msg::SongFailed(format!("歌曲生成失败: {e}")),
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
            Cmd::Assemble { revision } => {
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
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Documents")
        .join(WORKSHOP_DIR)
        .join("projects")
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
        std::fs::copy(&src, &temp.0).map_err(|e| {
            format!(
                "复用第 {} 句失败（{}）: {e}",
                old_sentence.index,
                src.display()
            )
        })?;
        std::fs::File::open(&temp.0)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("复用第 {} 句落盘失败: {e}", old_sentence.index))?;
        staged.push(StagedReuse {
            new_index: i,
            temp,
            dst,
            old_sentence,
        });
    }

    let mut reused = 0;
    for staged in staged {
        std::fs::rename(&staged.temp.0, &staged.dst)
            .map_err(|e| format!("复用句落到 {} 失败: {e}", staged.dst.display()))?;
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
fn load_resumable(
    dir: &Path,
    script: &str,
    model: &str,
    voice_ref: Option<String>,
) -> Result<LoadedProject, String> {
    let voice_ref_hash = match voice_ref.as_deref() {
        Some(path) => Some(sha256_file(Path::new(path))?),
        None => None,
    };
    let saved = Project::load(dir).ok();
    if let Some(saved) = saved.as_ref() {
        if saved.model == model
            && voice_ref_matches(saved, &voice_ref, &voice_ref_hash)
            && sentence_texts_match(saved, script)
        {
            return Ok(LoadedProject {
                project: saved.clone(),
                reused: 0,
            });
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("建工程目录失败: {e}"))?;
    let mut project = Project::new(
        script,
        model,
        GAP_MS,
        BASE_SEED,
        voice_ref.clone(),
        DEFAULT_PUNCTUATION,
        MAX_CHARS,
        |t| aw_core::normalize(t, &Default::default()),
    );
    project.voice_ref_hash = voice_ref_hash.clone();
    let reused = if let Some(saved) = saved.as_ref() {
        if saved.model == model && voice_ref_matches(saved, &voice_ref, &voice_ref_hash) {
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
    let stop = Arc::new(AtomicBool::new(false));
    let state = Rc::new(UiState {
        assembled: RefCell::new(None),
        project_dir: RefCell::new(None),
        playing_total: std::cell::Cell::new(1.0),
        project_ready: std::cell::Cell::new(false),
        project_revision: std::cell::Cell::new(0),
        bgm_artifacts: RefCell::new(None),
        song_artifact: RefCell::new(None),
    });
    {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            worker_loop(WorkerCtx {
                rx: cmd_rx,
                tx: msg_tx,
                stop,
            })
        });
    }

    // 启动时恢复默认工程的进度（断点续作可见）
    restore_project(&ui, &rows, &state, &cmd_tx);

    // ── 窗口控制 ──
    slint_pixel::install_title_bar_controls(&ui);
    slint_pixel::install_window_resize(&ui);

    wire_theme(&ui);
    wire_script(&ui, &rows, &cmd_tx, &state);
    wire_engine_changes(&ui, &cmd_tx, &state);
    wire_voice_panel(&ui, &cmd_tx, &state);
    wire_global_settings(&ui, &msg_tx_ui, &cmd_tx, &state);
    wire_sentence_actions(&ui, &rows, &cmd_tx, &player, &state);
    wire_run(&ui, &rows, &cmd_tx, &player, &stop, &state);
    wire_export(&ui, &cmd_tx, &state);
    wire_bgm(&ui, &cmd_tx, &state, &player);
    wire_song(&ui, &cmd_tx, &state, &player);
    wire_keys(&ui, &rows, &player, &state);

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
    let Ok(project) = Project::load(&dir) else {
        return;
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
    let done = apply_project_to_rows(ui, rows, &project);
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
        ui.set_status_text(note.into());
    });
}

fn invalidate_worker_project(cmd_tx: &Sender<Cmd>, state: &Rc<UiState>) {
    state.project_ready.set(false);
    state
        .project_revision
        .set(state.project_revision.get().wrapping_add(1));
    state.assembled.borrow_mut().take();
    let _ = cmd_tx.send(Cmd::InvalidateProject);
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
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：工程名暂不可改".into());
            return;
        }
        invalidate_worker_project(&tx, &state0);
        reset_bgm(&ui, &state0);
        ui.set_has_result(false);
        ui.set_status_text(format!("工程名：{}", ui.get_project_name()).into());
    });

    let weak = ui.as_weak();
    let rows1 = rows.clone();
    let tx1 = cmd_tx.clone();
    let state1 = state.clone();
    ui.on_script_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：等这轮跑完或先停止，再编辑稿件".into());
            return;
        }
        let text = ui.get_script_text();
        rebuild(&ui, &rows1, &text);
        invalidate_worker_project(&tx1, &state1);
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
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：暂不能载入示例稿".into());
            return;
        }
        ui.set_script_text(SAMPLE_SCRIPT.into());
        rebuild(&ui, &rows2, SAMPLE_SCRIPT);
        invalidate_worker_project(&tx2, &state2);
        reset_bgm(&ui, &state2);
        ui.set_status_text(format!("已载入示例稿：{} 句", rows2.row_count()).into());
    });

    let weak = ui.as_weak();
    let rows3 = rows.clone();
    let tx3 = cmd_tx.clone();
    let state3 = state.clone();
    ui.on_clear_script(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：暂不能清空稿件".into());
            return;
        }
        ui.set_script_text("".into());
        rebuild(&ui, &rows3, "");
        invalidate_worker_project(&tx3, &state3);
        reset_bgm(&ui, &state3);
        ui.set_status_text("稿件已清空，粘一段口播稿试试".into());
    });

    let weak = ui.as_weak();
    let rows4 = rows.clone();
    let tx4 = cmd_tx.clone();
    let state4 = state.clone();
    ui.on_resplit(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：暂不能重新切句".into());
            return;
        }
        let text = ui.get_script_text();
        rebuild(&ui, &rows4, &text);
        invalidate_worker_project(&tx4, &state4);
        reset_bgm(&ui, &state4);
        let n = rows4.row_count();
        ui.set_status_text(format!("已重新切句：{n} 句").into());
        toast(&ui, &format!("已重新切句：{n} 句"));
    });
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
        if ui.get_running() || ui.get_busy() {
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
        ui.set_status_text(format!("正在合成试听（{what} · {model}）…").into());
        let _ = tx.send(Cmd::PreviewVoice {
            revision: st.project_revision.get(),
            model,
            voice_ref,
            text: VOICE_PREVIEW_TEXT.to_string(),
        });
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
        if ui.get_running() {
            ui.set_status_text("合成进行中：等这轮跑完再重录单句".into());
            return;
        }
        if ui.get_busy() {
            ui.set_status_text("重录 / 导出正在进行：请等当前任务结束".into());
            return;
        }
        if !state3.project_ready.get() {
            ui.set_status_text("工程已变更：先开始合成，再重录单句".into());
            return;
        }
        ui.set_selected(i);
        ui.set_busy(true);
        if tx3
            .send(Cmd::Redo {
                revision: state3.project_revision.get(),
                index: idx,
            })
            .is_err()
        {
            ui.set_busy(false);
            ui.set_status_text("工作线程不可用：重录未发出，请重启应用".into());
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
        if ui.get_busy() {
            ui.set_status_text("重录 / 导出正在进行：请等当前任务结束".into());
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
        ui.set_running(true);
        ui.set_has_result(false);
        ui.set_progress(ui.get_done_count() as f32 / n.max(1) as f32);
        ui.set_playing(false);
        player1.stop();
        state1.assembled.borrow_mut().take();
        state1.project_ready.set(false);
        let stem = file_stem(&ui.get_project_name());
        *state1.project_dir.borrow_mut() = Some(project_dir(&stem));
        if tx
            .send(Cmd::Run {
                revision: state1.project_revision.get(),
                script: ui.get_script_text().to_string(),
                model: model_name.clone(),
                voice_ref,
                project_name: stem,
            })
            .is_err()
        {
            ui.set_running(false);
            state1.project_ready.set(false);
            ui.set_status_text("工作线程不可用：合成未发出，请重启应用".into());
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
        if !ui.get_running() {
            return;
        }
        stop2.store(true, Ordering::Relaxed);
        ui.set_status_text("正在停止：当前句合成完就停".into());
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

fn wire_export(ui: &MainWindow, cmd_tx: &Sender<Cmd>, state: &Rc<UiState>) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state = state.clone();
    ui.on_export_requested(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_busy() {
            ui.set_status_text("已有导出 / 重录任务正在进行".into());
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
            })
            .is_err()
        {
            ui.set_busy(false);
            ui.set_status_text("工作线程不可用：导出未发出，请重启应用".into());
            return;
        }
        ui.set_status_text("拼装成品中（完成后按导出开关复制）…".into());
    });
}

fn export_bgm_tracks(ui: &MainWindow, state: &Rc<UiState>) {
    let Some(artifacts) = state.bgm_artifacts.borrow().clone() else {
        ui.set_status_text("还没有 BGM 成品：先生成并混音".into());
        return;
    };
    let dir = PathBuf::from(ui.get_export_dir().to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        ui.set_status_text(format!("导出目录不可写（{}）: {e}", dir.display()).into());
        return;
    }
    let stem = stem_of(ui);
    let jobs = [
        (artifacts.voice, format!("{stem}_voice.wav")),
        (artifacts.bgm, format!("{stem}_bgm.wav")),
        (artifacts.mixed, format!("{stem}_mixed.wav")),
        (artifacts.srt, format!("{stem}.srt")),
    ];
    let mut last = PathBuf::new();
    for (src, name) in jobs {
        let dst = dir.join(name);
        if let Err(e) = std::fs::copy(&src, &dst) {
            ui.set_status_text(format!("导出 {} 失败: {e}", dst.display()).into());
            return;
        }
        last = dst;
    }
    toast(ui, &format!("已导出三轨：{}", last.display()));
    ui.set_status_text(format!("已导出 voice / bgm / mixed / srt 到 {}", dir.display()).into());
}

fn wire_bgm(
    ui: &MainWindow,
    cmd_tx: &Sender<Cmd>,
    state: &Rc<UiState>,
    player: &Rc<player::Player>,
) {
    let weak = ui.as_weak();
    let tx = cmd_tx.clone();
    let state1 = state.clone();
    ui.on_bgm_generate(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：等当前任务结束再生成 BGM".into());
            return;
        }
        if !state1.project_ready.get() {
            ui.set_status_text("先完成配音并载入当前工程，再生成 BGM".into());
            return;
        }
        let prompt = ui.get_bgm_prompt().to_string();
        if prompt.trim().is_empty() {
            ui.set_status_text("先写一段 BGM 描述".into());
            return;
        }
        reset_bgm(&ui, &state1);
        ui.set_busy(true);
        ui.set_bgm_status_text("正在生成 BGM 分段…".into());
        if tx
            .send(Cmd::RunBgm {
                revision: state1.project_revision.get(),
                prompt,
            })
            .is_err()
        {
            ui.set_busy(false);
            let note = "工作线程不可用：BGM 未发出，请重启应用";
            ui.set_bgm_status_text(note.into());
            ui.set_status_text(note.into());
        }
    });

    let weak = ui.as_weak();
    let state2 = state.clone();
    let player2 = player.clone();
    ui.on_bgm_preview(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some((path, duration)) = state2
            .bgm_artifacts
            .borrow()
            .as_ref()
            .map(|a| (a.mixed.clone(), a.duration))
        else {
            ui.set_status_text("还没有 BGM 成品：先生成并混音".into());
            return;
        };
        match player2.play_wav(&path) {
            Ok(()) => {
                state2.playing_total.set(duration as f32);
                ui.set_playing(true);
                ui.set_status_text("试听 BGM 混音".into());
            }
            Err(e) => ui.set_status_text(e.into()),
        }
    });

    let weak = ui.as_weak();
    let state3 = state.clone();
    ui.on_bgm_export_tracks(move || {
        let Some(ui) = weak.upgrade() else { return };
        export_bgm_tracks(&ui, &state3);
    });
}

fn export_song(ui: &MainWindow, state: &Rc<UiState>) {
    let Some((source, _)) = state.song_artifact.borrow().clone() else {
        ui.set_status_text("还没有歌曲成品：先生成歌曲".into());
        return;
    };
    let dir = PathBuf::from(ui.get_export_dir().to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        ui.set_status_text(format!("导出目录不可写（{}）: {e}", dir.display()).into());
        return;
    }
    let dst = dir.join(format!("{}_song.wav", stem_of(ui)));
    if let Err(e) = std::fs::copy(&source, &dst) {
        ui.set_status_text(format!("导出 {} 失败: {e}", dst.display()).into());
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
        if ui.get_running() || ui.get_busy() {
            ui.set_status_text("任务进行中：等当前任务结束再生成歌曲".into());
            return;
        }
        let lyrics = ui.get_song_lyrics().to_string();
        let style = ui.get_song_style().to_string();
        if lyrics.trim().is_empty() || style.trim().is_empty() {
            ui.set_status_text("先填歌词和歌曲风格".into());
            return;
        }
        ui.set_busy(true);
        ui.set_song_has_result(false);
        ui.set_song_status_text(
            "歌曲生成中（yue2 可能约 8 分钟，ACE-Step 120s 约 6.4 分钟）…".into(),
        );
        let model = if ui.get_song_model_index() == 1 {
            "ace-step"
        } else {
            "yue2"
        };
        if tx
            .send(Cmd::RunSong {
                revision: state1.project_revision.get(),
                project_name: file_stem(&ui.get_project_name()),
                model: model.into(),
                lyrics,
                style,
            })
            .is_err()
        {
            ui.set_busy(false);
            let note = "工作线程不可用：歌曲未发出，请重启应用";
            ui.set_song_status_text(note.into());
            ui.set_status_text(note.into());
        }
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
        let revision_agnostic = matches!(
            worker_msg.msg,
            Msg::ServerHealth { .. } | Msg::ModelDirPicked { .. }
        );
        if !revision_agnostic
            && !worker_message_is_current(&worker_msg, state.project_revision.get())
        {
            continue;
        }
        match worker_msg.msg {
            Msg::ProjectLoaded { project, reused } => {
                apply_project_to_rows(ui, rows, &project);
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
                ui.set_status_text(t.into());
            }
            Msg::Sentence {
                index,
                status,
                duration,
            } => {
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
                    ui.set_progress(done as f32 / rows.row_count().max(1) as f32);
                }
            }
            Msg::RunDone {
                failed,
                stopped,
                reused,
            } => {
                ui.set_busy(false);
                run_finished = Some((failed, stopped, reused));
            }
            Msg::RedoDone { index, error } => {
                ui.set_busy(false);
                if let Some(error) = error {
                    set_status(rows, index, "error");
                    ui.set_status_text(error.into());
                } else {
                    ui.set_status_text(format!("第 {} 句重录完成", index + 1).into());
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
            }
            Msg::VoicePreview { wav, label } => {
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
                        ui.set_status_text(format!("试听临时文件写入失败：{e}").into());
                    }
                }
            }
            Msg::VoicePreviewFailed { label, error } => {
                ui.set_status_text(format!("试听失败（{label}）：{error}").into());
            }
            Msg::BgmProgress { done, total } => {
                let progress = done as f32 / total.max(1) as f32;
                ui.set_bgm_progress(progress);
                let note = format!("BGM 生成中：{done}/{total} 段");
                ui.set_bgm_status_text(note.clone().into());
                ui.set_status_text(note.into());
            }
            Msg::BgmDone {
                artifacts,
                segments,
            } => {
                ui.set_busy(false);
                ui.set_bgm_has_result(true);
                ui.set_bgm_progress(1.0);
                let note = format!(
                    "BGM 完成：{segments} 段 · 成品 {:.1}s · 已生成 voice/bgm/mixed",
                    artifacts.duration
                );
                ui.set_bgm_status_text(note.clone().into());
                ui.set_status_text(note.into());
                *state.bgm_artifacts.borrow_mut() = Some(artifacts);
            }
            Msg::BgmFailed(error) => {
                ui.set_busy(false);
                ui.set_bgm_has_result(false);
                ui.set_bgm_progress(0.0);
                ui.set_bgm_status_text(error.clone().into());
                ui.set_status_text(error.into());
            }
            Msg::SongDone { path, duration } => {
                ui.set_busy(false);
                ui.set_song_has_result(true);
                let note = format!("歌曲完成：{duration:.1}s · {}", path.display());
                ui.set_song_status_text(note.clone().into());
                ui.set_status_text(note.into());
                *state.song_artifact.borrow_mut() = Some((path, duration));
            }
            Msg::SongFailed(error) => {
                ui.set_busy(false);
                ui.set_song_has_result(false);
                ui.set_song_status_text(error.clone().into());
                ui.set_status_text(error.into());
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
                let export_outcome = export_copies(
                    &stem_of(ui),
                    &PathBuf::from(ui.get_export_dir().to_string()),
                    &wav,
                    &srt,
                    ui.get_export_wav_on(),
                    ui.get_export_srt_on(),
                );
                let export_note = match export_outcome {
                    ExportOutcome::Exported(path) => {
                        toast(ui, &format!("已导出 {}", path.display()));
                        format!(" · 已导出 {}", path.display())
                    }
                    ExportOutcome::NoneSelected => " · 未选择导出格式，仅更新成品".into(),
                    ExportOutcome::Failed(e) => format!(" · 导出失败: {e}"),
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

    // ── 试听结束：rodio 队列播空 → 复位 playing ──
    if ui.get_playing() && !player.is_playing() {
        ui.set_playing(false);
        ui.set_status_text("试听结束".into());
    }
}

enum ExportOutcome {
    Exported(PathBuf),
    NoneSelected,
    Failed(String),
}

/// 按导出开关把成品复制到导出目录。未勾选格式也给出明确结果，不再静默返回。
fn export_copies(
    stem: &str,
    dir: &Path,
    wav: &Path,
    srt: &Path,
    wav_on: bool,
    srt_on: bool,
) -> ExportOutcome {
    if !wav_on && !srt_on {
        return ExportOutcome::NoneSelected;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return ExportOutcome::Failed(format!("导出目录不可写（{}）: {e}", dir.display()));
    }
    let mut written: Vec<String> = Vec::new();
    if wav_on {
        let t = dir.join(format!("{stem}.wav"));
        if let Err(e) = std::fs::copy(wav, &t) {
            return ExportOutcome::Failed(format!("复制 {} 失败: {e}", t.display()));
        }
        written.push(t.display().to_string());
    }
    if srt_on {
        let t = dir.join(format!("{stem}.srt"));
        if let Err(e) = std::fs::copy(srt, &t) {
            return ExportOutcome::Failed(format!("复制 {} 失败: {e}", t.display()));
        }
        written.push(t.display().to_string());
    }
    let last = written
        .last()
        .cloned()
        .unwrap_or_else(|| dir.display().to_string());
    ExportOutcome::Exported(PathBuf::from(last))
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
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/Documents/{WORKSHOP_DIR}")
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

    /// 评审 MUST-1：稿件改一句后，未变句必须按文本复用，而不是整工程重录。
    #[test]
    fn edited_script_reuses_unchanged_done_sentences() {
        let dir = temp_dir("resume-edited-script");
        let mut old = saved_project("第一句。第二句。第三句。", None);
        save_done_project(&dir, &mut old);

        let loaded =
            load_resumable(&dir, "第一句。改过的第二句。第三句。", "audio8-tts", None).unwrap();
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

        let loaded = load_resumable(&dir, "丙句。甲句。丁句。", "audio8-tts", None).unwrap();
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

        let loaded = load_resumable(&dir, "重复句。重复句。不同句。", "audio8-tts", None).unwrap();
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

        let err =
            load_resumable(&dir, "第一句。第二句。", "audio8-tts", Some(missing_path)).unwrap_err();
        assert!(err.contains("参考音频不可读"), "应明确报错: {err}");
        let on_disk = Project::load(&dir).unwrap();
        assert_eq!(on_disk.sentences[0].status, "done", "旧工程不得被覆盖");
    }

    #[test]
    fn invalidation_bumps_revision_and_clears_ready() {
        let state = Rc::new(UiState {
            assembled: RefCell::new(None),
            project_dir: RefCell::new(None),
            playing_total: std::cell::Cell::new(1.0),
            project_ready: std::cell::Cell::new(true),
            project_revision: std::cell::Cell::new(7),
            bgm_artifacts: RefCell::new(None),
            song_artifact: RefCell::new(None),
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

        let loaded = load_resumable(&dir, "第一句。第二句。", "index-tts2", None).unwrap();
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
        let loaded =
            load_resumable(&dir, "第一句。第二句。", "audio8-tts", Some(voice_path)).unwrap();
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

    #[test]
    fn export_copies_reports_selection_and_writes_expected_files() {
        let dir = temp_dir("export-copies");
        let src_dir = dir.join("src");
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&src_dir).unwrap();
        let wav = src_dir.join("source.wav");
        let srt = src_dir.join("source.srt");
        std::fs::write(&wav, b"wav-data").unwrap();
        std::fs::write(&srt, b"srt-data").unwrap();

        match export_copies("我的工程", &out_dir, &wav, &srt, true, true) {
            ExportOutcome::Exported(path) => {
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
            export_copies("我的工程", &no_export_dir, &wav, &srt, false, false),
            ExportOutcome::NoneSelected
        ));
        assert!(!no_export_dir.exists());
    }

    #[test]
    fn fatal_marks_running_rows_failed() {
        let rows: Rc<VecModel<Sentence>> = Rc::new(VecModel::from(vec![Sentence {
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
}
