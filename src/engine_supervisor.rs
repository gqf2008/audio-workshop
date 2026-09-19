//! 随包推理引擎（audio.cpp 的 `audiocpp_server`）的发现、拉起与回收。
//!
//! 目标：用户下载安装包后直接用，不需要自己跑服务。但**外部服务优先** —— 现状大量
//! 用户自己跑着服务、甚至指向别的机器，壳不能把他们的用法弄坏。
//!
//! 优先级（与 `resolve_base` 的三档一致，别在这里造第二套判据）：
//!   1. 用户显式配置了地址（`AW_SERVER` / 全局设置里的 host:port）→ **不拉起内置引擎**，
//!      只当客户端；地址不是回环更是如此（那是别人的服务）。
//!   2. 该地址已经有服务在响应 `/health` → 复用，不拉起。
//!   3. 否则拉起随包引擎（仅当它是回环地址、且引擎文件确实存在）。
//!
//! 两条硬约束（见 `LESSON` 与设计报告）：
//!   · **只 kill 自己 spawn 的 PID**。"按名字杀 audiocpp_server" 会误杀用户自己那份。
//!   · 引擎的 `models` 数组为空时**拒绝启动**（`config.cpp:275` 实测），所以配置文件必须
//!     带上真实模型条目；一个都没有时如实返回 NotConfigured，不假装能跑。
//!
//! 写盘位置：`server.json` / 日志都落**用户数据目录**，绝不落安装目录
//! （见 `LESSON_运行时可写数据严禁落安装目录或应用包内.md`）。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 拉起结果：调用方据此决定要不要提示用户。
#[derive(Debug, PartialEq, Eq)]
pub enum StartOutcome {
    /// 配置的地址上已经有服务，直接复用（可能是用户自己跑的，也可能是上次留下的）。
    Reused,
    /// 内置引擎已拉起并通过 `/health`。
    Started,
    /// 引擎文件不在（开发构建、或用户装了不带引擎的包）。
    EngineMissing,
    /// 模型目录里一个能识别的模型都没有 —— 引擎拒绝启动，如实上报。
    NotConfigured,
    /// 拉起了但没起来（崩溃 / 端口占用 / 启动超时），附原因。
    Failed(String),
}

/// 引擎发现：`AW_ENGINE_DIR` > `.app/Contents/Resources/engine` > 可执行文件同级 `engine/`。
///
/// 三级是为了三平台共用一个函数：macOS 在 bundle Resources 里，Windows/Linux 的
/// 安装布局把 `engine/` 放在 exe 同级。`AW_ENGINE_DIR` 留给开发与排障。
pub fn engine_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AW_ENGINE_DIR") {
        let p = PathBuf::from(dir);
        return p.is_dir().then_some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    // macOS bundle：<App>.app/Contents/MacOS/<exe> → ../Resources/engine
    let bundle_engine = exe_dir.parent()?.join("Resources/engine");
    if bundle_engine.is_dir() {
        return Some(bundle_engine);
    }
    let sibling = exe_dir.join("engine");
    sibling.is_dir().then_some(sibling)
}

/// 引擎可执行文件名（Windows 带 .exe）。
pub fn engine_binary_name() -> &'static str {
    if cfg!(windows) {
        "audiocpp_server.exe"
    } else {
        "audiocpp_server"
    }
}

pub fn engine_binary() -> Option<PathBuf> {
    let p = engine_dir()?.join(engine_binary_name());
    p.is_file().then_some(p)
}

/// 引擎要跑的 host/port：从壳自己的地址解析结果里取。
///
/// 只接受回环 —— 非回环是"用户在连别的机器"，那种情况根本不进这个模块。
pub fn is_loopback_host(host: &str) -> bool {
    let h = host
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let h = h.split('/').next().unwrap_or(h);
    let h = h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h);
    matches!(h, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// 从 `http://host:port` 里拆出 port。解析不出来时返回 None（调用方会回落默认端口）。
pub fn port_of(base: &str) -> Option<u16> {
    let rest = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    let host_port = rest.split('/').next().unwrap_or(rest);
    let (_, port) = host_port.rsplit_once(':')?;
    port.parse().ok()
}

/// 引擎默认后端：macOS 用 Metal，其它平台先 CPU（CUDA/Vulkan 做成按需加速包，P2）。
pub fn default_backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "metal"
    } else {
        "cpu"
    }
}

/// 一个要在 `server.json` 里声明的模型条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedModel {
    pub id: String,
    pub family: String,
    pub path: String,
    pub task: String,
}

/// 把随包清单的 `spec`（如 `audio8_tts.json`）翻成引擎认的 family（`audio8_tts`）。
///
/// 依据：`model_specs/<spec>` 的文件名主干就是上游 family 名，`server.json` 里的
/// `family` 字段实测与之一致（见 engine-lock.json 与 audio.cpp 的 `model_specs/`）。
pub fn family_from_spec(spec: &str) -> String {
    spec.strip_suffix(".json").unwrap_or(spec).to_string()
}

/// 从 family 推引擎 `task`：`asr` / `tts` / 其余归 `gen`。
///
/// 只用于**自动生成**的 server.json；用户自己那份 server.json 的 task 永远优先
/// （那是上游写的，比我们的推断可信）。
pub fn task_from_family(family: &str) -> &'static str {
    if family.contains("asr") {
        "asr"
    } else if family.contains("tts") {
        "tts"
    } else {
        "gen"
    }
}

/// 在模型目录里找出**已下载**、且能被随包清单识别的模型。
///
/// 匹配依据是清单里每条包的 `local_paths`（相对模型目录）解析出的绝对路径是否存在 ——
/// 不靠文件名猜家族。`model_dir` 是壳的「模型目录」唯一入口给出的路径。
pub fn managed_models(
    model_dir: &Path,
    catalog: &crate::model_sources::Catalog,
) -> Vec<ManagedModel> {
    let mut out: Vec<ManagedModel> = Vec::new();
    for m in &catalog.models {
        let Some(entry) = m.entry.as_ref() else {
            continue;
        };
        let Some(rel) = entry.local_paths.first() else {
            continue;
        };
        let abs = model_dir.join(rel);
        if !abs.is_file() {
            continue;
        }
        let family = family_from_spec(&m.spec);
        // 同一个 family 可能有多条清单（不同量化档/流式变体）指向同一个文件，
        // 只留第一条：引擎按 family 找模型，重复条目没有意义。
        if out.iter().any(|e| e.family == family) {
            continue;
        }
        out.push(ManagedModel {
            id: m.id.clone(),
            task: task_from_family(&family).to_string(),
            family,
            path: abs.display().to_string(),
        });
    }
    out
}

/// 生成 `server.json` 文本（纯函数，便于单测与排障时肉眼核对）。
pub fn render_server_config(
    host: &str,
    port: u16,
    backend: &str,
    models: &[ManagedModel],
) -> String {
    let entries: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "family": m.family,
                "path": m.path,
                "task": m.task,
                "mode": "offline",
            })
        })
        .collect();
    let cfg = serde_json::json!({
        "host": host,
        "port": port,
        "backend": backend,
        "threads": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        "lazy_load": true,
        "models": entries,
        "max_loaded_models": 2,
        "idle_unload_ms": 300_000,
        "min_free_memory_mb": 0,
    });
    serde_json::to_string_pretty(&cfg).unwrap_or_default()
}

/// 进程监护：**自己拉起的引擎不能变成孤儿**。
///
/// 为什么需要它：壳被 `SIGTERM`/崩溃带走时 `ui.run()` 不会正常返回，`stop()` 没机会跑
/// —— 实测 `pkill` 壳之后，包内引擎继续占着端口活下去。`Drop` 也兜不住：进程被信号
/// 直接终结时 Rust 不跑析构。
///
/// 做法：**壳侧**再起一个线程（`--engine-monitor <壳pid> <引擎pid>`），每秒看一次壳还在不在，
/// 壳没了就把引擎收掉再退出。不用"引擎自己监视父进程"，是因为那要改 audio.cpp；壳侧线程
/// 只需要自己的 pid 与引擎 pid，不动引擎一行代码。
///
/// 局限（如实写在这里，别让人以为万能）：Windows 上 `kill(pid, 0)` 不可用，
/// 当前实现只覆盖 Unix；Windows 靠用户正常关闭窗口（走 `stop()`）与安装器卸载兜底，
/// 强杀进程组那条 P1 再补（需要 `OpenProcess`/Job Object）。
pub const ENGINE_MONITOR_ARG: &str = "--engine-monitor";

/// `kill(pid, 0)`：只探测目标是否存在，不发任何信号。
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

#[cfg(not(unix))]
fn process_alive(pid: i32) -> bool {
    let _ = pid;
    true
}

/// 一次监视动作：壳没了就收引擎。返回 true 表示"已经收掉，监视者该结束了"。
///
/// 拆成 `monitor_once` 是为了**可测**：`run_monitor` 里那一步是 `std::process::exit`，
/// 直接在测试里调会把测试进程一起杀掉（实测：测试结果根本打不出来）。
#[cfg(unix)]
pub fn monitor_once(shell_pid: i32, engine_pid: i32) -> bool {
    if process_alive(shell_pid) {
        return false;
    }
    unsafe {
        libc::kill(engine_pid, libc::SIGTERM);
    }
    // 给它一秒收尾的机会，再硬杀 —— 引擎当前没有优雅停机路径，
    // 这一步只是尽量让它有机会自己释放端口/显存。
    std::thread::sleep(Duration::from_secs(1));
    if process_alive(engine_pid) {
        unsafe {
            libc::kill(engine_pid, libc::SIGKILL);
        }
    }
    true
}

#[cfg(not(unix))]
pub fn monitor_once(shell_pid: i32, engine_pid: i32) -> bool {
    let _ = (shell_pid, engine_pid);
    false
}

/// 监视线程主体：壳没了就收掉引擎，然后自己退出。
#[cfg(unix)]
pub fn run_monitor(shell_pid: i32, engine_pid: i32) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if monitor_once(shell_pid, engine_pid) {
            std::process::exit(0);
        }
    }
}

#[cfg(not(unix))]
pub fn run_monitor(shell_pid: i32, engine_pid: i32) {
    let _ = (shell_pid, engine_pid);
}

/// `main` 最开头判这一条：本次运行是监护线程而不是正常启动。
/// 返回 true 表示"已处理完，调用方应直接退出"。
pub fn monitor_entry(args: &[String]) -> bool {
    let Some(pos) = args.iter().position(|a| a == ENGINE_MONITOR_ARG) else {
        return false;
    };
    let shell_pid = args
        .get(pos + 1)
        .and_then(|p| p.parse::<i32>().ok())
        .unwrap_or(0);
    let engine_pid = args
        .get(pos + 2)
        .and_then(|p| p.parse::<i32>().ok())
        .unwrap_or(0);
    run_monitor(shell_pid, engine_pid);
    true
}

/// 托管实例：持有子进程句柄，**只回收自己拉起的那一个**。
#[derive(Debug, Default)]
pub struct EngineSupervisor {
    child: Option<Child>,
    /// 监视进程句柄（壳正常退出时一并回收）。
    monitor: Option<Child>,
}

impl EngineSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 按上文的优先级确保服务可用。
    ///
    /// `base` 是壳解析出的生效地址（`resolve_base` 的结果）；`explicit` 表示地址来自
    /// 用户显式配置（AW_SERVER / 全局设置）—— 那种情况一律不拉起内置引擎。
    pub fn ensure_serving(
        &mut self,
        base: &str,
        explicit: bool,
        model_dir: &Path,
        catalog: &crate::model_sources::Catalog,
        data_dir: &Path,
    ) -> StartOutcome {
        if explicit {
            return StartOutcome::Reused;
        }
        let host = base
            .split_once("://")
            .map(|(_, r)| r.to_string())
            .unwrap_or_else(|| base.to_string());
        if !is_loopback_host(&host) {
            return StartOutcome::Reused;
        }
        if healthy(base) {
            return StartOutcome::Reused;
        }
        let Some(bin) = engine_binary() else {
            return StartOutcome::EngineMissing;
        };
        let models = managed_models(model_dir, catalog);
        if models.is_empty() {
            return StartOutcome::NotConfigured;
        }
        let port = port_of(base).unwrap_or(8080);
        let backend = std::env::var("AW_BACKEND").unwrap_or_else(|_| default_backend().to_string());
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            return StartOutcome::Failed(format!("建数据目录失败：{e}"));
        }
        let cfg_path = data_dir.join("server.json");
        let cfg_text = render_server_config("127.0.0.1", port, &backend, &models);
        if let Err(e) = std::fs::write(&cfg_path, &cfg_text) {
            return StartOutcome::Failed(format!("写 server.json 失败：{e}"));
        }
        let log_path = data_dir.join("engine.log");
        let log = match std::fs::File::create(&log_path) {
            Ok(f) => f,
            Err(e) => return StartOutcome::Failed(format!("建日志文件失败：{e}")),
        };
        let log_err = match log.try_clone() {
            Ok(f) => f,
            Err(e) => return StartOutcome::Failed(format!("日志句柄复制失败：{e}")),
        };
        let mut cmd = Command::new(&bin);
        cmd.arg("--config")
            .arg(&cfg_path)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--backend")
            .arg(&backend)
            .arg("--log")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        // 引擎是随包只读文件；工作目录给数据目录，避免它在安装目录里写东西。
        cmd.current_dir(data_dir);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return StartOutcome::Failed(format!("拉起引擎失败：{e}")),
        };
        let engine_pid = child.id();
        self.child = Some(child);
        // 壳侧监视进程：壳被强杀时由它收掉引擎（见 run_monitor 的说明）。
        if let Ok(exe) = std::env::current_exe() {
            let mut mon = Command::new(exe);
            mon.arg(ENGINE_MONITOR_ARG)
                .arg(std::process::id().to_string())
                .arg(engine_pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(m) = mon.spawn() {
                self.monitor = Some(m);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if healthy(base) {
                return StartOutcome::Started;
            }
            // 子进程已经退出就别等满 30s —— 立刻把退出码报出来。
            if let Some(child) = self.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        self.child = None;
                        return StartOutcome::Failed(format!(
                            "引擎启动即退出（{status}），日志：{}",
                            log_path.display()
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        return StartOutcome::Failed(format!("等待引擎失败：{e}"));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        StartOutcome::Failed(format!("引擎 30s 内没就绪，日志：{}", log_path.display()))
    }

    /// 回收自己拉起的引擎。**必须只在壳退出时调用**，且只作用于自己那份句柄。
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut mon) = self.monitor.take() {
            let _ = mon.kill();
            let _ = mon.wait();
        }
    }
}

impl Drop for EngineSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `GET /health` 探活：连接失败或返回里没有 `"ok"` 都算没服务。
///
/// 超时给得短（1s）：这是**启动路径**上的探测，用户等不起 5s 的客户端超时
/// （客户端那个 5s 是合成调用的，语境不同）。
pub fn healthy(base: &str) -> bool {
    ureq::get(&format!("{base}/health"))
        .timeout(Duration::from_secs(1))
        .call()
        .map(|r| r.into_string().unwrap_or_default().contains("\"ok\""))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> &'static crate::model_sources::Catalog {
        crate::model_sources::catalog().expect("内置清单应可解析")
    }

    #[test]
    fn loopback_detection_covers_the_shapes_we_accept() {
        assert!(is_loopback_host("127.0.0.1:8080"));
        assert!(is_loopback_host("http://127.0.0.1:8080"));
        assert!(is_loopback_host("localhost:8080"));
        assert!(is_loopback_host("http://[::1]:8080"));
        // 非回环一律不许拉内置引擎：那是别人的机器
        assert!(!is_loopback_host("192.168.1.10:8080"));
        assert!(!is_loopback_host("http://example.com:8080"));
    }

    #[test]
    fn port_parsing_falls_back_cleanly() {
        assert_eq!(port_of("http://127.0.0.1:8080"), Some(8080));
        assert_eq!(port_of("127.0.0.1:9"), Some(9));
        assert_eq!(port_of("http://127.0.0.1"), None);
    }

    #[test]
    fn family_and_task_follow_the_spec_name() {
        assert_eq!(family_from_spec("audio8_tts.json"), "audio8_tts");
        assert_eq!(family_from_spec("ace_step.json"), "ace_step");
        assert_eq!(task_from_family("audio8_tts"), "tts");
        assert_eq!(task_from_family("qwen3_asr"), "asr");
        assert_eq!(task_from_family("ace_step"), "gen");
    }

    #[test]
    fn rendered_config_carries_the_models_engine_needs_to_boot() {
        let models = vec![ManagedModel {
            id: "audio8-tts".into(),
            family: "audio8_tts".into(),
            path: "/models/a.gguf".into(),
            task: "tts".into(),
        }];
        let text = render_server_config("127.0.0.1", 8080, "metal", &models);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["host"], "127.0.0.1");
        assert_eq!(v["port"], 8080);
        assert_eq!(v["backend"], "metal");
        // 引擎没有非空 models 就拒绝启动（config.cpp:275）——这条是硬要求
        assert_eq!(v["models"].as_array().unwrap().len(), 1);
        assert_eq!(v["models"][0]["family"], "audio8_tts");
        assert_eq!(v["models"][0]["task"], "tts");
        assert_eq!(v["models"][0]["mode"], "offline");
    }

    /// 模型目录里没有已下载模型时，`managed_models` 必须为空 —— 调用方据此报
    /// NotConfigured，而不是写一份空 models 的配置让引擎启动失败。
    #[test]
    fn missing_models_yield_no_entries() {
        let dir = std::env::temp_dir().join(format!("aw-eng-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let got = managed_models(&dir, catalog());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(got.is_empty(), "空目录不该产出条目：{got:?}");
    }

    /// 监视逻辑必须真的能收掉引擎 —— 这是"壳被强杀后不残留孤儿"的唯一保证。
    ///
    /// 用一个假的"引擎"（`sleep`）代替真引擎：这里验的是**回收语义**，与引擎是谁无关，
    /// 也不该依赖真引擎产物（CI 上没有）。
    #[cfg(unix)]
    #[test]
    fn monitor_reaps_a_child_after_the_shell_is_gone() {
        use std::process::Command;
        // 假引擎：活得比测试久
        let mut fake = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep 应可执行");
        let engine_pid = fake.id() as i32;
        // 壳也用一个真进程再**杀掉**：直接取"某个立刻退出的进程的 pid"会踩 pid 复用
        // （刚释放的号可能马上被别的进程拿走，`kill(pid,0)` 于是恒真、监视者永远不收）。
        let mut shell = Command::new("sleep").arg("30").spawn().unwrap();
        let shell_pid = shell.id() as i32;
        let _ = shell.kill();
        let _ = shell.wait();
        // 等到它真的从进程表消失
        let gone_deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(shell_pid) && Instant::now() < gone_deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(shell_pid), "前置条件：壳进程必须已经不在了");

        // 直接跑一次监视动作：壳已死 → 它必须收掉引擎
        assert!(
            monitor_once(shell_pid, engine_pid),
            "壳不在时监视动作必须报告'已收掉'"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut gone = false;
        while Instant::now() < deadline {
            if fake.try_wait().ok().flatten().is_some() {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !gone {
            let _ = fake.kill();
        }
        assert!(gone, "假引擎必须被收掉，否则真实场景就是孤儿进程");
        // 反过来：壳还活着时不许动引擎
        assert!(
            !monitor_once(std::process::id() as i32, engine_pid),
            "壳活着的时候监视者不该收引擎"
        );
    }

    /// **真起来一个引擎**：只在本机手动跑（`--ignored`），因为它要启动真实进程、
    /// 占端口、并且依赖 `AW_ENGINE_DIR` 指向一份真引擎。
    ///
    /// 覆盖的是托管的核心承诺：挑一个空闲端口 → 写配置 → 拉起 → `/health` 就绪 →
    /// `stop()` 之后进程确实没了。CI 不跑（无引擎产物 + 不想占端口）。
    #[test]
    #[ignore = "需要真实引擎与网络端口；本机手动: cargo test --bin audio-workshop spawns_embedded_engine -- --ignored"]
    fn spawns_embedded_engine_and_reaps_it() {
        if engine_binary().is_none() {
            eprintln!(
                "跳过：AW_ENGINE_DIR 没指向含 {} 的目录",
                engine_binary_name()
            );
            return;
        }
        // 占一个端口拿到空闲号，再放掉 —— 比写死端口稳（别的进程可能占着）
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let base = format!("http://127.0.0.1:{port}");

        // 造一个"已下载"的模型：路径形状必须与随包清单里某条的 local_paths 一致
        let root = std::env::temp_dir().join(format!("aw-eng-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cat = catalog();
        let (rel, _id, _spec) = cat
            .models
            .iter()
            .find_map(|m| {
                let e = m.entry.as_ref()?;
                Some((e.local_paths.first()?.clone(), m.id.clone(), m.spec.clone()))
            })
            .expect("清单里应有带 local_paths 的包");
        let models_dir = root.join("models");
        let abs = models_dir.join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, b"not-a-real-model").unwrap();
        let data_dir = root.join("engine");

        let mut sup = EngineSupervisor::new();
        let outcome = sup.ensure_serving(&base, false, &models_dir, cat, &data_dir);
        // 引擎可能因为权重是假的而在**加载时**失败，但托管本身（写配置/拉起/判定）必须成立：
        // 要么就绪，要么明确报 Failed 并带上日志路径 —— 不许静默什么都不做。
        match &outcome {
            StartOutcome::Started => {
                assert!(healthy(&base), "报 Started 就必须真的健康");
            }
            StartOutcome::Failed(why) => {
                assert!(
                    why.contains("engine.log") || why.contains("server.json"),
                    "失败原因要指向日志或配置，实际：{why}"
                );
            }
            other => panic!("不该是这个结果：{other:?}"),
        }
        assert!(data_dir.join("server.json").is_file(), "配置必须落盘供排障");
        sup.stop();
        // 回收后端口必须真的空出来（否则是"僵尸引擎"占着 8080 这类难查故障）
        std::thread::sleep(std::time::Duration::from_millis(800));
        assert!(!healthy(&base), "stop() 之后不该还有服务在响应");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 造一个与清单 `local_paths` 对得上的假文件，条目必须带正确的 family/task。
    #[test]
    fn downloaded_model_is_recognised_by_catalog_paths() {
        let dir = std::env::temp_dir().join(format!("aw-eng-hit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cat = catalog();
        // 取清单里第一条有 local_paths 的条目来造文件，避免测试里写死某个模型的布局
        let (rel, id, spec) = cat
            .models
            .iter()
            .find_map(|m| {
                let e = m.entry.as_ref()?;
                let rel = e.local_paths.first()?.clone();
                Some((rel, m.id.clone(), m.spec.clone()))
            })
            .expect("清单里应至少有一条带 local_paths 的包");
        let abs = dir.join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, b"x").unwrap();

        let got = managed_models(&dir, cat);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(got.len(), 1, "应只识别出一条：{got:?}");
        assert_eq!(got[0].id, id);
        assert_eq!(got[0].family, family_from_spec(&spec));
        assert_eq!(got[0].path, abs.display().to_string());
    }
}
