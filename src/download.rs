//! 增强模型下载器核心（M4-P7）：N 并发队列 + 断点续传 + 校验后提交。
//!
//! 设计要点（与仓库既有的落盘约定一致，见 docs/robustness.md）：
//!   · 先写 `<目标>.part`，**校验通过才 rename 成正式文件**——正式路径要么没有，
//!     要么是完整的；中断只留下可续传的 `.part`，不留半成品。
//!   · 续传发 `Range: bytes=<已下载>-`，**只有服务器回 206 才算续传**；回 200
//!     就是服务器不认 Range，如实从 0 重下并在状态里标出来，绝不假装续传。
//!   · 没有期望 sha256 时按声明大小（清单里的 size，或响应 Content-Length）核大小。
//!
//! 队列跑在 `effective_concurrency()` 个后台 worker 上（默认 2，可配 1..=4）；
//! `enqueue` 只按目标路径去重 + 登记，`cancel` 通过原子标志让正在下载的读循环尽快
//! 退出——取消不会阻塞、也不需要等网络超时。
//!
//! **同一目标路径只允许一个 writer**：`.part` + `rename` 的提交点语义在
//! 两个 writer 下会坏（两次 append 交错、长度还可能刚好等于声明大小），
//! 所以在入队时按 dest 去重（见 `Enqueued::Duplicate`）。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// 读盘缓冲区：64 KiB 在进度粒度与系统调用次数之间取平衡。
const CHUNK: usize = 64 * 1024;

/// 网络超时。用**单次 socket 读超时**而不是 ureq 的总超时：总超时会把"下 2 GB 权重"
/// 整条判失败，读超时只判"连接还在但一直不给数据"——正是"服务端卡住，用户点了取消
/// 却一直等"的场景（ureq 2 默认**没有任何超时**，会一直阻塞在 read 上）。
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub connect: Duration,
    pub read: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: Duration::from_secs(15),
            read: Duration::from_secs(30),
        }
    }
}

/// `Downloader::cancel` 的结果（UI 要据此说对话，不能把"其实已经结束"说成"已取消"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 这次真的置上了取消标志（第一次请求取消）
    Requested,
    /// 之前已经请求过取消，任务还在收尾——这次是 no-op
    AlreadyRequested,
    /// 任务已经不在队列里（终态已出），没有什么可取消的
    Finished,
}

/// 单任务状态机。`Failed` 带原因，`Done`/`Cancelled` 终态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// 已入队、还没轮到
    Queued,
    /// 正在下载
    Downloading,
    /// 下载完、正在校验（sha256 / 大小）
    Verifying,
    /// 校验通过、已 rename 成正式文件
    Done,
    /// 失败（原因已写在快照的 note 里，这里也留一份结构化文案）
    Failed(String),
    /// 用户取消
    Cancelled,
}

impl State {
    /// 是否终态（终态不再变化；UI 据此决定按钮文案）。
    pub fn is_terminal(&self) -> bool {
        matches!(self, State::Done | State::Failed(_) | State::Cancelled)
    }

    /// 界面上的状态文案。失败原因已经拼在 note 里，这里只给短标签。
    pub fn label(&self) -> &'static str {
        match self {
            State::Queued => "排队",
            State::Downloading => "下载中",
            State::Verifying => "校验中",
            State::Done => "已完成",
            State::Failed(_) => "失败",
            State::Cancelled => "已取消",
        }
    }
}

/// 一条下载任务：下载什么、放哪、校验依据。
#[derive(Clone, Debug)]
pub struct TaskSpec {
    /// 展示名（模型 id）
    pub label: String,
    pub url: String,
    /// 正式目标路径（不是 `.part`）
    pub dest: PathBuf,
    /// 期望 sha256（清单里给了就必校验）
    pub expected_sha256: Option<String>,
    /// 期望字节数（没给 sha256 时按它核大小；都缺则按响应声明的总长核）
    pub expected_size: Option<u64>,
}

/// 任务快照：每次状态/进度变化都会推一份给 UI。
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: u64,
    pub label: String,
    pub dest: PathBuf,
    pub state: State,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// 续传成功标 true；服务器不支持 Range、本次从 0 重下时写清说明。
    pub note: String,
}

impl Snapshot {
    /// 0..1 的进度；总长未知时返回 0（UI 会退化成"已下载 X"文案）。
    pub fn fraction(&self) -> f32 {
        match self.total {
            Some(t) if t > 0 => (self.downloaded as f64 / t as f64).clamp(0.0, 1.0) as f32,
            _ => 0.0,
        }
    }
}

/// 下载失败原因（可执行：带期望值/实际值或路径）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadError {
    Cancelled,
    Http(String),
    Io(String),
    /// sha256 对不上（期望 vs 实际）
    Sha256Mismatch {
        expected: String,
        got: String,
    },
    /// 大小对不上（期望 vs 实际）
    SizeMismatch {
        expected: u64,
        got: u64,
    },
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Cancelled => write!(f, "已取消"),
            DownloadError::Http(e) => write!(f, "网络错误：{e}"),
            DownloadError::Io(e) => write!(f, "写盘错误：{e}"),
            DownloadError::Sha256Mismatch { expected, got } => write!(
                f,
                "sha256 校验失败（期望 {expected}，实际 {got}）——文件可能被截断或被换源，已丢弃"
            ),
            DownloadError::SizeMismatch { expected, got } => write!(
                f,
                "大小校验失败（期望 {expected} 字节，实际 {got} 字节）——已丢弃"
            ),
        }
    }
}

impl std::error::Error for DownloadError {}

/// 下载结果：给调用方解释"这次到底续了没"。
///
/// **`download()` 不推终态快照**：终态只有一处产出——`worker_loop` 在
/// `finish_job`（摘 flag / 放开 dest 名额 / 减 in_flight）**之后**推的那一条。
/// 这是 `Downloader` 契约"看到终态 ⇒ 已完全收尾"的实现方式。早期版本在
/// `download()` 内部也推一条 Done，而那条必然早于收尾，于是留出窗口期
/// （cancel 报 AlreadyRequested、同 dest 重排被判 Duplicate）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// 最终文件字节数
    pub bytes: u64,
    /// 本次真的从断点续上了（服务器给了 206）
    pub resumed: bool,
    /// 本地有 `.part` 但服务器不支持 Range，从 0 重下了
    pub restarted_from_zero: bool,
}

/// `<目标>.part` 的路径（在正式文件名后追加，不改扩展名）。
pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// 下载单条任务（阻塞；在后台线程里跑）。
///
/// `cancel` 为 true 时读循环尽快退出：保留 `.part` 以便下次续传，但不碰正式路径。
/// 校验失败时删 `.part`（期望的是"重下"，留着坏字节反而会污染下次续传）。
pub fn download(
    spec: &TaskSpec,
    cancel: &AtomicBool,
    on_progress: impl FnMut(&Snapshot),
) -> Result<Outcome, DownloadError> {
    download_with(spec, cancel, Timeouts::default(), on_progress)
}

/// 同 [`download`]，但可注入超时（单测用它把"服务端卡住"压到毫秒级）。
pub fn download_with(
    spec: &TaskSpec,
    cancel: &AtomicBool,
    timeouts: Timeouts,
    mut on_progress: impl FnMut(&Snapshot),
) -> Result<Outcome, DownloadError> {
    // 还没开始就被取消：一个网络请求都不发（取消排队中的任务走这条）
    if cancel.load(Ordering::Relaxed) {
        return Err(DownloadError::Cancelled);
    }
    let agent = ureq::builder()
        .timeout_connect(timeouts.connect)
        .timeout_read(timeouts.read)
        .build();
    if let Some(parent) = spec.dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DownloadError::Io(format!("创建目录 {} 失败：{e}", parent.display()))
            })?;
        }
    }
    let part = part_path(&spec.dest);

    // 本地已有断点：先按它发 Range；服务器回 416（断点比文件还大，常见于换源/文件变小）
    // 时删掉重来一次，最多两轮，避免"坏 .part 永远续不上"。
    let mut offset = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut restarted_from_zero = false;
    let mut allow_retry = offset > 0;

    let (mut reader, mut writer, mut downloaded, total, resumed) = loop {
        // 循环顶也查一次取消：416（断点不合法）会让这里再补发一次 GET，
        // 没有这一句的话"取消后不再有网络请求"在 416 这条路上不成立。
        if cancel.load(Ordering::Relaxed) {
            return Err(DownloadError::Cancelled);
        }
        let resp = send_get(&agent, &spec.url, offset)?;
        let status = resp.status();
        match status {
            206 if offset > 0 => {
                // 服务器认了 Range：追加写，进度从已下载起算
                let file = open_append(&part)?;
                let total = content_total(&resp, offset);
                break (resp.into_reader(), file, offset, total, true);
            }
            416 if allow_retry => {
                // 断点不合法（比当前文件还大）：丢弃本地 .part，从 0 重来（只重试一次）
                let _ = std::fs::remove_file(&part);
                offset = 0;
                allow_retry = false;
                restarted_from_zero = true;
                continue;
            }
            200 => {
                // 不是 206：服务器不支持 Range（或本地没断点）→ 如实从 0 写
                if offset > 0 {
                    restarted_from_zero = true;
                }
                let file = open_truncate(&part)?;
                let total = resp
                    .header("Content-Length")
                    .and_then(|v| v.parse::<u64>().ok());
                break (resp.into_reader(), file, 0u64, total, false);
            }
            other => {
                // 错误也要读 body，否则 403/404 的原因（限流 / 需要授权 / 源没了）看不见
                let text = resp.into_string().unwrap_or_default();
                let reason: String = text.trim().chars().take(200).collect();
                return Err(DownloadError::Http(if reason.is_empty() {
                    format!("服务器返回 HTTP {other}（{}）", spec.url)
                } else {
                    format!("服务器返回 HTTP {other}：{reason}（{}）", spec.url)
                }));
            }
        }
    };

    let note = if resumed {
        format!("从 {downloaded} 字节断点续传")
    } else if restarted_from_zero {
        "服务器不支持续传，已从 0 重新下载".to_string()
    } else {
        String::new()
    };
    let mut snap = Snapshot {
        id: 0,
        label: spec.label.clone(),
        dest: spec.dest.clone(),
        state: State::Downloading,
        downloaded,
        total,
        note: note.clone(),
    };

    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            // 保留 .part：下次还能续；正式路径一个字节都没动
            return Err(DownloadError::Cancelled);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| DownloadError::Http(format!("读取响应失败：{e}")))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        downloaded += n as u64;
        snap.downloaded = downloaded;
        snap.total = total.or(Some(downloaded));
        if !note.is_empty() {
            // 续传/重下说明要一直带着，别被后续进度覆盖掉
            snap.note = note.clone();
        }
        on_progress(&snap);
    }
    writer
        .flush()
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    writer
        .sync_all()
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    drop(writer);

    // ── 校验（通过才提交）──
    snap.state = State::Verifying;
    snap.downloaded = downloaded;
    snap.total = total.or(Some(downloaded));
    snap.note = "校验中".to_string();
    on_progress(&snap);

    let got = std::fs::metadata(&part)
        .map(|m| m.len())
        .unwrap_or(downloaded);
    let known_total = spec.expected_size.or(total);
    if let Some(exp) = spec.expected_sha256.as_deref() {
        let actual = sha256_file(&part).map_err(|e| DownloadError::Io(e.to_string()))?;
        if !sha256_eq(exp, &actual) {
            let _ = std::fs::remove_file(&part);
            return Err(DownloadError::Sha256Mismatch {
                expected: exp.trim().to_ascii_lowercase(),
                got: actual,
            });
        }
    }
    if let Some(exp) = known_total {
        if got != exp {
            let _ = std::fs::remove_file(&part);
            return Err(DownloadError::SizeMismatch { expected: exp, got });
        }
    }

    // ── 提交点：rename 到正式路径（同目录内 rename 是原子的）──
    std::fs::rename(&part, &spec.dest)
        .map_err(|e| DownloadError::Io(format!("重命名到 {} 失败：{e}", spec.dest.display())))?;

    // 终态文案不在这里拼：终态快照由 `worker_loop` 在收尾后推，
    // 文案也在 `run_one` 里按 `Outcome` 三个字段现算（一处措辞）。
    Ok(Outcome {
        bytes: got,
        resumed,
        restarted_from_zero,
    })
}

/// 发一次 GET；`offset > 0` 时带 Range 头。`Accept-Encoding: identity` 保证
/// 字节流与 Content-Length 对齐（透明 gzip 会让续传的偏移全错）。
fn send_get(agent: &ureq::Agent, url: &str, offset: u64) -> Result<ureq::Response, DownloadError> {
    let mut req = agent.get(url).set("Accept-Encoding", "identity");
    if offset > 0 {
        req = req.set("Range", &format!("bytes={offset}-"));
    }
    match req.call() {
        Ok(resp) => Ok(resp),
        // 4xx/5xx 在 ureq 里是 Err(Status(code, resp))：这里**把响应本体交回给上层**，
        // 由上层按状态码分支——416（断点不合法）要能走补救，其它错误要能读 body 说清原因。
        // 只在这里把响应当 Err 吞掉的话，416 补救分支是死代码，永远走不到。
        Err(ureq::Error::Status(_, resp)) => Ok(resp),
        Err(other) => Err(DownloadError::Http(other.to_string())),
    }
}

/// 响应声明的**总**字节数：优先解析 `Content-Range: bytes a-b/total`
/// （206 的 Content-Length 只是剩余部分），没有就按 `offset + Content-Length`。
fn content_total(resp: &ureq::Response, offset: u64) -> Option<u64> {
    if let Some(cr) = resp.header("Content-Range") {
        if let Some(total) = cr
            .rsplit('/')
            .next()
            .and_then(|t| t.trim().parse::<u64>().ok())
        {
            return Some(total);
        }
    }
    resp.header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|len| len + offset)
}

fn open_append(path: &Path) -> Result<std::fs::File, DownloadError> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| DownloadError::Io(format!("打开断点文件失败：{e}")))
}

fn open_truncate(path: &Path) -> Result<std::fs::File, DownloadError> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| DownloadError::Io(format!("创建下载文件失败：{e}")))
}

/// 文件 sha256 的十六进制小写串。
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// sha256 比较：大小写不敏感、忽略首尾空白（清单里手抄的大小写不一很常见）。
fn sha256_eq(expected: &str, actual: &str) -> bool {
    expected.trim().eq_ignore_ascii_case(actual)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ===========================================================================
// 队列：N 并发、目标去重、可取消
// ===========================================================================

/// 并发上限（用户可配 1..=4）。装模型是网络密集 + 磁盘密集各占一半，2 条并排
/// 能把"一次装多个"的等待折半，又不会把带宽切成四份导致每条都更慢。
pub const MAX_CONCURRENCY: u32 = 4;

/// 默认并发。
pub const DEFAULT_CONCURRENCY: u32 = 2;

/// 把配置里的并发数夹到有效区间：缺省 / 0 / 越界都回落 2。
///
/// 这是**唯一**的并发数归一化入口：`Downloader` 的线程数与界面回显都从它算，
/// 免得界面写"2 并发"、队列其实起了 4 个。
pub fn effective_concurrency(configured: Option<u32>) -> u32 {
    configured
        .unwrap_or(DEFAULT_CONCURRENCY)
        .clamp(1, MAX_CONCURRENCY)
}

/// `enqueue` 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enqueued {
    /// 真的排上了：`id` 是它的任务 id
    Started(u64),
    /// 已有一个活动任务在写同一个目标路径——第二个 writer 会把 `.part`
    /// 搅坏（两个 append 交错、长度还可能刚好对上声明大小），所以**拒绝**，
    /// 返回已经在跑的那个 id 让调用方说清"它已经在队列里了"。
    Duplicate { existing: u64 },
}

/// 一条排队中的任务（id + 规格 + 它自己的取消标志）。
struct Job {
    id: u64,
    spec: TaskSpec,
    cancel: Arc<AtomicBool>,
}

/// 队列共享状态。锁只保护下面这几个容器，**不覆盖下载过程**——下载在各自的
/// worker 线程里跑，否则"并行"会退化成串行。
struct QueueState {
    /// 待执行（FIFO）
    queued: VecDeque<Job>,
    /// 目标路径 → 已有活动任务 id。入队去重与 worker 收尾都维护它。
    active_dests: HashMap<PathBuf, u64>,
    /// 排队中 + 在跑中的任务数（判并发上限）
    in_flight: u32,
    /// 还活着的 worker 线程数（Drop 时递减，减到 0 让阻塞等待的线程退出）
    alive: u32,
}

/// 下载队列句柄。
///
/// `enqueue` 把任务交给队列（按目标路径去重 + 受并发上限约束）；
/// `concurrency` 个后台线程各自取一条执行，一条失败/取消不影响其它。
pub struct Downloader {
    state: Arc<(Mutex<QueueState>, Condvar)>,
    flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    next_id: Arc<AtomicU64>,
    notify: Arc<dyn Fn(Snapshot) + Send + Sync>,
    workers: u32,
}

impl Downloader {
    /// 起 `concurrency`（夹到 1..=4）个后台 worker；`notify` 在每次状态/进度变化时
    /// 被调用（在 worker 线程里）。
    pub fn new<F>(notify: F, concurrency: Option<u32>) -> Downloader
    where
        F: Fn(Snapshot) + Send + Sync + 'static,
    {
        let workers = effective_concurrency(concurrency);
        let state = Arc::new((
            Mutex::new(QueueState {
                queued: VecDeque::new(),
                active_dests: HashMap::new(),
                in_flight: 0,
                alive: workers,
            }),
            Condvar::new(),
        ));
        let flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let notify: Arc<dyn Fn(Snapshot) + Send + Sync> = Arc::new(notify);
        for _ in 0..workers {
            let state = Arc::clone(&state);
            let flags = Arc::clone(&flags);
            let notify = Arc::clone(&notify);
            std::thread::spawn(move || worker_loop(state, flags, notify));
        }
        Downloader {
            state,
            flags,
            next_id,
            notify,
            workers,
        }
    }

    /// 入队一条任务。同一目标路径若已有活动任务（排队中或在跑），**拒绝**第二个
    /// writer 并返回它的 id。
    ///
    /// `mirror` 把清单里的原始 URL 改写成当前生效的源——**在入队时**应用，
    /// 所以"入队后改设置"不会让已经在排的旧任务偷偷换源（用户看到什么就下什么）。
    pub fn enqueue<G>(&self, spec: TaskSpec, mirror: G) -> Enqueued
    where
        G: FnOnce(&str) -> String,
    {
        let dest = spec.dest.clone();
        let (lock, cvar) = &*self.state;
        let (id, spec, cancel) = {
            let mut g = lock.lock().unwrap();
            while g.in_flight >= self.workers {
                g = cvar.wait(g).unwrap();
            }
            if let Some(existing) = g.active_dests.get(&dest).copied() {
                return Enqueued::Duplicate { existing };
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let cancel = Arc::new(AtomicBool::new(false));
            let spec = TaskSpec {
                url: mirror(&spec.url),
                ..spec
            };
            g.active_dests.insert(dest, id);
            g.in_flight += 1;
            g.queued.push_back(Job {
                id,
                spec: spec.clone(),
                cancel: Arc::clone(&cancel),
            });
            (id, spec, cancel)
        };
        self.flags.lock().unwrap().insert(id, cancel);
        cvar.notify_one();
        // 快照在锁外推：notify 是调用方的闭包，锁内调用可能反向加锁
        (self.notify)(Snapshot {
            id,
            label: spec.label.clone(),
            dest: spec.dest.clone(),
            state: State::Queued,
            downloaded: 0,
            total: spec.expected_size,
            note: String::new(),
        });
        Enqueued::Started(id)
    }

    /// 这条任务是否还在登记里（只读；**不给生产代码用**，只给"终态前必须已收尾"那条
    /// 回归用——`cancel()` 有副作用（会置标志），拿它当探针会把下一次查询也污染掉）。
    #[cfg(test)]
    fn is_registered(&self, id: u64) -> bool {
        self.flags.lock().unwrap().contains_key(&id)
    }

    /// 这个目标路径是否还有活动任务登记（只读；同上，测试专用）。
    #[cfg(test)]
    fn has_active_dest(&self, dest: &Path) -> bool {
        self.state.0.lock().unwrap().active_dests.contains_key(dest)
    }

    /// 取消一条任务。正在下载的由原子标志让读循环退出；还没开始的会在轮到它时跳过。
    ///
    /// 返回 [`CancelOutcome`]：任务已经收尾时返回 `Finished`，**不要**把它说成"已取消"
    /// （终态快照可能还在路上，但任务确实已经不在队列里了）。
    pub fn cancel(&self, id: u64) -> CancelOutcome {
        let flags = self.flags.lock().unwrap();
        match flags.get(&id) {
            Some(flag) => {
                if flag.swap(true, Ordering::Relaxed) {
                    CancelOutcome::AlreadyRequested
                } else {
                    CancelOutcome::Requested
                }
            }
            None => CancelOutcome::Finished,
        }
    }
}

impl Drop for Downloader {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.state;
        let mut g = lock.lock().unwrap();
        g.alive = g.alive.saturating_sub(1);
        cvar.notify_all();
    }
}

/// 从共享队列取一条可跑的任务。没有可跑的时：
/// `wait=true` → 阻塞到有任务或被 Drop 唤醒；`wait=false` → 立刻返回 `None`。
fn take_job(state: &Arc<(Mutex<QueueState>, Condvar)>, wait: bool) -> Option<Job> {
    let (lock, cvar) = &**state;
    let mut g = lock.lock().unwrap();
    loop {
        if let Some(job) = g.queued.pop_front() {
            return Some(job);
        }
        if !wait || g.alive == 0 {
            return None;
        }
        g = cvar.wait(g).unwrap();
    }
}

/// 一条活动任务收尾：摘登记、放开名额、唤醒等名额 / 等任务的线程。
fn finish_job(
    state: &Arc<(Mutex<QueueState>, Condvar)>,
    flags: &Mutex<HashMap<u64, Arc<AtomicBool>>>,
    dest: &Path,
    id: u64,
) {
    flags.lock().unwrap().remove(&id);
    let (lock, cvar) = &**state;
    let mut g = lock.lock().unwrap();
    // 只有登记的确实是自己时才摘（入队去重保证同一 dest 不会有第二个，
    // 这条判断让"万一被覆盖"也不会误删活跃登记）。
    if g.active_dests.get(dest).copied() == Some(id) {
        g.active_dests.remove(dest);
    }
    g.in_flight = g.in_flight.saturating_sub(1);
    cvar.notify_all();
}

fn worker_loop(
    state: Arc<(Mutex<QueueState>, Condvar)>,
    flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    notify: Arc<dyn Fn(Snapshot) + Send + Sync>,
) {
    loop {
        let Some(job) = take_job(&state, true) else {
            return;
        };
        let terminal = run_one(&job, &notify);
        // **先收尾、再推终态**：契约是"看到终态 ⇒ 已完全收尾"。顺序反了会留出
        // 一个窗口期（cancel 报 AlreadyRequested、同 dest 重排被判 Duplicate），
        // 下面的 `terminal_snapshot_means_the_task_is_fully_finished` 钉住这条。
        finish_job(&state, &flags, &job.spec.dest, job.id);
        notify(terminal);
    }
}

/// 执行一条任务（含取消检查与终态快照）。
/// 跑一条任务：过程中推进度快照，**返回终态快照**（不自己推）。
///
/// 终态由 `worker_loop` 在 `finish_job` **之后**推——这是 `Downloader` 的一条契约：
/// **看到终态快照 ⇒ 该任务已完全收尾**（flag 已摘、dest 名额已放开、in_flight 已减）。
/// 反过来写（先 notify 再收尾）会留一个窗口期：`cancel(id)` 在窗口内报
/// `AlreadyRequested` 而不是 `Finished`，同一窗口里重排同一 dest 还会被当成
/// `Duplicate` 并指向已完成的旧任务 id——复核实测在负载下约 0.7%~2.4% 命中。
fn run_one(job: &Job, notify: &Arc<dyn Fn(Snapshot) + Send + Sync>) -> Snapshot {
    let id = job.id;
    let spec = &job.spec;
    // 还没开始就被取消（取消排队中的任务走这条）：终态由调用方在收尾后推
    if job.cancel.load(Ordering::Relaxed) {
        return snapshot_of(spec, id, State::Cancelled, 0, None, "已取消");
    }
    notify(snapshot_of(
        spec,
        id,
        State::Downloading,
        0,
        spec.expected_size,
        "",
    ));
    let result = download(spec, &job.cancel, |s| {
        // 进度快照把 id 补上（download 不知道自己的 id）
        let mut s = s.clone();
        s.id = id;
        notify(s);
    });
    match result {
        Ok(out) => {
            let note = if out.resumed {
                "断点续传完成".to_string()
            } else if out.restarted_from_zero {
                "服务器不支持续传，已从 0 重下完成".to_string()
            } else {
                "下载完成".to_string()
            };
            snapshot_of(spec, id, State::Done, out.bytes, Some(out.bytes), &note)
        }
        Err(DownloadError::Cancelled) => snapshot_of(spec, id, State::Cancelled, 0, None, "已取消"),
        Err(e) => snapshot_of(
            spec,
            id,
            State::Failed(e.to_string()),
            0,
            None,
            &e.to_string(),
        ),
    }
}

fn snapshot_of(
    spec: &TaskSpec,
    id: u64,
    state: State,
    downloaded: u64,
    total: Option<u64>,
    note: &str,
) -> Snapshot {
    Snapshot {
        id,
        label: spec.label.clone(),
        dest: spec.dest.clone(),
        state,
        downloaded,
        total,
        note: note.to_string(),
    }
}

// ===========================================================================
// 测试：本地 mock HTTP（std::net::TcpListener，不引第三方 server）
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver};
    use std::time::{Duration, Instant};

    /// 一条被 mock server 收到的请求（记录 Range 头与路径，供断言）。
    #[derive(Clone, Debug, Default)]
    struct Hit {
        path: String,
        range: Option<String>,
    }

    /// 服务端行为：`arm_before_reply` 在回响应**之前**置位（把"取消已经发生"钉成确定事件）；
    /// `stall` 让服务端发完头就不写 body（钉读超时）。
    struct ServeBehavior {
        honor_range: bool,
        arm_before_reply: Option<Arc<AtomicBool>>,
        stall_body: Option<Duration>,
    }

    /// 极简 HTTP/1.1 mock：每个 GET 回一次响应；`honor_range` 决定认不认 Range。
    struct MockServer {
        addr: std::net::SocketAddr,
        hits: Arc<Mutex<Vec<Hit>>>,
        stop: Arc<AtomicBool>,
    }

    impl MockServer {
        fn start(body: Vec<u8>, honor_range: bool) -> MockServer {
            Self::start_with(
                body,
                ServeBehavior {
                    honor_range,
                    arm_before_reply: None,
                    stall_body: None,
                },
            )
        }

        /// 发完响应头就停住不写 body，`stall` 之后才关连接（B2：读超时必须生效）。
        fn start_stalling(len: usize, stall: Duration) -> MockServer {
            Self::start_with(
                vec![0u8; len],
                ServeBehavior {
                    honor_range: false,
                    arm_before_reply: None,
                    stall_body: Some(stall),
                },
            )
        }

        /// 每次服务请求前先把 `flag` 置 true（B3：钉「取消之后不再发请求」）。
        fn start_arming(body: Vec<u8>, honor_range: bool, flag: Arc<AtomicBool>) -> MockServer {
            Self::start_with(
                body,
                ServeBehavior {
                    honor_range,
                    arm_before_reply: Some(flag),
                    stall_body: None,
                },
            )
        }

        fn start_with(body: Vec<u8>, behavior: ServeBehavior) -> MockServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let hits_srv = Arc::clone(&hits);
            let stop_srv = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop_srv.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(mut stream) = stream else { break };
                    serve_one(&mut stream, &body, &behavior, &hits_srv);
                }
            });
            MockServer { addr, hits, stop }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        fn hits(&self) -> Vec<Hit> {
            self.hits.lock().unwrap().clone()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // 唤醒阻塞的 accept，让后台线程看到 stop 后退出
            let _ = TcpStream::connect(self.addr);
        }
    }

    fn serve_one(
        stream: &mut TcpStream,
        body: &[u8],
        behavior: &ServeBehavior,
        hits: &Mutex<Vec<Hit>>,
    ) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                if k.eq_ignore_ascii_case("range") {
                    range = Some(v.trim().to_string());
                }
            }
        }
        hits.lock().unwrap().push(Hit {
            path,
            range: range.clone(),
        });
        // 先置位再回响应：客户端一定是在"标志已置位"之后才读到这个响应，
        // 于是"416 之后还会不会再发请求"变成确定性的（不赌时序）。
        if let Some(flag) = &behavior.arm_before_reply {
            flag.store(true, Ordering::Relaxed);
        }
        // 卡住模式：只发头，body 一直不写，`stall` 之后才关连接
        if let Some(stall) = behavior.stall_body {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            use std::io::Write as _;
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(stall);
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }

        let honor_range = behavior.honor_range;
        let start = range
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes="))
            .and_then(|r| r.split('-').next())
            .and_then(|s| s.trim().parse::<usize>().ok());
        let total = body.len();
        let (status, extra, payload): (&str, String, &[u8]) = match start {
            Some(s) if honor_range && s <= total => (
                "206 Partial Content",
                format!(
                    "Content-Range: bytes {s}-{}/{total}\r\n",
                    total.saturating_sub(1)
                ),
                &body[s..],
            ),
            Some(_) if honor_range => (
                "416 Range Not Satisfiable",
                format!("Content-Range: bytes */{total}\r\n"),
                &[],
            ),
            _ => ("200 OK", String::new(), body),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
            payload.len()
        );
        use std::io::Write as _;
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(payload);
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-download-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex(&h.finalize())
    }

    fn spec(url: String, dest: PathBuf, body: &[u8]) -> TaskSpec {
        TaskSpec {
            label: "测试模型".to_string(),
            url,
            dest,
            expected_sha256: Some(sha256_hex(body)),
            expected_size: Some(body.len() as u64),
        }
    }

    /// 直接跑核心下载（不经队列），收集进度快照。
    fn run(
        spec: &TaskSpec,
        cancel: &AtomicBool,
    ) -> (Result<Outcome, DownloadError>, Vec<Snapshot>) {
        let mut seen = Vec::new();
        let r = download(spec, cancel, |s| seen.push(s.clone()));
        (r, seen)
    }

    #[test]
    fn downloads_full_file_and_commits_by_rename() {
        let root = temp_dir("plain");
        let body = vec![7u8; 200 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("model.bin");
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);
        let (r, seen) = run(&s, &AtomicBool::new(false));
        let out = r.expect("下载应成功");
        assert_eq!(out.bytes, body.len() as u64);
        assert!(!out.resumed && !out.restarted_from_zero);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists(), ".part 提交后不应残留");
        // `download()` 只推进度快照，**不推终态**（终态由 worker 在收尾后推一条）——
        // 所以这里最后一条应是 Verifying。终态那条与"看到终态 ⇒ 已完全收尾"
        // 的不变式由 `terminal_snapshot_means_the_task_is_fully_finished` 覆盖。
        assert!(seen.iter().any(|s| s.state == State::Downloading));
        assert!(seen.iter().any(|s| s.state == State::Verifying));
        assert!(
            seen.iter().all(|s| !s.state.is_terminal()),
            "download() 不该推终态：{:?}",
            seen.iter().map(|s| s.state.clone()).collect::<Vec<_>>()
        );
        assert_eq!(srv.hits().len(), 1);
        assert_eq!(srv.hits()[0].path, "/m/model.bin");
        assert!(srv.hits()[0].range.is_none(), "没有断点时不该发 Range");
    }

    #[test]
    fn resumes_from_part_with_range_header() {
        let root = temp_dir("resume");
        let body = vec![3u8; 300 * 1024];
        let srv = MockServer::start(body.clone(), true);
        let dest = root.join("model.bin");
        let part = part_path(&dest);
        // 预置断点：前 100 KiB 已经下好
        let done = 100 * 1024;
        std::fs::write(&part, &body[..done]).unwrap();
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);

        let (r, _seen) = run(&s, &AtomicBool::new(false));
        let out = r.expect("续传应成功");
        assert!(out.resumed, "服务器给了 206，应如实标为续传");
        assert!(!out.restarted_from_zero);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let hits = srv.hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].range.as_deref(), Some("bytes=102400-"));
    }

    #[test]
    fn restarts_from_zero_when_server_ignores_range() {
        let root = temp_dir("range-less");
        let body = vec![9u8; 250 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("model.bin");
        let part = part_path(&dest);
        // 本地有一截"旧"断点（内容不同，模拟换源）：不支持 Range 时必须整份重下，
        // 不能把旧字节当续传拼进去
        std::fs::write(&part, vec![0u8; 50 * 1024]).unwrap();
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);

        let (r, _seen) = run(&s, &AtomicBool::new(false));
        let out = r.expect("重下应成功");
        assert!(!out.resumed, "服务器回 200，不算续传");
        assert!(out.restarted_from_zero, "必须如实标出从 0 重下");
        assert_eq!(std::fs::read(&dest).unwrap(), body, "结果应是完整新文件");
        assert_eq!(srv.hits()[0].range.as_deref(), Some("bytes=51200-"));
    }

    #[test]
    fn sha256_mismatch_discards_part_and_leaves_no_dest() {
        let root = temp_dir("sha");
        let body = vec![1u8; 64 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("model.bin");
        let mut s = spec(srv.url("/m/model.bin"), dest.clone(), &body);
        s.expected_sha256 = Some("00".repeat(32));
        s.expected_size = None;

        let (r, seen) = run(&s, &AtomicBool::new(false));
        match r {
            Err(DownloadError::Sha256Mismatch { expected, got }) => {
                assert_eq!(expected, "00".repeat(32));
                assert_eq!(got.len(), 64);
            }
            other => panic!("应报 sha256 不匹配，实际 {other:?}"),
        }
        assert!(!dest.exists(), "校验失败不能留正式文件");
        assert!(!part_path(&dest).exists(), "校验失败应删掉 .part");
        assert!(seen.iter().any(|s| s.state == State::Verifying));
    }

    #[test]
    fn size_mismatch_without_sha_discards() {
        let root = temp_dir("size");
        let body = vec![2u8; 10 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("model.bin");
        let mut s = spec(srv.url("/m/model.bin"), dest.clone(), &body);
        s.expected_sha256 = None;
        s.expected_size = Some(body.len() as u64 + 1); // 清单声明的大小对不上

        let (r, _seen) = run(&s, &AtomicBool::new(false));
        match r {
            Err(DownloadError::SizeMismatch { expected, got }) => {
                assert_eq!(expected, body.len() as u64 + 1);
                assert_eq!(got, body.len() as u64);
            }
            other => panic!("应报大小不匹配，实际 {other:?}"),
        }
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn cancel_before_start_stops_without_touching_dest() {
        let root = temp_dir("cancel-pre");
        let body = vec![5u8; 128 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("model.bin");
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);

        let cancel = AtomicBool::new(true);
        let (r, _seen) = run(&s, &cancel);
        assert!(matches!(r, Err(DownloadError::Cancelled)));
        assert!(!dest.exists(), "取消后不留半成品在正式路径");
        assert!(srv.hits().is_empty(), "取消后不应再发网络请求");
    }

    #[test]
    fn cancel_midway_stops_requests_and_keeps_part_only() {
        let root = temp_dir("cancel-mid");
        // 分两段写：第一段响应后读循环会看到 cancel，第二段不会再读
        let body = vec![4u8; 512 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("model.bin");
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);

        let cancel = AtomicBool::new(false);
        let mut first = true;
        let r = download(&s, &cancel, |snap| {
            if first && snap.state == State::Downloading && snap.downloaded > 0 {
                first = false;
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(matches!(r, Err(DownloadError::Cancelled)));
        assert!(!dest.exists(), "取消后不留半成品在正式路径");
        assert!(part_path(&dest).exists(), "取消保留 .part 以便续传");
        // 只发过一次请求：取消后读循环退出，不再继续拉
        assert_eq!(srv.hits().len(), 1);
    }

    /// `cancel` 要如实区分「首次请求取消 / 已在取消中 / 其实已经收尾」，UI 才不会把
    /// 已经结束的任务说成"已取消"（复核顺手项）。
    #[test]
    fn cancel_reports_requested_then_already_requested_then_finished() {
        let root = temp_dir("cancel-outcome");
        let body = vec![3u8; 32 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(
            move |s| {
                let _ = tx.send(s);
            },
            Some(1),
        );
        let id = match dl.enqueue(
            spec(srv.url("/m/model.bin"), root.join("m.bin"), &body),
            |u| u.to_string(),
        ) {
            Enqueued::Started(id) => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(dl.cancel(id), CancelOutcome::Requested);
        assert_eq!(dl.cancel(id), CancelOutcome::AlreadyRequested);

        // 等终态：worker 先摘 flag 再发终态快照，所以看到终态时 cancel 一定报 Finished
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(s) if s.state.is_terminal() => break,
                Ok(_) => {}
                Err(_) => assert!(Instant::now() < deadline, "等不到终态快照"),
            }
        }
        assert_eq!(
            dl.cancel(id),
            CancelOutcome::Finished,
            "任务已收尾，不能再报「已取消」"
        );
    }

    /// 416 补救本身也必须真的能走通：`.part` 比文件还大（换源/文件变小）时，丢弃
    /// `.part` 从 0 重下。
    ///
    /// 这条同时钉住一个真 bug：416 以前是**死代码**——ureq 把 4xx 当 `Err(Error::Status)`
    /// 返回，`send_get` 又把它直接翻成 `DownloadError::Http`，补救分支永远走不到，用户
    /// 只会看到"服务器返回 HTTP 416"。修好 `send_get`（把响应交回上层）后这条才会绿。
    #[test]
    fn stale_oversized_part_recovers_by_restarting_from_zero() {
        let root = temp_dir("416-recover");
        let body = vec![4u8; 16 * 1024];
        let srv = MockServer::start(body.clone(), true);
        let dest = root.join("model.bin");
        std::fs::write(part_path(&dest), vec![0u8; 32 * 1024]).unwrap();
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);

        let (r, _seen) = run(&s, &AtomicBool::new(false));
        let out = r.expect("416 之后应丢弃坏断点、从 0 重下成功");
        assert!(out.restarted_from_zero, "应如实标出不是续传");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let hits = srv.hits();
        assert_eq!(hits.len(), 2, "第一次带 Range 被 416 拒，第二次不带 Range");
        assert_eq!(hits[0].range.as_deref(), Some("bytes=32768-"));
        assert!(hits[1].range.is_none());
    }

    /// B3（复核阻塞项）：416（断点不合法）之后会再补发一次 GET——这条路也必须看取消标志，
    /// 否则「取消后不再有网络请求」在 416 上不成立。
    ///
    /// mock 在回第一个响应**之前**就把取消标志置上，把时序钉成确定性的：
    /// 旧实现会在 416 之后补发第二次请求（hits=2），修好后 hits=1。
    #[test]
    fn cancel_before_416_retry_prevents_the_second_request() {
        let root = temp_dir("416-cancel");
        let body = vec![5u8; 8 * 1024];
        let cancel = Arc::new(AtomicBool::new(false));
        let srv = MockServer::start_arming(body.clone(), true, Arc::clone(&cancel));
        let dest = root.join("model.bin");
        // .part 比文件还大 → Range 不合法 → 服务器回 416
        std::fs::write(part_path(&dest), vec![0u8; 16 * 1024]).unwrap();
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);

        let r = download_with(&s, &cancel, Timeouts::default(), |_| {});
        assert!(matches!(r, Err(DownloadError::Cancelled)), "实际 {r:?}");
        assert_eq!(
            srv.hits().len(),
            1,
            "416 之后不该再发第二次请求（取消标志已置位）"
        );
        assert!(!dest.exists());
    }

    /// B2（复核阻塞项）：服务端发完响应头就卡住不写 body 时，读超时必须生效——否则
    /// 取消（以及后面排队的任务）会一直等下去。ureq 2 默认**没有任何超时**。
    ///
    /// 这条对旧实现会红：不设超时时读会一直阻塞到 mock 8 秒后关连接，`elapsed < 5s` 不成立。
    #[test]
    fn stalled_read_hits_the_timeout_instead_of_hanging() {
        let root = temp_dir("stall");
        let srv = MockServer::start_stalling(32 * 1024, Duration::from_secs(8));
        let dest = root.join("model.bin");
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &[]);
        let start = Instant::now();
        let r = download_with(
            &s,
            &AtomicBool::new(false),
            Timeouts {
                connect: Duration::from_secs(2),
                read: Duration::from_millis(300),
            },
            |_| {},
        );
        let elapsed = start.elapsed();
        assert!(
            matches!(r, Err(DownloadError::Http(_))),
            "卡住的读应报网络错误，实际 {r:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "读超时没生效：卡了 {elapsed:?}（默认无超时时会一直等到服务端关连接）"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn creates_missing_target_directory() {
        let root = temp_dir("mkdir");
        let body = vec![6u8; 8 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("nested/deeper/model.bin"); // 目录还不存在
        let s = spec(srv.url("/m/model.bin"), dest.clone(), &body);
        let (r, _seen) = run(&s, &AtomicBool::new(false));
        assert!(r.is_ok());
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    // ── 并发队列 ──────────────────────────────────────────────────────────
    //
    // 这一组用**进程内确定性同步**证明"真的并行"：mock 服务端要等**两个**请求
    // 都到齐才回响应。串行队列下第二个请求永远发不出来 → 服务端等到超时 → 该任务失败，
    // 用例红。不靠 sleep 计时（机器负载会把它变成 flaky 假绿/假红）。

    /// 一个"要等 N 个并发连接到齐才放行"的 mock 服务端。
    struct BarrierServer {
        addr: std::net::SocketAddr,
        hits: Arc<Mutex<Vec<String>>>,
        deal: usize,
        /// 等到 `deal` 个请求都到了才放行（或者等到超时——串行实现就是这条）
        gate: Arc<(Mutex<usize>, Condvar)>,
        stop: Arc<AtomicBool>,
    }

    impl BarrierServer {
        /// `wait` 是**客户端读超时之外**的兜底：正常情况下 `deal` 个请求一到齐就立刻放行，
        /// 完全不依赖这个时长。它只在"串行实现"下才生效（永远等不到第 `deal` 个），
        /// 以及极端负载下连接迟迟没被 accept 时给客户端留足时间。
        fn start(deal: usize, wait: Duration) -> BarrierServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            // 阻塞 accept：非阻塞 + sleep 轮询在高负载下会拖慢"第二个请求被 accept"，
            // 让屏障误判成"没等齐"（我压测时 720 次里踩到 2 次，都以 5.03s 超时告终）。
            listener.set_nonblocking(false).unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(Mutex::new(Vec::new()));
            let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let hits_srv = Arc::clone(&hits);
            let gate_srv = Arc::clone(&gate);
            let stop_srv = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut handles = Vec::new();
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let hits = Arc::clone(&hits_srv);
                            let gate = Arc::clone(&gate_srv);
                            handles.push(std::thread::spawn(move || {
                                // 读到请求行才记账：连接建立 ≠ 请求发出
                                let mut stream = stream;
                                let mut reader =
                                    std::io::BufReader::new(stream.try_clone().unwrap());
                                let mut request_line = String::new();
                                if reader.read_line(&mut request_line).is_err() {
                                    return;
                                }
                                let path = request_line
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("")
                                    .to_string();
                                hits.lock().unwrap().push(path);
                                // 关掉读端，让客户端看到连接仍然活着（我们只写响应）
                                drop(reader);
                                // 等齐 `deal` 个请求
                                let (lk, cv) = &*gate;
                                let mut g = lk.lock().unwrap();
                                *g += 1;
                                cv.notify_all();
                                let deadline = Instant::now() + wait;
                                while *g < deal {
                                    let left = deadline.saturating_duration_since(Instant::now());
                                    if left.is_zero() {
                                        break;
                                    }
                                    let (ng, _) = cv.wait_timeout(g, left).unwrap();
                                    g = ng;
                                }
                                let opened = *g >= deal;
                                drop(g);
                                use std::io::Write as _;
                                if !opened {
                                    // **没等齐就超时**：这次必须让客户端看到失败，否则
                                    // "串行 = 第一个请求等到超时也会拿到完整响应"，
                                    // 用例就变成恒绿（我第一版正是这么被骗过的）。
                                    // 声明的长度远大于实际写入的字节 → 客户端必然发现截断。
                                    let head = "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n";
                                    let _ = stream.write_all(head.as_bytes());
                                    let _ = stream.write_all(b"short");
                                    let _ = stream.flush();
                                    let _ = stream.shutdown(Shutdown::Both);
                                    return;
                                }
                                let body = b"payload";
                                let head = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                );
                                let _ = stream.write_all(head.as_bytes());
                                let _ = stream.write_all(body);
                                let _ = stream.flush();
                                let _ = stream.shutdown(Shutdown::Both);
                            }));
                        }
                        Err(_) => {
                            if stop_srv.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                    }
                }
                for h in handles {
                    let _ = h.join();
                }
            });
            BarrierServer {
                addr,
                hits,
                deal,
                gate,
                stop,
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        fn hits(&self) -> Vec<String> {
            self.hits.lock().unwrap().clone()
        }
    }

    impl Drop for BarrierServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // 唤醒可能还在等 gate 的 handler，别让它们拖住测试进程
            let (lk, cv) = &*self.gate;
            if let Ok(mut g) = lk.lock() {
                *g = self.deal;
                cv.notify_all();
            }
            // accept 循环现在是**阻塞** accept，上面那把锁唤不醒它——照 MockServer 的做法
            // 打一条 dummy 连接把 accept 放出来，它才会看到 stop 并退出。
            // （现在不 join 那条线程、进程退出即结束；但哪天有人改成 join，缺这一行会永久挂住。）
            let _ = TcpStream::connect(self.addr);
        }
    }

    /// 等所有 `wanted` 个任务都到终态，返回事件流。
    fn collect_until<F>(rx: &Receiver<Snapshot>, wanted: &[u64], ok: F) -> Vec<Snapshot>
    where
        F: Fn(&Snapshot) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut events: Vec<Snapshot> = Vec::new();
        while !wanted
            .iter()
            .all(|id| events.iter().any(|s| s.id == *id && ok(s)))
        {
            assert!(
                Instant::now() < deadline,
                "任务 {wanted:?} 没能在 20s 内到达预期终态；已有事件：{:?}",
                events
                    .iter()
                    .map(|s| (s.id, s.state.clone()))
                    .collect::<Vec<_>>()
            );
            if let Ok(snap) = rx.recv_timeout(Duration::from_millis(200)) {
                events.push(snap);
            }
        }
        events
    }

    /// 并发是真的：两个任务必须**同时在飞**，服务端才肯放行。
    ///
    /// 阳性对照：把 `Downloader::new(.., Some(2))` 改成 `Some(1)`（串行）→
    /// 服务端永远等不到第二个请求 → 5s 超时 → 超时的那个任务失败 → 用例红。
    #[test]
    fn two_tasks_are_in_flight_at_the_same_time() {
        let root = temp_dir("parallel");
        let body = b"payload";
        let srv = BarrierServer::start(2, Duration::from_secs(5));
        let dest_c = root.join("c.bin");
        let dest_d = root.join("d.bin");

        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(
            move |s| {
                let _ = tx.send(s);
            },
            Some(2),
        );
        let spec_c = TaskSpec {
            label: "c".into(),
            url: srv.url("/c.bin"),
            dest: dest_c.clone(),
            expected_sha256: None,
            expected_size: Some(body.len() as u64),
        };
        let spec_d = TaskSpec {
            label: "d".into(),
            url: srv.url("/d.bin"),
            dest: dest_d.clone(),
            expected_sha256: None,
            expected_size: Some(body.len() as u64),
        };
        let id_c = match dl.enqueue(spec_c, |u| u.to_string()) {
            Enqueued::Started(id) => id,
            other => panic!("第一条应排上：{other:?}"),
        };
        let id_d = match dl.enqueue(spec_d, |u| u.to_string()) {
            Enqueued::Started(id) => id,
            other => panic!("第二条应排上（不同 dest）：{other:?}"),
        };
        let events = collect_until(&rx, &[id_c, id_d], |s| s.state.is_terminal());
        for id in [id_c, id_d] {
            let last = events
                .iter()
                .rev()
                .find(|s| s.id == id && s.state.is_terminal())
                .expect("应有终态");
            assert_eq!(last.state, State::Done, "id={id} 未能完成：{last:?}");
        }
        assert_eq!(std::fs::read(&dest_c).unwrap(), body);
        assert_eq!(std::fs::read(&dest_d).unwrap(), body);
        // 两个请求都必须到过服务端，而且**必须同时在飞**：服务端是"等齐 2 个才回"，
        // 并发度 1 时第一个请求会一直等第二个 → 5s 超时 → 该任务失败 → 上面那条 Done
        // 断言红。所以这条断言不是"两条都完成了"，而是"两条同时在飞"。
        let mut hits = srv.hits();
        hits.sort();
        assert_eq!(hits, vec!["/c.bin".to_string(), "/d.bin".to_string()]);
    }

    /// 同一个目标路径入队两次：第二次**拒绝**，不产生第二个 writer。
    ///
    /// 阳性对照：去掉 `active_dests` 去重 → 第二次会 `Started`，且服务端会收到
    /// 两个请求（`.part` 被两个 append 同时写）→ 用例红。
    #[test]
    fn duplicate_destination_is_refused_and_produces_one_writer() {
        let root = temp_dir("dup-dest");
        let body = b"payload";
        let srv = BarrierServer::start(1, Duration::from_secs(3));
        let dest = root.join("same.bin");
        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(
            move |s| {
                let _ = tx.send(s);
            },
            Some(2),
        );
        let mk = || TaskSpec {
            label: "same".into(),
            url: srv.url("/same.bin"),
            dest: dest.clone(),
            expected_sha256: None,
            expected_size: Some(body.len() as u64),
        };
        let first = match dl.enqueue(mk(), |u| u.to_string()) {
            Enqueued::Started(id) => id,
            other => panic!("第一条应排上：{other:?}"),
        };
        let second = dl.enqueue(mk(), |u| u.to_string());
        assert_eq!(
            second,
            Enqueued::Duplicate { existing: first },
            "同 dest 的第二个请求必须被拒（否则两个 writer 抢同一个 .part）"
        );
        let events = collect_until(&rx, &[first], |s| s.state.is_terminal());
        let last = events
            .iter()
            .rev()
            .find(|s| s.id == first && s.state.is_terminal())
            .unwrap();
        assert_eq!(last.state, State::Done, "{last:?}");
        assert_eq!(srv.hits(), vec!["/same.bin".to_string()], "只允许一次请求");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    /// **看到终态快照 ⇒ 该任务已完全收尾**（flag 摘掉、dest 名额放开、in_flight 减掉）。
    ///
    /// 这条钉两件事，都是复核驳回时点名的：
    ///   ① 顺序不变式：`finish_job` 必须在终态 `notify` **之前**。反过来写会留一个
    ///      窗口期，`cancel(id)` 报 `AlreadyRequested` 而不是 `Finished`
    ///      —— `cancel_reports_requested_then_already_requested_then_finished` 因此在
    ///      负载下 ~0.7%~2.4% flaky（复核实测 6/250 与 2/300，我本地 1/120）。
    ///   ② `finish_job` 的**释放**那一半：整段 `active_dests.remove` 删掉后以前全仓
    ///      337 条仍全绿（没有测试隔离），这条补上。
    ///
    /// **断言必须写在 `notify` 回调里面**：只有那里继承了 `finish_job` 的
    /// happens-before（worker 先收尾再 notify）。在另一个线程里等快照到达**再**查，
    /// 窗口已经过去、恒绿——我第一版就是这么写的，被自己骗过。
    ///
    /// 探针也必须是**只读**的：第一版我在回调里调 `dl.cancel(id)` 来验证，
    /// 结果 `cancel` 自己把标志置上了，断言当然读到 `Requested`（假红）。
    /// 这里改查 `is_registered` / `has_active_dest` 两个纯读探针。
    #[test]
    fn terminal_snapshot_means_the_task_is_fully_finished() {
        let root = temp_dir("terminal-cleanup");
        let body = vec![5u8; 16 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest = root.join("m.bin");
        let (tx, rx) = channel::<Snapshot>();
        let dl_holder: Arc<Mutex<Option<Arc<Downloader>>>> = Arc::new(Mutex::new(None));
        let dl_slot = Arc::clone(&dl_holder);
        // 在 notify 里（worker 线程，finish_job 之后）抓到的只读探针结果
        let probes: Arc<Mutex<Vec<(u64, bool, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let probes_slot = Arc::clone(&probes);
        let dl = Downloader::new(
            move |snap| {
                if snap.state.is_terminal() {
                    if let Some(dl) = dl_slot.lock().unwrap().as_ref() {
                        probes_slot.lock().unwrap().push((
                            snap.id,
                            !dl.is_registered(snap.id),
                            !dl.has_active_dest(&snap.dest),
                        ));
                    }
                    let _ = tx.send(snap);
                }
            },
            Some(1),
        );
        let dl = Arc::new(dl);
        *dl_holder.lock().unwrap() = Some(Arc::clone(&dl));

        let id = match dl.enqueue(spec(srv.url("/m.bin"), dest.clone(), &body), |u| {
            u.to_string()
        }) {
            Enqueued::Started(id) => id,
            other => panic!("{other:?}"),
        };
        // 等终态快照。一条任务**只推一条**终态，而且必须是在收尾（摘 flag / 放 dest 名额）
        // **之后**才推——所以"看到终态"就等于"该任务已完全收尾"（复核抓过这条被反过来的竞态）。
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut terminal: Option<Snapshot> = None;
        while terminal.is_none() {
            assert!(Instant::now() < deadline, "等不到终态快照");
            if let Ok(snap) = rx.recv_timeout(Duration::from_millis(50)) {
                if snap.id == id && snap.state.is_terminal() {
                    terminal = Some(snap);
                }
            }
        }

        let probes = probes.lock().unwrap().clone();
        assert!(
            !probes.is_empty(),
            "notify 回调里应至少抓到一次终态（否则断言没生效）"
        );
        for (snap_id, flag_gone, dest_freed) in &probes {
            assert!(
                *flag_gone,
                "终态快照（id={snap_id}）发出时 flag 还在——说明收尾被放在了 notify 之后\
                 （复核实测的 flaky 根因：cancel 会报 AlreadyRequested）"
            );
            assert!(
                *dest_freed,
                "终态快照（id={snap_id}）发出时 dest 名额还没放开——说明 active_dests \
                 的释放在 notify 之后（同一窗口里重排同 dest 会被误判 Duplicate）"
            );
        }

        // 公开 API 上再确认一次：终态已到，现在重排同一 dest 必须能排上
        let id2 = match dl.enqueue(spec(srv.url("/m.bin"), dest.clone(), &body), |u| {
            u.to_string()
        }) {
            Enqueued::Started(id2) => id2,
            Enqueued::Duplicate { existing } => {
                panic!("终态后同一 dest 必须能重排，却被当成 Duplicate（指向已完成的 #{existing}）")
            }
        };
        assert_ne!(id2, id);
    }

    /// 去重按**完整目标路径**，不是按文件名。
    ///
    /// 反例很常见：两个模型各在 `A/model.gguf` 与 `B/model.gguf`，文件名相同但落点不同，
    /// 它们**必须能同时下**（否则第二个被拒，用户以为在装两个模型其实只装了一个）。
    /// 把去重键改成 `file_name()` 就会红。
    #[test]
    fn dedup_is_by_full_dest_not_by_basename() {
        let root = temp_dir("dedup-path");
        let body = b"payload";
        let srv = BarrierServer::start(2, Duration::from_secs(5));
        let dest_a = root.join("A/model.gguf");
        let dest_b = root.join("B/model.gguf");
        assert_eq!(
            dest_a.file_name(),
            dest_b.file_name(),
            "构造前提：两个落点文件名相同"
        );
        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(
            move |s| {
                let _ = tx.send(s);
            },
            Some(2),
        );
        let mk = |url: String, dest: PathBuf| TaskSpec {
            label: dest.display().to_string(),
            url,
            dest,
            expected_sha256: None,
            expected_size: Some(body.len() as u64),
        };
        let id_a = match dl.enqueue(mk(srv.url("/A/model.gguf"), dest_a.clone()), |u| {
            u.to_string()
        }) {
            Enqueued::Started(id) => id,
            other => panic!("同名的第一条应排上：{other:?}"),
        };
        let id_b = match dl.enqueue(mk(srv.url("/B/model.gguf"), dest_b.clone()), |u| {
            u.to_string()
        }) {
            Enqueued::Started(id) => id,
            other => panic!("**文件名相同但落点不同**的第二条也必须能排上：{other:?}"),
        };
        let events = collect_until(&rx, &[id_a, id_b], |s| s.state.is_terminal());
        for id in [id_a, id_b] {
            let last = events
                .iter()
                .rev()
                .find(|s| s.id == id && s.state.is_terminal())
                .expect("应有终态");
            assert_eq!(last.state, State::Done, "id={id} 未能完成：{last:?}");
        }
        assert_eq!(std::fs::read(&dest_a).unwrap(), body);
        assert_eq!(std::fs::read(&dest_b).unwrap(), body);
        assert_eq!(srv.hits().len(), 2, "两个不同落点都该真的发过请求");
    }

    /// 一个任务失败（校验不过）不影响另一个照常完成。
    #[test]
    fn one_failure_does_not_stop_the_other() {
        let root = temp_dir("one-fail");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_srv = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                // 读到头部结束
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                hits_srv.lock().unwrap().push(path.clone());
                let body: &[u8] = if path == "/bad.bin" {
                    b"WRONG"
                } else {
                    b"payload"
                };
                use std::io::Write as _;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
        let dest_ok = root.join("ok.bin");
        let dest_bad = root.join("bad.bin");
        let body = b"payload";
        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(
            move |s| {
                let _ = tx.send(s);
            },
            Some(2),
        );
        let ok = match dl.enqueue(
            TaskSpec {
                label: "ok".into(),
                url: format!("http://{addr}/ok.bin"),
                dest: dest_ok.clone(),
                expected_sha256: None,
                expected_size: Some(body.len() as u64),
            },
            |u| u.to_string(),
        ) {
            Enqueued::Started(id) => id,
            other => panic!("{other:?}"),
        };
        // bad.bin 声明了 999 字节但服务端只给 5 字节 → 必然失败
        let bad = match dl.enqueue(
            TaskSpec {
                label: "bad".into(),
                url: format!("http://{addr}/bad.bin"),
                dest: dest_bad.clone(),
                expected_sha256: None,
                expected_size: Some(999),
            },
            |u| u.to_string(),
        ) {
            Enqueued::Started(id) => id,
            other => panic!("{other:?}"),
        };
        let events = collect_until(&rx, &[ok, bad], |s| s.state.is_terminal());
        let finish = |id: u64| {
            events
                .iter()
                .rev()
                .find(|s| s.id == id && s.state.is_terminal())
                .cloned()
                .unwrap()
        };
        assert_eq!(finish(ok).state, State::Done, "好的那条应照常完成");
        assert!(
            matches!(finish(bad).state, State::Failed(_)),
            "坏的那条应失败而不是静默成功：{:?}",
            finish(bad)
        );
        assert_eq!(std::fs::read(&dest_ok).unwrap(), body);
        assert!(!dest_bad.exists(), "失败的正式路径不该出现");
    }

    /// 入队时应用镜像：队列真的把 URL 改写了（不是只在界面上写一行）。
    #[test]
    fn enqueue_applies_the_mirror_to_the_requested_url() {
        let root = temp_dir("mirror");
        let body = b"payload";
        let srv = MockServer::start(body.to_vec(), false);
        let dest = root.join("m.bin");
        // 清单里的原始 URL 是官方域名；镜像前缀把它指到 mock 上
        let official = "https://huggingface.co/org/repo/resolve/main/m.bin";
        let mirror_base = format!("http://{}", srv.addr);
        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(
            move |s| {
                let _ = tx.send(s);
            },
            Some(1),
        );
        let id = match dl.enqueue(
            TaskSpec {
                label: "m".into(),
                url: official.to_string(),
                dest,
                expected_sha256: None,
                expected_size: Some(body.len() as u64),
            },
            |url| crate::download_mirror::rewrite_url(url, &mirror_base),
        ) {
            Enqueued::Started(id) => id,
            other => panic!("{other:?}"),
        };
        let events = collect_until(&rx, &[id], |s| s.state.is_terminal());
        assert_eq!(
            events.iter().rev().find(|s| s.id == id).unwrap().state,
            State::Done
        );
        assert_eq!(
            srv.hits()[0].path,
            "/org/repo/resolve/main/m.bin",
            "请求路径必须来自被改写后的镜像 URL"
        );
    }

    /// 取消排队中的任务：它不该占用一个 writer 名额，也不该落地。
    #[test]
    fn cancelling_queued_task_skips_it() {
        let root = temp_dir("queue-cancel");
        let body = vec![8u8; 256 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest_a = root.join("a.bin");
        let dest_b = root.join("b.bin");
        let (tx, rx) = channel::<Snapshot>();
        // 1 并发：第二条一定还在排队
        let dl = Downloader::new(
            move |s| {
                let _ = tx.send(s);
            },
            Some(1),
        );
        let id_a = match dl.enqueue(spec(srv.url("/a.bin"), dest_a.clone(), &body), |u| {
            u.to_string()
        }) {
            Enqueued::Started(id) => id,
            other => panic!("{other:?}"),
        };
        let id_b = match dl.enqueue(spec(srv.url("/b.bin"), dest_b.clone(), &body), |u| {
            u.to_string()
        }) {
            Enqueued::Started(id) => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(dl.cancel(id_b), CancelOutcome::Requested);

        let events = collect_until(&rx, &[id_a, id_b], |s| s.state.is_terminal());
        let finish = |id: u64| {
            events
                .iter()
                .rev()
                .find(|s| s.id == id && s.state.is_terminal())
                .cloned()
                .unwrap()
        };
        assert_eq!(
            finish(id_b).state,
            State::Cancelled,
            "被取消的排队任务应标已取消"
        );
        assert_eq!(finish(id_a).state, State::Done, "另一条应照常完成");
        assert!(!dest_b.exists());
        assert_eq!(std::fs::read(&dest_a).unwrap(), body);
    }

    /// 并发数归一化：缺省 / 0 / 越界都夹到 1..=4。
    #[test]
    fn concurrency_is_clamped_to_a_sane_range() {
        assert_eq!(effective_concurrency(None), 2);
        assert_eq!(effective_concurrency(Some(0)), 1);
        assert_eq!(effective_concurrency(Some(1)), 1);
        assert_eq!(effective_concurrency(Some(3)), 3);
        assert_eq!(effective_concurrency(Some(4)), 4);
        assert_eq!(effective_concurrency(Some(9)), 4);
    }
}
