//! 重试策略（评审第 9 条）：按**状态码**判 503、次数与 Python `post_retry(tries=6)` 对齐、
//! 传输/解析错误不重试。用进程内 mock 确定性验证，不依赖真机服务。

mod support;

use aw_core::Client;
use std::time::Duration;

fn client(base: &str, retries: u32) -> Client {
    // 退避取 1ms：测的是"重不重试/重试几次"，不是退避时长
    Client::new(base).with_retry(retries, Duration::from_millis(1))
}

#[test]
fn retries_on_503_until_success() {
    let mock = support::Mock::start(vec![
        (503, r#"{"error":"内存预检拒绝"}"#.into()),
        (503, r#"{"error":"模型忙碌"}"#.into()),
        (
            200,
            support::audio_response(&support::tiny_wav(&[0, 1, 2, 3])),
        ),
    ]);
    let c = client(&mock.base, 6);
    let wav = c.synth("audio8-tts", "你好", Some(1), None).unwrap();
    assert!(!wav.is_empty());
    assert_eq!(mock.hit_count(), 3, "503 后应重试直到成功");
}

/// 结构化内存不足是“明确拒绝”，不是“服务忙”：不能在后台白等 5 次退避，
/// 要把控制权立刻交回配音队列，让用户释放内存后点继续。
#[test]
fn insufficient_memory_is_not_retried() {
    let body = r#"{"error":{"message":"cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}"#;
    let mock = support::Mock::start(vec![(503, body.into())]);
    let c = client(&mock.base, 6);
    let err = c.synth("audio8-tts", "你好", Some(1), None).unwrap_err();
    assert_eq!(mock.hit_count(), 1, "OOM 必须只发一次，不能自动重试");
    let note = err.to_string();
    assert!(note.contains("释放模型内存"), "提示要有可执行动作: {note}");
    assert!(note.contains("q4_0"), "提示要有可执行动作: {note}");
    assert!(note.contains("3.84 GiB"), "服务端原文不能被吞: {note}");
}

/// **HTTP 级**的不变式：传输层必须把 503 的 body **原样**交给上层，
/// 展示层才做截断。构造一条合法但很长的 insufficient_memory body，
/// 把 `"type":"insufficient_memory"` 与那几个数字推到第 200 个字符**之后**：
/// 只要 post_once 提前截断，JSON 就会残缺 → `insufficient_memory()` 解析不出来 → 本条红。
///
/// 这同时钉住两件事：OOM 不重试（hit_count == 1）与 body 不被提前截断。
#[test]
fn long_insufficient_memory_body_is_not_truncated_on_the_http_path() {
    let head = "cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB \
                headroom exceeds available host memory (3.84 GiB)";
    // 填充要足够长：让 type 落在 200 字符之后（否则这条用例证明不了截断问题）
    let body = format!(
        r#"{{"error":{{"message":"{head}{}","type":"insufficient_memory"}}}}"#,
        "；后面还跟着一长段与判定无关的补充说明".repeat(12)
    );
    assert!(
        body.find(r#""type""#).unwrap() > 200,
        "构造失败：type 必须落在 200 字符之后，否则测不到截断"
    );

    let mock = support::Mock::start(vec![(503, body)]);
    let c = client(&mock.base, 6);
    let err = c
        .asr_with("qwen3-asr", std::path::Path::new("/tmp/a.wav"))
        .unwrap_err();

    assert_eq!(mock.hit_count(), 1, "OOM 必须只发一次，不能自动重试");
    let mem = err
        .insufficient_memory()
        .expect("长 body 也必须被结构化识别（说明传输层没提前截断）");
    assert_eq!(mem.model.as_deref(), Some("qwen3-asr"));
    assert_eq!(mem.estimated_mib, Some(3389), "估算值要完整");
    assert_eq!(mem.required_mib(), Some(4413), "估算 + 余量");
    assert_eq!(mem.available_mib, Some(3932), "可用内存要完整");
    assert!(
        mem.message.contains(head),
        "服务端原文要透出：{}",
        mem.message
    );
}

#[test]
fn gives_up_after_six_attempts_like_python() {
    let mock = support::Mock::start(vec![(503, r#"{"error":"一直忙"}"#.into())]);
    let c = client(&mock.base, aw_core::audio_client::DEFAULT_RETRIES);
    let err = c.synth("audio8-tts", "你好", Some(1), None).unwrap_err();
    assert_eq!(err.status(), Some(503));
    assert_eq!(
        mock.hit_count(),
        aw_core::audio_client::DEFAULT_RETRIES as usize,
        "Python post_retry(tries=6) 是 6 次尝试，不是 7 次"
    );
    // 错误体必须透出，否则看不见 503 的原因
    assert!(err.to_string().contains("一直忙"), "错误体应透出: {err}");
}

#[test]
fn unload_all_models_reports_names_and_service_errors_truthfully() {
    use std::io::{Read as _, Write as _};

    // 成功路径用裸 TCP mock 钉住**真实 endpoint**；support::Mock 只看 body，
    // 不能在测试里证明服务端路径没写错。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // 读完整请求（headers + Content-Length body），不能在客户端还在写 body
        // 时就响应并关闭；高负载下单次 read 只会拿到半个 headers。
        let mut request = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..n]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head_end = request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
            .expect("请求必须包含 headers 结束标记");
        let head = String::from_utf8_lossy(&request[..head_end]);
        assert!(
            head.starts_with("POST /v1/tasks/unload_all_models HTTP/1.1"),
            "unload endpoint 写错: {head}"
        );
        let content_length = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let already = request.len().saturating_sub(head_end);
        if content_length > already {
            let mut rest = vec![0u8; content_length - already];
            stream.read_exact(&mut rest).unwrap();
        }
        let body = r#"{"unloaded":["yue2","qwen3-asr"]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let note = client(&base, 1).unload_all_models().expect("200 应成功");
    assert!(
        note.contains("yue2") && note.contains("qwen3-asr"),
        "{note}"
    );
    server.join().unwrap();

    let failed = support::Mock::start(vec![(500, r#"{"error":"unload failed"}"#.into())]);
    let err = client(&failed.base, 1)
        .unload_all_models()
        .expect_err("500 必须报错，不能假装卸载成功");
    assert!(err.to_string().contains("unload failed"), "{err}");

    // 200 但协议体不合法也不能当成功；三种畸形体都要显式失败。
    for body in [r#"{}"#, r#"{"unloaded":"x"}"#, r#"{"unloaded":[1]}"#] {
        let malformed = support::Mock::start(vec![(200, body.into())]);
        let err = client(&malformed.base, 1)
            .unload_all_models()
            .expect_err("畸形 unload 响应必须报 Decode，不能假装成功");
        assert!(
            err.to_string().contains("unload") || err.to_string().contains("响应解析失败"),
            "{err}"
        );
    }
}

/// 评审原话：500 且 body 恰好含 "503" 会被旧实现的 `contains("503")` 误判成可重试
#[test]
fn a_500_body_mentioning_503_is_not_retryable() {
    let mock = support::Mock::start(vec![(
        500,
        r#"{"error":"后端在 503 号槽位启动失败"}"#.into(),
    )]);
    let c = client(&mock.base, 6);
    let err = c.synth("audio8-tts", "你好", Some(1), None).unwrap_err();
    assert_eq!(err.status(), Some(500));
    assert_eq!(mock.hit_count(), 1, "500 不该重试");
}

/// 死服务不该让每句白等一整个退避周期（旧实现连传输错误也重试 7 次）
#[test]
fn transport_errors_are_not_retried() {
    // **数连接次数**，不看耗时。
    //
    // 原来这条是用"耗时 < backoff"当作"没重试"的代理指标 —— 那个代理**在 Windows 上不成立**：
    // 连一个没人监听的本地端口，单次 connect 就要 ~2s（SYN 重传），于是它会红，
    // 尽管重试逻辑根本没跑（`retryable = e.status() == Some(503)`，传输错误 status 是 None）。
    // 数连接次数是直接判据，且与平台/时序无关。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = hits.clone();
    std::thread::spawn(move || {
        // 接受连接后立刻关掉：客户端拿到的是**传输错误**（读不到响应），不是 HTTP 状态码
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    drop(s);
                }
                Err(_) => break,
            }
        }
    });

    let backoff = Duration::from_millis(300);
    let c = Client::new(format!("http://{addr}")).with_retry(6, backoff);
    let err = c.synth("audio8-tts", "你好", Some(1), None).unwrap_err();
    assert!(err.status().is_none(), "传输错误没有状态码: {err}");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "传输错误只该试一次（重试会很贵：每句白等 {backoff:?}×5）"
    );
}
