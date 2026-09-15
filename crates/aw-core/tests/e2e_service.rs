//! 真机端到端：需要本机 audiocpp_server 在 8080 且已注册 audio8-tts。
//! 未就绪时自动跳过（打印 SKIP），不阻塞普通 cargo test。

use aw_core::{Client, Project};
use std::collections::BTreeMap;

#[test]
fn dub_pipeline_end_to_end() {
    let client = Client::new("http://127.0.0.1:8080");
    if !client.healthy() {
        eprintln!("SKIP: audiocpp_server 未就绪（先 audio-service server ensure）");
        return;
    }
    let dict = BTreeMap::new();
    let script = "第一句测试。第二句测试，含 2026 年。";
    let mut prj = Project::new(
        script,
        "audio8-tts",
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
    prj.synthesize(&client, &dir, None, None, |i, s| eprintln!("  [{i}] {s}"))
        .unwrap();
    let done = prj.sentences.iter().filter(|s| s.status == "done").count();
    assert!(done >= 2, "至少两句应合成成功，实际 {done}");
    let (dur, wav, srt) = prj.assemble(&dir).unwrap();
    assert!(dur > 0.5, "成品时长应大于 0.5s，实际 {dur}");
    let srt_text = std::fs::read_to_string(&srt).unwrap();
    assert!(srt_text.contains(" --> "), "SRT 应含时间轴");
    eprintln!("  成品 {dur:.2}s → {}", wav.display());
}
