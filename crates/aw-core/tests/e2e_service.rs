//! 真机端到端：需要本机 audiocpp_server 在跑，且已注册 audio8-tts。
//!
//! **默认不跑**（`#[ignore]`）。上一版是"服务没起就 eprintln!("SKIP") + return"——
//! 测试静默变绿，而 cargo 默认捕获 stderr，那行 SKIP 根本看不见：
//! 本地全绿、CI 全绿，实际上一次真机链路都没跑过。
//! 标 `#[ignore]` 后默认输出里会出现 `1 ignored`，看得见。
//!
//! 跑法：`cargo test -p aw-core --test e2e_service -- --ignored`
//! 地址与模型可用环境变量覆盖：`AW_SERVER`（默认 http://127.0.0.1:8080）、
//! `AW_TTS_MODEL`（默认 audio8-tts）。
//! 策略类断言（重试/失败计数/默认 instruction）在 tests/client_retry.rs 与
//! tests/dub_mock.rs 里用进程内 mock 确定性验证，不依赖真机。

use aw_core::{Client, Project};
use std::collections::BTreeMap;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[test]
#[ignore = "需要本机 audiocpp_server + audio8-tts；用 -- --ignored 显式跑"]
fn dub_pipeline_end_to_end() {
    let base = env_or("AW_SERVER", "http://127.0.0.1:8080");
    let model = env_or("AW_TTS_MODEL", "audio8-tts");
    let client = Client::new(&base);
    // 被显式要求跑就不该再"跳过"：服务不可用必须红着脸失败，不能绿着通过
    assert!(
        client.healthy(),
        "服务不可用: {base}/health（先 audio-service server ensure，或用 AW_SERVER 指定地址）"
    );

    let dict = BTreeMap::new();
    let script = "第一句测试。第二句测试，含 2026 年。";
    let mut prj = Project::new(
        script,
        &model,
        200,
        831001,
        None,
        "。！？；…",
        80,
        |t| aw_core::normalize(t, &dict),
    );
    assert!(prj.sentences.len() >= 2, "应切出多句");
    assert!(
        prj.sentences[1].spoken.contains("二零二六"),
        "数字应被兜底: {}",
        prj.sentences[1].spoken
    );

    let dir = std::env::temp_dir().join("aw-core-e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let failed = prj
        .synthesize(&client, &dir, None, None, |i, s| eprintln!("  [{i}] {s}"))
        .expect("合成调用本身不应失败");
    assert_eq!(failed, 0, "不该有失败句（失败句数会被返回，不再静默）");
    let done = prj.sentences.iter().filter(|s| s.status == "done").count();
    assert!(done >= 2, "至少两句应合成成功，实际 {done}");

    let out = prj.assemble(&dir).expect("拼装应成功");
    assert_eq!(out.skipped, 0, "不该有被跳过的句子");
    assert!(
        out.duration > 0.5,
        "成品时长应大于 0.5s，实际 {}",
        out.duration
    );
    let srt_text = std::fs::read_to_string(&out.srt).unwrap();
    assert!(srt_text.contains(" --> "), "SRT 应含时间轴");
    eprintln!(
        "  成品 {:.2}s（{}/{} 句）→ {}",
        out.duration,
        out.done,
        out.done + out.skipped,
        out.wav.display()
    );
}
