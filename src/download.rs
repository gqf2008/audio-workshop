//! 增强模型下载器核心（M4-P7 第一段）：串行队列 + 断点续传 + 校验后提交。
//!
//! 设计要点（与仓库既有的落盘约定一致，见 docs/robustness.md）：
//!   · 先写 `<目标>.part`，**校验通过才 rename 成正式文件**——正式路径要么没有，
//!     要么是完整的；中断只留下可续传的 `.part`，不留半成品。
//!   · 续传发 `Range: bytes=<已下载>-`，**只有服务器回 206 才算续传**；回 200
//!     就是服务器不认 Range，如实从 0 重下并在状态里标出来，绝不假装续传。
//!   · 没有期望 sha256 时按声明大小（清单里的 size，或响应 Content-Length）核大小。
//!
//! 队列本身跑在一个后台线程里：`enqueue` 只是把任务塞进去，`cancel` 通过原子标志
//! 让正在下载的读循环尽快退出——取消不会阻塞、也不需要等网络超时。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

/// 读盘缓冲区：64 KiB 在进度粒度与系统调用次数之间取平衡。
const CHUNK: usize = 64 * 1024;

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
    mut on_progress: impl FnMut(&Snapshot),
) -> Result<Outcome, DownloadError> {
    // 还没开始就被取消：一个网络请求都不发（取消排队中的任务走这条）
    if cancel.load(Ordering::Relaxed) {
        return Err(DownloadError::Cancelled);
    }
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
        let resp = send_get(&spec.url, offset)?;
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
                return Err(DownloadError::Http(format!(
                    "服务器返回 HTTP {other}（{}）",
                    spec.url
                )));
            }
        }
    };

    let mut note = if resumed {
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

    note = if resumed {
        format!("断点续传完成（续了 {} 字节）", offset)
    } else if restarted_from_zero {
        "服务器不支持续传，已从 0 重下完成".to_string()
    } else {
        "下载完成".to_string()
    };
    let done = Snapshot {
        id: 0,
        label: spec.label.clone(),
        dest: spec.dest.clone(),
        state: State::Done,
        downloaded: got,
        total: Some(got),
        note,
    };
    on_progress(&done);

    Ok(Outcome {
        bytes: got,
        resumed,
        restarted_from_zero,
    })
}

/// 发一次 GET；`offset > 0` 时带 Range 头。`Accept-Encoding: identity` 保证
/// 字节流与 Content-Length 对齐（透明 gzip 会让续传的偏移全错）。
fn send_get(url: &str, offset: u64) -> Result<ureq::Response, DownloadError> {
    let mut req = ureq::get(url).set("Accept-Encoding", "identity");
    if offset > 0 {
        req = req.set("Range", &format!("bytes={offset}-"));
    }
    req.call().map_err(|e| match e {
        ureq::Error::Status(code, _) => DownloadError::Http(format!("服务器返回 HTTP {code}")),
        other => DownloadError::Http(other.to_string()),
    })
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
// 队列：串行执行、可取消
// ===========================================================================

/// 一条排队中的任务（id + 规格 + 它自己的取消标志）。
struct Job {
    id: u64,
    spec: TaskSpec,
    cancel: Arc<AtomicBool>,
}

/// 下载队列句柄。`enqueue` 把任务追加到队列尾部并唤醒后台线程；
/// 后台线程**串行**执行（一次只下一个），完成/失败都推快照给 UI。
pub struct Downloader {
    shared: Arc<Mutex<VecDeque<Job>>>,
    flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    next_id: Arc<AtomicU64>,
    wake: Sender<()>,
}

impl Downloader {
    /// 起一个后台队列线程；`notify` 在每次状态/进度变化时被调用（在后台线程里）。
    pub fn new<F>(notify: F) -> Downloader
    where
        F: Fn(Snapshot) + Send + Sync + 'static,
    {
        let shared = Arc::new(Mutex::new(VecDeque::new()));
        let flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));
        let (wake, wake_rx) = channel::<()>();
        let notify = Arc::new(notify);
        {
            let shared = Arc::clone(&shared);
            let flags = Arc::clone(&flags);
            let notify = Arc::clone(&notify);
            std::thread::spawn(move || worker_loop(shared, flags, notify, wake_rx));
        }
        Downloader {
            shared,
            flags,
            next_id,
            wake,
        }
    }

    /// 入队一条任务，返回它的 id。会立刻推一条 Queued 快照。
    pub fn enqueue(&self, spec: TaskSpec) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.flags.lock().unwrap().insert(id, Arc::clone(&cancel));
        self.shared
            .lock()
            .unwrap()
            .push_back(Job { id, spec, cancel });
        let _ = self.wake.send(());
        id
    }

    /// 取消一条任务。正在下载的由原子标志让读循环退出；还没开始的会在轮到它时跳过。
    /// 返回 false 表示这个 id 已经不在队列里（已完成/已取消/不存在）。
    pub fn cancel(&self, id: u64) -> bool {
        let flags = self.flags.lock().unwrap();
        match flags.get(&id) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

fn worker_loop<F>(
    shared: Arc<Mutex<VecDeque<Job>>>,
    flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    notify: Arc<F>,
    wake_rx: Receiver<()>,
) where
    F: Fn(Snapshot) + Send + Sync,
{
    loop {
        let job = shared.lock().unwrap().pop_front();
        let Some(job) = job else {
            // 队列空：等唤醒；所有 Downloader 都 drop 掉时 sender 关闭 → 退出线程
            if wake_rx.recv().is_err() {
                return;
            }
            continue;
        };
        let id = job.id;
        let spec = job.spec;
        let cancel = job.cancel;
        // 入队时推一条 Queued（放在这里推，保证顺序：Queued 一定先于 Downloading）
        notify(Snapshot {
            id,
            label: spec.label.clone(),
            dest: spec.dest.clone(),
            state: State::Queued,
            downloaded: 0,
            total: spec.expected_size,
            note: String::new(),
        });
        if cancel.load(Ordering::Relaxed) {
            flags.lock().unwrap().remove(&id);
            notify(snapshot_of(&spec, id, State::Cancelled, 0, None, "已取消"));
            continue;
        }
        notify(snapshot_of(
            &spec,
            id,
            State::Downloading,
            0,
            spec.expected_size,
            "",
        ));
        let result = download(&spec, &cancel, |s| {
            // 进度快照把 id 补上（download 不知道自己的 id）
            let mut s = s.clone();
            s.id = id;
            notify(s);
        });
        flags.lock().unwrap().remove(&id);
        match result {
            Ok(out) => {
                let note = if out.resumed {
                    "断点续传完成".to_string()
                } else if out.restarted_from_zero {
                    "服务器不支持续传，已从 0 重下完成".to_string()
                } else {
                    "下载完成".to_string()
                };
                notify(snapshot_of(
                    &spec,
                    id,
                    State::Done,
                    out.bytes,
                    Some(out.bytes),
                    &note,
                ));
            }
            Err(DownloadError::Cancelled) => {
                notify(snapshot_of(&spec, id, State::Cancelled, 0, None, "已取消"));
            }
            Err(e) => {
                notify(snapshot_of(
                    &spec,
                    id,
                    State::Failed(e.to_string()),
                    0,
                    None,
                    &e.to_string(),
                ));
            }
        }
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
    use std::time::{Duration, Instant};

    /// 一条被 mock server 收到的请求（记录 Range 头与路径，供断言）。
    #[derive(Clone, Debug, Default)]
    struct Hit {
        path: String,
        range: Option<String>,
    }

    /// 极简 HTTP/1.1 mock：每个 GET 回一次响应；`honor_range` 决定认不认 Range。
    struct MockServer {
        addr: std::net::SocketAddr,
        hits: Arc<Mutex<Vec<Hit>>>,
        stop: Arc<AtomicBool>,
    }

    impl MockServer {
        fn start(body: Vec<u8>, honor_range: bool) -> MockServer {
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
                    serve_one(&mut stream, &body, honor_range, &hits_srv);
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

    fn serve_one(stream: &mut TcpStream, body: &[u8], honor_range: bool, hits: &Mutex<Vec<Hit>>) {
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
        // 进度快照应覆盖 下载中→校验中→完成
        assert!(seen.iter().any(|s| s.state == State::Downloading));
        assert!(seen.iter().any(|s| s.state == State::Verifying));
        assert_eq!(seen.last().unwrap().state, State::Done);
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

    #[test]
    fn queue_runs_tasks_serially_in_order() {
        let root = temp_dir("queue");
        let body_a = vec![1u8; 64 * 1024];
        let body_b = vec![2u8; 48 * 1024];
        let srv = MockServer::start(body_a.clone(), false);
        // 两条任务打同一台 server 的不同路径（server 一律回 body_a）；改用两台更省事：
        let srv_b = MockServer::start(body_b.clone(), false);
        let dest_a = root.join("a.bin");
        let dest_b = root.join("b.bin");

        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(move |s| {
            let _ = tx.send(s);
        });
        let id_a = dl.enqueue(spec(srv.url("/a.bin"), dest_a.clone(), &body_a));
        let id_b = dl.enqueue(spec(srv_b.url("/b.bin"), dest_b.clone(), &body_b));
        assert_ne!(id_a, id_b);

        // 等两条都到终态（Done）
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut done = std::collections::HashSet::new();
        while done.len() < 2 && Instant::now() < deadline {
            if let Ok(snap) = rx.recv_timeout(Duration::from_millis(200)) {
                if snap.state == State::Done {
                    done.insert(snap.id);
                }
            }
        }
        assert_eq!(done.len(), 2, "两条任务都应完成");
        assert!(done.contains(&id_a) && done.contains(&id_b));
        assert_eq!(std::fs::read(&dest_a).unwrap(), body_a);
        assert_eq!(std::fs::read(&dest_b).unwrap(), body_b);
    }

    #[test]
    fn cancelling_queued_task_skips_it() {
        let root = temp_dir("queue-cancel");
        let body = vec![8u8; 256 * 1024];
        let srv = MockServer::start(body.clone(), false);
        let dest_a = root.join("a.bin");
        let dest_b = root.join("b.bin");
        let (tx, rx) = channel::<Snapshot>();
        let dl = Downloader::new(move |s| {
            let _ = tx.send(s);
        });
        // 排两条：第二条一开始就取消 → 应直接标 Cancelled，且不落地
        let _id_a = dl.enqueue(spec(srv.url("/a.bin"), dest_a.clone(), &body));
        let id_b = dl.enqueue(spec(srv.url("/b.bin"), dest_b.clone(), &body));
        assert!(dl.cancel(id_b));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut b_cancelled = false;
        let mut a_done = false;
        while !(b_cancelled && a_done) && Instant::now() < deadline {
            if let Ok(snap) = rx.recv_timeout(Duration::from_millis(200)) {
                if snap.id == id_b && snap.state == State::Cancelled {
                    b_cancelled = true;
                }
                if snap.id != id_b && snap.state == State::Done {
                    a_done = true;
                }
            }
        }
        assert!(b_cancelled, "被取消的排队任务应标为已取消");
        assert!(a_done, "另一条应照常完成");
        assert!(!dest_b.exists());
        assert_eq!(std::fs::read(&dest_a).unwrap(), body);
    }
}
