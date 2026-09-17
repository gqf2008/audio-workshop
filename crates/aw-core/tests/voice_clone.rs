//! 音色克隆的**成对下发**与前置拦截（招牌功能 D11）。
//!
//! 真机事实（issue 里贴过原始输出）：`audio8-tts` 只收 `voice_ref` 不收
//! `reference_text` 会 HTTP 500 —— 而这正是修复前的行为，用户按 README 做克隆
//! **每一句都失败**。所以这里钉的是两件事：
//!   ① 两个字段必须**同时**出现在请求体里（去掉任一行都该红）；
//!   ② 缺文本时**在发起前**就拦住（而不是让 N 句各撞一次同一个 500）。

mod support;

use aw_core::{Client, Project, VoiceClone};
use std::time::Duration;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aw-core-voiceclone-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn client(base: &str) -> Client {
    Client::new(base).with_retry(2, Duration::from_millis(1))
}

fn project(voice_ref: Option<&str>, voice_ref_text: Option<&str>) -> Project {
    let mut p = Project::new(
        "第一句。第二句。",
        "audio8-tts",
        100,
        831001,
        voice_ref.map(str::to_string),
        aw_core::DEFAULT_PUNCTUATION,
        80,
        |t| t.to_string(),
    );
    p.voice_ref_text = voice_ref_text.map(str::to_string);
    p
}

#[test]
fn clone_sends_reference_text_paired_with_voice_ref() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);

    let clone = VoiceClone::new("/tmp/ref.wav", "这是一段参考音频。").unwrap();
    client(&mock.base)
        .synth("audio8-tts", "你好。", Some(1), Some(clone))
        .unwrap();

    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 1);
    let b = &bodies[0];
    // 去掉 synth 里任意一行 → 这两条断言必红
    assert!(
        b.contains(r#""voice_ref":"/tmp/ref.wav""#),
        "必须带 voice_ref：{b}"
    );
    assert!(
        b.contains(r#""reference_text":"这是一段参考音频。""#),
        "必须带 reference_text（服务端要求成对）：{b}"
    );
}

#[test]
fn clone_without_reference_text_is_refused_before_any_request() {
    // 空白（含纯空格）一律拒绝：不是 None，也不是"悄悄发个空串"
    for blank in ["", "   ", "\t\n"] {
        let err = VoiceClone::new("/tmp/ref.wav", blank).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("reference_text") && msg.contains("自动转写"),
            "提示要说清缺什么、去哪填：{msg}"
        );
    }
    // 非空白就通过（不 trim 内容，服务端要的是真实文本）
    let c = VoiceClone::new("/tmp/ref.wav", " 你好 ").unwrap();
    assert_eq!(c.reference_text(), " 你好 ");
    assert_eq!(c.path(), "/tmp/ref.wav");

    // 路径也要校验：只校文本的话"空路径 + 有文本"会过，然后发出 voice_ref: ""
    // —— 与这个类型自称的"成对"不一致（复核指出；UI 走不到，但类型不变式该自己守）
    for blank in ["", "   ", "\t"] {
        let err = VoiceClone::new(blank, "有文本").unwrap_err();
        assert!(
            err.to_string().contains("参考音频路径"),
            "空路径要报「没给路径」：{err}"
        );
    }
}

#[test]
fn project_synthesize_refuses_clone_without_text_before_touching_the_network() {
    let mock = support::Mock::start(vec![(200, "{}".into())]);
    let dir = temp_dir("no-text");

    let mut prj = project(Some("/tmp/ref.wav"), None);
    let err = prj
        .synthesize(&client(&mock.base), &dir, None, |_, _| {})
        .unwrap_err();

    assert_eq!(
        mock.hit_count(),
        0,
        "缺参考文本时一个请求都不该发出去（发了就是 N 句各撞一次 500）"
    );
    assert!(
        err.to_string().contains("reference_text"),
        "错误要说清缺什么：{err}"
    );
    // 前置拦截在循环之前 ⇒ 没有句子被标成 done/error（用户改完文本直接重跑即可）
    assert!(
        prj.sentences.iter().all(|s| s.status == "pending"),
        "拦截时不该动句状态：{:?}",
        prj.sentences.iter().map(|s| &s.status).collect::<Vec<_>>()
    );
}

#[test]
fn project_with_reference_text_sends_the_pair_on_every_sentence() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("with-text");

    let mut prj = project(Some("/tmp/ref.wav"), Some("参考音频的内容。"));
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, |_, _| {})
        .unwrap();

    assert_eq!(failed, 0);
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2, "两句各一次请求");
    for b in &bodies {
        assert!(b.contains(r#""voice_ref":"/tmp/ref.wav""#), "{b}");
        assert!(b.contains(r#""reference_text":"参考音频的内容。""#), "{b}");
    }
}

/// 不克隆（内置音色）时两个字段**都**不该出现：否则服务端会以为这是克隆请求。
#[test]
fn builtin_voice_sends_neither_field() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("builtin");

    let mut prj = project(None, None);
    prj.synthesize(&client(&mock.base), &dir, None, |_, _| {})
        .unwrap();

    for b in &mock.bodies() {
        assert!(!b.contains("voice_ref"), "不该带 voice_ref：{b}");
        assert!(!b.contains("reference_text"), "不该带 reference_text：{b}");
    }
}
