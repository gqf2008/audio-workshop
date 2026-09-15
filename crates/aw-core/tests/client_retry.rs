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
