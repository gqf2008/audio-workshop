//! 合成/拼装的**失败可见性**与默认参数（评审第 10/11 条），用进程内 mock 跑真链路：
//! - 每个请求都必须带默认 instruction（Python `cmd_synth` 恒发）
//! - 全句失败时 `synthesize` 必须返回失败句数，而不是 `Ok(())` 的假成功
//! - `assemble` 必须报出被跳过的句数，而不是静默丢句

mod support;

use aw_core::{Client, Project, DEFAULT_INSTRUCTION};
use std::time::Duration;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aw-core-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn project() -> Project {
    Project::new(
        "第一句。第二句。第三句。",
        "audio8-tts",
        100,
        831001,
        None,
        aw_core::DEFAULT_PUNCTUATION,
        80,
        |t| t.to_string(),
    )
}

fn client(base: &str) -> Client {
    Client::new(base).with_retry(2, Duration::from_millis(1))
}

#[test]
fn sends_default_instruction_and_counts_failures() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&wav)),
        (500, r#"{"error":"模型没加载"}"#.into()),
        (200, support::audio_response(&wav)),
    ]);
    let dir = temp_dir("failure-count");
    let mut prj = project();
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, None, |_, _| {})
        .unwrap();

    // 第 11 条：全句失败也要有聚合信号（这里是 1 句失败，不是 Ok(())）
    assert_eq!(failed, 1, "失败句数必须返回");
    assert_eq!(
        prj.sentences[1].status,
        "error: 服务端拒绝: HTTP 500 {\"error\":\"模型没加载\"}"
    );
    assert_eq!(prj.sentences[0].status, "done");

    // 第 10 条：默认 instruction 必须发出去（Python `cmd_synth` 恒发）
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 3);
    for b in &bodies {
        assert!(
            b.contains(&format!(r#""instruction":"{DEFAULT_INSTRUCTION}""#)),
            "请求应带默认 instruction: {b}"
        );
        assert!(b.contains(r#""seed":"#), "seed 应逐句固定可复现: {b}");
    }
    assert!(bodies[0].contains("第一句。"), "首句文本应落在请求里");

    // 拼装：成功 2 句 + 跳过 1 句，跳过数必须报出来
    let out = prj.assemble(&dir).unwrap();
    assert_eq!(out.done, 2);
    assert_eq!(out.skipped, 1, "被跳过的失败句数必须可见");
    assert!(out.duration > 0.0);
}

/// 全部句子都失败：synthesize 返回全部失败数、assemble 明确报错（不是产出空成品）
#[test]
fn all_sentences_failing_is_reported_not_silent() {
    let mock = support::Mock::start(vec![(500, r#"{"error":"模型没加载"}"#.into())]);
    let dir = temp_dir("all-failed");
    let mut prj = project();
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, None, |_, _| {})
        .unwrap();
    assert_eq!(failed, prj.sentences.len(), "全失败要如实报数");
    let err = prj.assemble(&dir).unwrap_err();
    assert!(err.contains("还没有已合成的句子"), "应明确失败: {err}");
}

/// 重录要换 seed（Python `cmd_redo`：`s["seed"] += 1000`），否则拿回同一条音频；
/// 文本改动要走文本层
#[test]
fn redo_bumps_seed_and_normalizes_new_text() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
    ]);
    let dir = temp_dir("redo");
    let mut prj = project();
    let c = client(&mock.base);
    let failed = prj
        .synthesize(&c, &dir, None, None, |_, _| {})
        .expect("首轮合成");
    assert_eq!(failed, 0);
    let seed_before = prj.sentences[1].seed;

    let failed = prj
        .redo(
            &c,
            &dir,
            1,
            Some("改后的这一句 2026 年。"),
            |t| aw_core::normalize(t, &Default::default()),
            None,
            |_, _| {},
        )
        .expect("重录调用");
    assert_eq!(failed, 0);
    assert_eq!(
        prj.sentences[1].seed,
        seed_before + aw_core::REDO_SEED_STEP,
        "重录必须换 seed"
    );
    assert_eq!(prj.sentences[1].text, "改后的这一句 2026 年。");
    assert_eq!(
        prj.sentences[1].spoken, "改后的这一句 二零二六年。",
        "重录的新文本也要过文本层"
    );
    // 只重录指定句
    assert_eq!(prj.sentences[0].seed, seed_before - 1);
    assert_eq!(mock.hit_count(), 4, "3 句首轮 + 1 句重录");

    // 重录编号不存在要报错，不能静默什么都不做
    assert!(prj
        .redo(&c, &dir, 99, None, |t| t.to_string(), None, |_, _| {})
        .is_err());
}
