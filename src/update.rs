//! 检查更新（M4-P8 的另一半）：v1 只做「知道」与「拿到」——比对版本、给出发布页链接。
//!
//! **不做**（见 docs/update.md 的边界）：不自动下载安装包、不静默安装、不重启应用、
//! 不做增量更新、不做渠道/灰度。所以这里没有任何写磁盘/替换文件的代码路径，
//! 唯一的网络动作是**只读**地把一份发布清单拉下来。
//!
//! 清单有两种被接受的形状：
//!
//! 1. **GitHub releases/latest API**（默认地址就是它）：
//!    `{ "tag_name": "v0.2.0", "body": "…", "html_url": "…", "assets": [ … ] }`
//! 2. **自建/内网镜像清单**（给局域网与镜像留的口子，见设置里的「清单地址」）：
//!    `{ "version": "0.2.0", "notes": "…", "url": "https://…", "sha256": "…", "size": 123 }`
//!
//! 两种都映射到同一个 [`Release`]。**必填字段只有 `version` 与 `url`**——它们是承重的
//! （前者决定"是不是新版"，后者是"打开发布页"唯一能点的东西）；`notes`/`sha256`/`size`
//! 只是展示与将来的校验信息，缺了不影响判断，所以缺省而不是报错。缺**承重**字段、
//! 或者字段类型不对，一律给出**可执行**的错误，绝不静默当作「已是最新」。
//!
//! 纯逻辑 + 一次 HTTP GET；UI 接线在 `src/main.rs`。

use std::time::Duration;

/// 默认清单地址：GitHub 最新 Release API（与 remote `github.com/gqf2008/audio-workshop` 对应）。
pub const DEFAULT_MANIFEST_URL: &str =
    "https://api.github.com/repos/gqf2008/audio-workshop/releases/latest";

/// 连接超时（DNS + TCP + TLS 握手）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// **单次读**超时（socket 级，等价 SO_RCVTIMEO）：只要数据还在持续到达就不会中断。
///
/// 刻意**不设 ureq 的总超时**（`.timeout()`）：总超时把 DNS+连接+读完 body 全算进去，
/// 慢网络/代理下会把一次本来能成功的正常响应整条判失败
/// （见 `LESSON_自动更新大文件下载须per-read超时且应用替换禁止cp_R嵌套.md`）。
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 当前版本：编译期从 `Cargo.toml` 取，**单一来源**，UI 与比对都用它。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 一份发布清单（两种输入形状归一后的结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// 发布说明（展示用；缺省 = 空串）
    pub notes: String,
    /// 发布页地址（「打开发布页」用的就是它）
    pub url: String,
    /// 安装包的 sha256（清单自带时才有；v1 不下载，只如实带出来）
    pub sha256: Option<String>,
    /// 安装包字节数（清单自带时才有；状态行用它显示体积）
    pub size: Option<u64>,
}

impl Release {
    /// 状态行那一句（只在**有新版**时调用）。
    pub fn summary(&self) -> String {
        let size = match self.size {
            Some(n) => format!("，安装包 {}", crate::backup::human_bytes(n)),
            None => String::new(),
        };
        let digest = if self.sha256.is_some() {
            "；清单已附 sha256，下一版做下载安装时用它校验"
        } else {
            ""
        };
        format!(
            "有新版本 {}{size}——点「打开发布页」看更新说明{digest}",
            self.version
        )
    }
}

/// 检查结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    /// 当前已是最新（或比清单更新，例如本地跑自编译版本）
    UpToDate,
    Newer(Release),
}

/// 「检查结果 → 界面该怎么显示」**唯一一份**决定：返回（状态行那句话，要不要留下发布页）。
///
/// 抽成纯函数是为了能被单测钉住：三种结果各自显示什么、哪种结果才留下可点的发布页。
/// 尤其 `UpToDate`/`Err` **必须交出 `None`**——tick 里就是拿它直接覆盖
/// `state.update_release` 的；要是"已是最新"还留着上一份发布页，
/// 按钮就亮着而点开是旧版本（假可点比不显示更糟）。
pub fn outcome_view(
    current: &str,
    result: Result<UpdateCheck, String>,
) -> (String, Option<Release>) {
    match result {
        Ok(UpdateCheck::Newer(release)) => {
            let info = release.summary();
            (info, Some(release))
        }
        Ok(UpdateCheck::UpToDate) => (
            format!("已是最新版本 {current}（清单里的版本不比它新）"),
            None,
        ),
        Err(error) => (format!("检查更新失败：{error}"), None),
    }
}

/// 解析一份发布清单。接受 GitHub Release API 与自建清单两种形状（见模块注释）。
pub fn parse_release(json: &str) -> Result<Release, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("发布清单不是合法 JSON：{e}（确认清单地址没被登录页/网关拦掉）"))?;
    let obj = value.as_object().ok_or_else(|| {
        "发布清单的顶层应该是 JSON 对象（形如 {\"version\":…,\"url\":…}），实际不是".to_string()
    })?;

    // GitHub 用 tag_name/body/html_url，自建清单用 version/notes/url。先认形状，
    // 否则 GitHub 的响应会被报成"缺 version"，用户拿到的是一句没法照做的错。
    let github = obj.contains_key("tag_name");
    let (ver_key, url_key, notes_key) = if github {
        ("tag_name", "html_url", "body")
    } else {
        ("version", "url", "notes")
    };

    let version = string_field(obj, ver_key)?;
    if version.trim().is_empty() {
        return Err(format!(
            "发布清单的 `{ver_key}` 是空字符串：没法判断版本新旧"
        ));
    }
    let url = string_field(obj, url_key)?;
    require_http_url(&url)?;
    let notes = optional_string_field(obj, notes_key)?.unwrap_or_default();

    // 安装包信息：GitHub 放在 assets[0]（digest 形如 "sha256:…"），自建清单是顶层字段。
    let (sha256, size) = if github {
        first_asset_info(obj)
    } else {
        (
            optional_string_field(obj, "sha256")?
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            optional_u64_field(obj, "size")?,
        )
    };

    Ok(Release {
        version: version.trim().to_string(),
        notes,
        url,
        sha256,
        size,
    })
}

/// 取一个必填字符串字段；缺了/不是字符串都给出**指名道姓**的错误。
fn string_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, String> {
    match obj.get(key) {
        None => Err(format!(
            "发布清单缺少字段 `{key}`（顶层字段：{}）——这个地址可能不是发布清单",
            keys_of(obj)
        )),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!(
            "发布清单字段 `{key}` 该是字符串，实际是 {}",
            kind_of(other)
        )),
    }
}

/// 取一个可选字符串字段：缺 / `null` → `None`；有值但不是字符串 → 报错（不静默吞掉类型错误）。
fn optional_string_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!(
            "发布清单字段 `{key}` 该是字符串，实际是 {}",
            kind_of(other)
        )),
    }
}

/// 取一个可选无符号整数字段（`size`）：缺 / `null` → `None`；其余类型 → 报错。
fn optional_u64_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<u64>, String> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            format!(
                "发布清单字段 `{key}` 该是非负整数（字节数），实际是 {}",
                kind_of(v)
            )
        }),
    }
}

/// GitHub Release 的 `assets[0]`：返回（sha256, size）。资产缺 `digest`（GitHub 只在较新
/// 的 Release 上给）时 sha256 为 `None`，不为它编造值。
fn first_asset_info(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> (Option<String>, Option<u64>) {
    let Some(asset) = obj
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
    else {
        return (None, None);
    };
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (sha256, asset.get("size").and_then(|s| s.as_u64()))
}

/// 地址是不是 http(s)。**唯一一处判据**——清单校验、拉取、以及 UI 的
/// 「打开发布页」都走它；各写一份前缀判断，迟早会漂移成"能显示不能开"。
pub fn is_http_url(url: &str) -> bool {
    let t = url.trim();
    t.starts_with("https://") || t.starts_with("http://")
}

/// 发布页只允许 http(s)：清单是**外部**输入，不能让一个 `file://` / 自定义 scheme
/// 顺着「打开发布页」交给系统打开器（`open` 会把它们当本地路径/协议处理）。
fn require_http_url(url: &str) -> Result<(), String> {
    if is_http_url(url) {
        return Ok(());
    }
    let t = url.trim();
    Err(format!(
        "发布清单里的发布页地址不是 http(s)：{t}——清单坏了或被人改过，别点它"
    ))
}

fn keys_of(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    if obj.is_empty() {
        return "（空对象）".to_string();
    }
    obj.keys().cloned().collect::<Vec<_>>().join(", ")
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "布尔值",
        serde_json::Value::Number(_) => "数字",
        serde_json::Value::String(_) => "字符串",
        serde_json::Value::Array(_) => "数组",
        serde_json::Value::Object(_) => "对象",
    }
}

/// 版本号切成「数字段 + 预发布标签」。`1.2.10 > 1.2.9` 靠的是数字段比较，不是字典序。
#[derive(Clone, Debug)]
struct Version {
    parts: Vec<u64>,
    /// `-` 之后的部分（`1.2.0-beta.2` 的 `beta.2`）。`+` 之后是构建元数据，比对时忽略。
    pre: Option<String>,
}

/// 解析版本串：允许 `v` 前缀（`v1.2.3`），允许 `-预发布` / `+构建` 后缀，其余一律报错。
fn parse_version(raw: &str) -> Result<Version, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("版本号是空的：没法比较新旧".to_string());
    }
    // 只吃一个前导 v/V（`v1.2.3`）；`v` 后面不是数字的话下一段会报错，不必特判。
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let s = s.split('+').next().unwrap_or(s);
    let (core, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, Some(pre.trim().to_string())),
        None => (s, None),
    };
    if core.is_empty() {
        return Err(format!("版本号 `{raw}` 里没有数字段"));
    }
    let mut parts = Vec::new();
    for seg in core.split('.') {
        if seg.is_empty() {
            return Err(format!(
                "版本号 `{raw}` 有点号包着空段（`..` / 结尾的点）——期望形如 1.2.10"
            ));
        }
        let n = seg.parse::<u64>().map_err(|_| {
            format!("版本号 `{raw}` 里的 `{seg}` 不是数字——期望形如 1.2.10（可带 v 前缀）")
        })?;
        parts.push(n);
    }
    Ok(Version { parts, pre })
}

/// 清单里的版本是不是比当前版本新。
///
/// - 按**数字段**比：`1.2.10 > 1.2.9`（字典序会得出相反的结论）；
/// - 段数不同时短的一边按 0 补：`1.2 == 1.2.0`；
/// - 数字段相同时，正式版 > 预发布版（`1.2.0 > 1.2.0-beta`）；
/// - 非法版本串（本机或清单任一侧）→ `Err`，**不**当作"不是新版"。
pub fn is_newer(current: &str, candidate: &str) -> Result<bool, String> {
    let cur = parse_version(current).map_err(|e| format!("当前版本 {e}"))?;
    let new = parse_version(candidate).map_err(|e| format!("清单里的{e}"))?;
    let n = cur.parts.len().max(new.parts.len());
    for i in 0..n {
        let a = cur.parts.get(i).copied().unwrap_or(0);
        let b = new.parts.get(i).copied().unwrap_or(0);
        if a != b {
            return Ok(b > a);
        }
    }
    Ok(match (&cur.pre, &new.pre) {
        (None, None) => false,
        // 同样的数字段，正式版比预发布版新
        (Some(_), None) => true,
        (None, Some(_)) => false,
        // 两边都是预发布：按点分段比（数字段按数字、其余按字典序）。
        // 注意方向：问的是"清单比当前新"，所以拿 (清单, 当前) 比。
        (Some(cur_pre), Some(new_pre)) => {
            compare_pre(new_pre, cur_pre) == std::cmp::Ordering::Greater
        }
    })
}

fn compare_pre(a: &str, b: &str) -> std::cmp::Ordering {
    fn seg(s: &str) -> (Option<u64>, &str) {
        match s.parse::<u64>() {
            Ok(n) => (Some(n), ""),
            Err(_) => (None, s),
        }
    }
    let (mut ia, mut ib) = (a.split('.'), b.split('.'));
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => {
                let (nx, sx) = seg(x);
                let (ny, sy) = seg(y);
                let ord = match (nx, ny) {
                    (Some(m), Some(n)) => m.cmp(&n),
                    // 数字段比非数字段小（semver：数字标识符优先级低于字母）
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => sx.cmp(sy),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// 超时（可注入，便于用**短超时**测"卡住的服务器"；生产走 [`CONNECT_TIMEOUT`]/[`READ_TIMEOUT`]）。
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub connect: Duration,
    pub read: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            read: READ_TIMEOUT,
        }
    }
}

/// 拉清单 → 解析 → 比对。网络失败/清单坏了都返回**可执行**的原因（调用方原样展示）。
pub fn check(url: &str, current: &str) -> Result<UpdateCheck, String> {
    check_with(url, current, Timeouts::default())
}

/// 同 [`check`]，但超时可注入（测试用短超时；生产别调它）。
pub fn check_with(url: &str, current: &str, timeouts: Timeouts) -> Result<UpdateCheck, String> {
    let body = fetch(url, timeouts)?;
    let release = parse_release(&body)?;
    if is_newer(current, &release.version).map_err(|e| format!("没法比对版本：{e}"))? {
        Ok(UpdateCheck::Newer(release))
    } else {
        Ok(UpdateCheck::UpToDate)
    }
}

/// 只读拉一份清单正文。**只设 connect / per-read 超时，不设总超时**（见模块注释）。
fn fetch(url: &str, timeouts: Timeouts) -> Result<String, String> {
    let trimmed = url.trim();
    if !is_http_url(trimmed) {
        return Err(format!(
            "清单地址要写成完整的 http(s) 地址，现在是「{trimmed}」"
        ));
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeouts.connect)
        .timeout_read(timeouts.read)
        .build();
    let resp = agent
        .get(trimmed)
        // GitHub API 不带 User-Agent 会 403；Accept 跟官方文档一致，避免拿到 HTML 版页面
        .set(
            "User-Agent",
            concat!("audio-workshop/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| describe_error(trimmed, e))?;
    resp.into_string()
        .map_err(|e| format!("读发布清单失败（{e}）——清单多半太大或连接被中途掐断"))
}

/// 把 ureq 的错误分成「连不上 / 超时 / HTTP 状态码 / 响应读不出」，每条都给下一步。
fn describe_error(url: &str, err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let detail = snippet(&body);
            let hint = match code {
                404 => {
                    // GitHub 对**私有**仓库也回 404（不是 403）——只写"地址不对或没有 Release"会
                    // 把"仓库不可匿名读"误导成"你填错了"。本项目的仓库就是私有的，这条不是假设。
                    "——地址不对、仓库不是匿名可读（GitHub 对私有仓库也回 404）、或还没有 Release；                     确认「清单地址」指向一个匿名可读的已发布 Release"
                }
                403 | 429 => "——多半被限流：稍后再试，或在「清单地址」里换成内网/镜像清单",
                _ => "——服务器拒绝了这次请求",
            };
            if detail.is_empty() {
                format!("服务器返回 HTTP {code}{hint}")
            } else {
                format!("服务器返回 HTTP {code}{hint}：{detail}")
            }
        }
        ureq::Error::Transport(t) => {
            let msg = snippet(t.message().unwrap_or(""));
            match t.kind() {
                ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => {
                    format!("清单地址不合法：{url}——要 http:// 或 https:// 开头的完整地址")
                }
                ureq::ErrorKind::Dns => format!(
                    "连不上：域名解析失败（{msg}）——检查网络/DNS，或把「清单地址」换成内网镜像"
                ),
                ureq::ErrorKind::ConnectionFailed => {
                    format!("连不上：{msg}——检查网络/代理，或把「清单地址」换成内网镜像")
                }
                ureq::ErrorKind::InvalidProxyUrl
                | ureq::ErrorKind::ProxyConnect
                | ureq::ErrorKind::ProxyUnauthorized => {
                    format!("代理连不上：{msg}——检查系统代理设置")
                }
                ureq::ErrorKind::TooManyRedirects => {
                    format!("清单地址重定向太多次：{url}——换成最终地址")
                }
                ureq::ErrorKind::BadStatus | ureq::ErrorKind::BadHeader => {
                    format!("服务器返回了看不懂的响应：{msg}——地址可能不是发布清单")
                }
                ureq::ErrorKind::Io if is_timeout(&t) => format!(
                    "超时：{} 秒内没读到数据（{url}）——网络慢或被墙，可换内网镜像清单",
                    timeouts_hint()
                ),
                ureq::ErrorKind::Io => format!("网络读取失败：{msg}——连接被中断，稍后重试"),
                _ => format!("检查更新失败：{msg}"),
            }
        }
    }
}

/// `ErrorKind::Io` 里区分"卡住了"和"被掐断"：只有底层 `io::Error` 是超时才算超时。
fn is_timeout(t: &ureq::Transport) -> bool {
    use std::error::Error as _;
    let Some(src) = t.source() else { return false };
    match src.downcast_ref::<std::io::Error>() {
        Some(io) => matches!(
            io.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ),
        None => false,
    }
}

fn timeouts_hint() -> u64 {
    // 与 READ_TIMEOUT 同源，别在文案里写死数字（改超时会漂）
    READ_TIMEOUT.as_secs()
}

/// 响应体/错误原因截断成一行：状态行只放得下一行，别把一整页 HTML 塞进去。
fn snippet(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 200 {
        return collapsed;
    }
    let head: String = collapsed.chars().take(200).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 进程内 mock HTTP：只回一条固定响应。**不用真实外网**（测试必须离线可跑）。
    struct Mock {
        base: String,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Mock {
        /// `response` 是完整的响应（含状态行/头/体）。
        fn start(response: &'static str) -> Mock {
            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
            let base = format!("http://{}", listener.local_addr().unwrap());
            let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let hits_t = hits.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    hits_t.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = drain_request(&mut stream);
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Mock { base, hits }
        }

        /// 接了连接但**永远不给响应**：用来验证 per-read 超时真的会到（不是无限等）。
        fn start_stalling() -> Mock {
            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
            let base = format!("http://{}", listener.local_addr().unwrap());
            let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let hits_t = hits.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    hits_t.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = drain_request(&mut stream);
                    // 保持连接但一个字节都不回；拿不到响应的客户端只能靠超时退出
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
            });
            Mock { base, hits }
        }

        fn hits(&self) -> usize {
            self.hits.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// 读完请求头（不读体；本模块只发 GET）——不读完就写响应会让客户端偶发 reset。
    fn drain_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let mut buf = [0u8; 1024];
        let mut seen = Vec::new();
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok(());
            }
        }
    }

    fn ok_json(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn status(code: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {code} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    // ── is_newer ────────────────────────────────────────────────────────────

    #[test]
    fn compares_by_numeric_segments_not_lexicographically() {
        // 字典序会把 "1.2.10" 排在 "1.2.9" 前面；数字段比较必须反过来
        assert!(is_newer("1.2.9", "1.2.10").unwrap(), "1.2.10 比 1.2.9 新");
        assert!(
            !is_newer("1.2.10", "1.2.9").unwrap(),
            "1.2.9 不比 1.2.10 新"
        );
        assert!(is_newer("1.9.9", "1.10.0").unwrap(), "1.10.0 比 1.9.9 新");
        assert!(is_newer("0.1.0", "0.2.0").unwrap());
        assert!(is_newer("9.9.9", "10.0.0").unwrap(), "主版本跨到两位也得对");
    }

    #[test]
    fn equal_and_differently_padded_versions_are_not_newer() {
        assert!(!is_newer("1.2.0", "1.2.0").unwrap());
        assert!(!is_newer("v1.2.0", "1.2.0").unwrap());
        // 段数不同按 0 补齐：1.2 == 1.2.0
        assert!(!is_newer("1.2", "1.2.0").unwrap());
        assert!(!is_newer("1.2.0", "1.2").unwrap());
        // 本机比清单新（自编译跑在前面）→ 不是"有新版本"
        assert!(!is_newer("0.3.0", "0.2.9").unwrap());
    }

    #[test]
    fn accepts_v_prefix_on_either_side() {
        assert!(is_newer("1.2.0", "v1.2.1").unwrap());
        assert!(is_newer("v1.2.0", "1.2.1").unwrap());
        assert!(
            !is_newer("v1.2.1", "V1.2.1").unwrap(),
            "大小写 v 都认且相等"
        );
        assert!(is_newer("1.2.1", "v1.2.10").unwrap());
    }

    #[test]
    fn invalid_versions_are_errors_not_quiet_no() {
        // 每一侧都试：把非法串当成"不是新版"是最危险的静默失败
        let bad = ["1.x", "1..2", "1.2.", "", "v", "abc", "1.2.3.4x", "-1.2"];
        for s in bad {
            let err = is_newer(s, "1.2.3").unwrap_err();
            assert!(!err.is_empty(), "当前版本 `{s}` 非法时必须报错");
            let err2 = is_newer("1.2.3", s).unwrap_err();
            assert!(!err2.is_empty(), "清单版本 `{s}` 非法时必须报错");
        }
    }

    #[test]
    fn pre_release_ranks_below_its_release() {
        assert!(
            is_newer("1.2.0-beta.1", "1.2.0").unwrap(),
            "正式版 > 预发布"
        );
        assert!(!is_newer("1.2.0", "1.2.0-beta.1").unwrap());
        assert!(is_newer("1.2.0-beta.1", "1.2.0-beta.2").unwrap());
        assert!(
            is_newer("1.2.0-beta.2", "1.2.0-beta.10").unwrap(),
            "beta.10 比 beta.2 新（不是字典序）"
        );
        assert!(!is_newer("1.2.0-beta.10", "1.2.0-beta.2").unwrap());
        // 构建元数据不参与比较
        assert!(!is_newer("1.2.0+build1", "1.2.0+build2").unwrap());
        assert!(is_newer("1.2.0", "1.2.1-rc1").unwrap());
    }

    // ── parse_release ───────────────────────────────────────────────────────

    #[test]
    fn parses_our_own_manifest_shape() {
        let r = parse_release(
            r#"{"version":"0.2.0","notes":"加了音色库","url":"https://example.com/rel/0.2.0",
                "sha256":"abc123","size":1024}"#,
        )
        .unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.notes, "加了音色库");
        assert_eq!(r.url, "https://example.com/rel/0.2.0");
        assert_eq!(r.sha256.as_deref(), Some("abc123"));
        assert_eq!(r.size, Some(1024));
        assert!(r.summary().contains("0.2.0"));
    }

    #[test]
    fn parses_github_release_api_shape() {
        // 默认清单地址返回的就是这个形状：tag_name/body/html_url/assets
        let r = parse_release(
            r#"{"tag_name":"v0.2.0","name":"0.2.0","body":"更新说明","html_url":"https://github.com/gqf2008/audio-workshop/releases/tag/v0.2.0",
                "assets":[{"name":"audio-workshop-macos.dmg","size":4096,
                           "digest":"sha256:deadbeef","browser_download_url":"https://example.com/a.dmg"}]}"#,
        )
        .unwrap();
        assert_eq!(r.version, "v0.2.0");
        assert_eq!(r.notes, "更新说明");
        assert!(r.url.starts_with("https://github.com/"));
        assert_eq!(r.sha256.as_deref(), Some("deadbeef"));
        assert_eq!(r.size, Some(4096));
    }

    #[test]
    fn optional_fields_may_be_absent_but_present_wrong_types_error() {
        // notes/sha256/size 缺省是允许的（不承重）
        let r = parse_release(r#"{"version":"1.0.0","url":"https://e.com/a"}"#).unwrap();
        assert_eq!(r.notes, "");
        assert_eq!(r.sha256, None);
        assert_eq!(r.size, None);
        // 但**给了**却类型不对，不能静默吞掉
        let e = parse_release(r#"{"version":"1.0.0","url":"https://e.com/a","size":"1024"}"#)
            .unwrap_err();
        assert!(e.contains("size"), "错误要说清是哪个字段：{e}");
        let e2 = parse_release(r#"{"version":"1.0.0","url":"https://e.com/a","notes":[1,2]}"#)
            .unwrap_err();
        assert!(e2.contains("notes"), "{e2}");
    }

    #[test]
    fn missing_required_fields_name_the_field_and_the_keys_present() {
        let e = parse_release(r#"{"url":"https://e.com/a"}"#).unwrap_err();
        assert!(e.contains("version"), "要指名缺哪个字段：{e}");
        assert!(e.contains("url"), "还要把实际有哪些字段列出来：{e}");

        let e2 = parse_release(r#"{"version":"1.0.0"}"#).unwrap_err();
        assert!(e2.contains("url"), "{e2}");

        // 类型不对也要指名道姓
        let e3 = parse_release(r#"{"version":2,"url":"https://e.com/a"}"#).unwrap_err();
        assert!(e3.contains("version") && e3.contains("数字"), "{e3}");
    }

    #[test]
    fn bad_json_is_an_actionable_error_not_up_to_date() {
        let e = parse_release("<html>404 Not Found</html>").unwrap_err();
        assert!(e.contains("JSON"), "要说明是清单本身不是 JSON：{e}");
        let e2 = parse_release("[]").unwrap_err();
        assert!(e2.contains("对象"), "{e2}");
        let e3 = parse_release("").unwrap_err();
        assert!(e3.contains("JSON"), "{e3}");
    }

    #[test]
    fn rejects_non_http_release_url() {
        // 清单是外部输入：file:// 之类的地址不能顺着"打开发布页"交给系统打开器
        let e = parse_release(r#"{"version":"1.0.0","url":"file:///etc/passwd"}"#).unwrap_err();
        assert!(e.contains("http"), "{e}");
        let e2 = parse_release(r#"{"version":"1.0.0","url":"/tmp/x"}"#).unwrap_err();
        assert!(e2.contains("http"), "{e2}");
    }

    #[test]
    fn empty_version_is_rejected() {
        let e = parse_release(r#"{"version":"  ","url":"https://e.com/a"}"#).unwrap_err();
        assert!(e.contains("空"), "{e}");
    }

    // ── check（本地 mock，不碰外网）───────────────────────────────────────────

    #[test]
    fn check_reports_newer_release() {
        let mock = Mock::start(Box::leak(
            ok_json(r#"{"version":"0.2.0","notes":"n","url":"https://e.com/r"}"#).into_boxed_str(),
        ));
        let got = check(&format!("{}/latest", mock.base), "0.1.0").unwrap();
        match got {
            UpdateCheck::Newer(r) => {
                assert_eq!(r.version, "0.2.0");
                assert_eq!(r.notes, "n");
            }
            UpdateCheck::UpToDate => panic!("0.2.0 比 0.1.0 新，必须报有新版"),
        }
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn check_reports_up_to_date_without_a_release() {
        let mock = Mock::start(Box::leak(
            ok_json(r#"{"version":"0.1.0","url":"https://e.com/r"}"#).into_boxed_str(),
        ));
        assert_eq!(
            check(&mock.base, "0.1.0").unwrap(),
            UpdateCheck::UpToDate,
            "同版本必须是 UpToDate"
        );
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn check_surfaces_http_status_and_body() {
        let mock = Mock::start(Box::leak(
            status(404, r#"{"message":"Not Found"}"#).into_boxed_str(),
        ));
        let e = check(&mock.base, "0.1.0").unwrap_err();
        assert!(e.contains("404"), "要报出状态码：{e}");
        assert!(e.contains("Not Found"), "要带出响应体原因：{e}");
        assert!(e.contains("Release"), "404 要给下一步：{e}");
        // 4xx 不能被当成"已是最新"
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn check_surfaces_broken_manifest() {
        let mock = Mock::start(Box::leak(ok_json(r#"{"oops":1}"#).into_boxed_str()));
        let e = check(&mock.base, "0.1.0").unwrap_err();
        assert!(e.contains("version"), "坏清单要说缺什么：{e}");
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn check_rejects_non_http_url_before_any_request() {
        let e = check("ftp://example.com/latest", "0.1.0").unwrap_err();
        assert!(e.contains("http"), "{e}");
        let e2 = check("", "0.1.0").unwrap_err();
        assert!(e2.contains("http"), "{e2}");
    }

    #[test]
    fn check_classifies_connection_refused_as_unreachable() {
        // 绑一次再放掉：这个端口上确定没有监听者 → 连接被拒（不是超时、不是 HTTP）
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let e = check(&format!("http://127.0.0.1:{port}/latest"), "0.1.0").unwrap_err();
        assert!(e.starts_with("连不上"), "要区分成「连不上」：{e}");
    }

    #[test]
    fn check_hits_read_timeout_instead_of_hanging() {
        // 服务器接了连接但永不回包：只有 per-read 超时能救。用 300ms 的短超时跑，
        // 顺手证明这条超时**真的被设进 agent**（不设就会一直挂到测试超时/永不返回）。
        let mock = Mock::start_stalling();
        let started = std::time::Instant::now();
        let e = check_with(
            &mock.base,
            "0.1.0",
            Timeouts {
                connect: Duration::from_millis(500),
                read: Duration::from_millis(300),
            },
        )
        .unwrap_err();
        let took = started.elapsed();
        assert!(e.contains("超时"), "要说清是超时：{e}");
        assert!(
            took < Duration::from_secs(2),
            "per-read 超时该在 300ms 量级就返回，实际 {took:?}"
        );
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn outcome_view_keeps_a_release_page_only_for_a_newer_version() {
        let (info, keep) = outcome_view(
            "0.1.0",
            Ok(UpdateCheck::Newer(Release {
                version: "0.2.0".into(),
                notes: "n".into(),
                url: "https://e.com/r".into(),
                sha256: Some("ab".into()),
                size: Some(4096),
            })),
        );
        assert!(
            info.contains("0.2.0") && info.contains("有新版本"),
            "{info}"
        );
        assert_eq!(keep.map(|r| r.url), Some("https://e.com/r".to_string()));

        // 已是最新：必须把上一次那份收掉，且状态行带上当前版本
        let (info, keep) = outcome_view("0.1.0", Ok(UpdateCheck::UpToDate));
        assert!(info.contains("0.1.0") && info.contains("最新"), "{info}");
        assert!(keep.is_none(), "没有新版时不能留下可点的发布页");

        // 失败：同样收掉发布页，并带上原因
        let (info, keep) = outcome_view("0.1.0", Err("超时：30 秒内没读到数据".into()));
        assert!(
            info.starts_with("检查更新失败：") && info.contains("超时"),
            "{info}"
        );
        assert!(keep.is_none());
    }

    #[test]
    fn snippet_collapses_and_truncates_long_bodies() {
        let long = "a".repeat(500);
        let s = snippet(&long);
        assert!(s.ends_with('…'));
        assert!(s.chars().count() <= 201, "截断后不该还有 500 字");
        assert_eq!(snippet("a\n b\t c"), "a b c");
    }
}
