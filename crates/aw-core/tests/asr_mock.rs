//! 质检链路里 ASR 请求的形状：**与 Python 权威实现（tools/audio_eval.py::transcribe）
//! 同款请求体**，否则真机行为会和服务端预期不一致。

mod support;

use aw_core::{Client, DEFAULT_ASR_MODEL};
use std::path::Path;

#[test]
fn asr_posts_the_audio_path_and_reads_text_back() {
    let mock = support::Mock::start(vec![(
        200,
        r#"{"text":"第一句测试。","language":"Chinese"}"#.to_string(),
    )]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));

    let text = client.asr(Path::new("/tmp/句子 001.wav")).unwrap();
    assert_eq!(text, "第一句测试。");

    let body = &mock.bodies()[0];
    assert!(body.contains(r#""model":"qwen3-asr""#), "{body}");
    assert!(body.contains(r#""audio":"/tmp/句子 001.wav""#), "{body}");
}

/// ASR 模型名可覆盖（服务里还有 audio8-asr / fun-asr）
#[test]
fn asr_model_can_be_overridden() {
    let mock = support::Mock::start(vec![(200, r#"{"text":"ok"}"#.to_string())]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let text = client
        .asr_with("audio8-asr", Path::new("/tmp/a.wav"))
        .unwrap();
    assert_eq!(text, "ok");
    assert!(mock.bodies()[0].contains(r#""model":"audio8-asr""#));

    assert_eq!(DEFAULT_ASR_MODEL, "qwen3-asr", "默认模型应与 M0 定标一致");
}

/// 响应里没有 text 字段要报明确错误，不能静默当空串（空串会变成"可懂度 0%"的假信号）
#[test]
fn asr_without_text_field_is_an_error() {
    let mock = support::Mock::start(vec![(200, r#"{"language":"Chinese"}"#.to_string())]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let err = client.asr(Path::new("/tmp/a.wav")).unwrap_err();
    assert!(err.to_string().contains("text"), "{err}");
}
