//! 重试策略（评审第 9 条）：按**状态码**判 503、次数与 Python `post_retry(tries=6)` 对齐、
//! 传输/解析错误不重试。用进程内 mock 确定性验证，不依赖真机服务。

mod support;

use aw_core::Client;
use std::time::{Duration, Instant};

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
    let wav = c.synth("audio8-tts", "你好", Some(1), None, None).unwrap();
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
    let err = c
        .synth("audio8-tts", "你好", Some(1), None, None)
        .unwrap_err();
    assert_eq!(mock.hit_count(), 1, "OOM 必须只发一次，不能自动重试");
    let note = err.to_string();
    assert!(note.contains("卸载空闲模型"), "提示要有可执行动作: {note}");
    assert!(note.contains("q4_0"), "提示要有可执行动作: {note}");
    assert!(note.contains("3.84 GiB"), "服务端原文不能被吞: {note}");
}

#[test]
fn gives_up_after_six_attempts_like_python() {
    let mock = support::Mock::start(vec![(503, r#"{"error":"一直忙"}"#.into())]);
    let c = client(&mock.base, aw_core::audio_client::DEFAULT_RETRIES);
    let err = c
        .synth("audio8-tts", "你好", Some(1), None, None)
        .unwrap_err();
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
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(
            request.starts_with("POST /v1/tasks/unload_all_models HTTP/1.1"),
            "unload endpoint 写错: {request}"
        );
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
}

/// 评审原话：500 且 body 恰好含 "503" 会被旧实现的 `contains("503")` 误判成可重试
#[test]
fn a_500_body_mentioning_503_is_not_retryable() {
    let mock = support::Mock::start(vec![(
        500,
        r#"{"error":"后端在 503 号槽位启动失败"}"#.into(),
    )]);
    let c = client(&mock.base, 6);
    let err = c
        .synth("audio8-tts", "你好", Some(1), None, None)
        .unwrap_err();
    assert_eq!(err.status(), Some(500));
    assert_eq!(mock.hit_count(), 1, "500 不该重试");
}

/// 死服务不该让每句白等一整个退避周期（旧实现连传输错误也重试 7 次）
#[test]
fn transport_errors_are_not_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // 端口随即关闭：连接必被拒绝
    let backoff = Duration::from_millis(300);
    let c = Client::new(format!("http://{addr}")).with_retry(6, backoff);
    let t0 = Instant::now();
    let err = c
        .synth("audio8-tts", "你好", Some(1), None, None)
        .unwrap_err();
    let elapsed = t0.elapsed();
    assert!(err.status().is_none(), "传输错误没有状态码: {err}");
    assert!(
        elapsed < backoff,
        "传输错误应立即返回（否则每句白等 {backoff:?}×5），实际 {elapsed:?}"
    );
}
