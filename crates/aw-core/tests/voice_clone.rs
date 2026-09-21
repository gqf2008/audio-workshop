//! 音色克隆的**按引擎可选参考文本**与前置拦截（招牌功能 D11）。
//!
//! 真机事实（issue 里贴过原始输出）：同一段 8s 参考音频只发 `voice_ref`：
//! · `audio8-tts` → HTTP 500
//!   `Audio8 TTS prepare with inline reference audio requires reference_text option`
//! · `index-tts2` → **HTTP 200**，正常出音频
//! 即参考文本是**按引擎**的要求，不是全局硬要求。这里钉的是两件事：
//!   ① 文本非空时两个字段**同时**出现在请求体里；空文本时**连字段都不发**
//!      （发空串与"没给"不是一回事）；
//!   ② 路径为空仍在**发起前**就拦住（类型不变式）；"该引擎要不要文本"的拦截
//!      在 main 侧按 `requires.reference_text` 做（aw-core 不知道引擎）。
//!      audio8-tts 必填时发请求前拦 → 不出现逐句 500；index-tts2 放行且请求体
//!      不带该字段。

mod support;

use aw_core::{Client, Project, VoiceClone, VoiceSource};
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
        .synth("audio8-tts", "你好。", Some(1), VoiceSource::Clone(clone))
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
        "非空文本必须发 reference_text（audio8-tts 等引擎要求成对）：{b}"
    );
}

#[test]
fn clone_without_reference_text_is_allowed_and_sends_voice_ref_only() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);

    // 空白（含纯空格）一律放行：index-tts2 等引擎只收 voice_ref 就 200（真机实测）
    for blank in ["", "   ", "\t\n"] {
        let clone = VoiceClone::new("/tmp/ref.wav", blank).expect("空文本必须放行");
        client(&mock.base)
            .synth("index-tts2", "你好。", Some(1), VoiceSource::Clone(clone))
            .unwrap();
    }
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 3);
    for b in &bodies {
        assert!(
            b.contains(r#""voice_ref":"/tmp/ref.wav""#),
            "必须带 voice_ref：{b}"
        );
        assert!(
            !b.contains("reference_text"),
            "空文本连字段都不该发（发空串与没给不是一回事）：{b}"
        );
    }

    // 非空白就发（不 trim 内容，服务端要的是真实文本）
    let c = VoiceClone::new("/tmp/ref.wav", " 你好 ").unwrap();
    assert_eq!(c.reference_text(), " 你好 ");
    assert_eq!(c.path(), "/tmp/ref.wav");

    // 路径仍要校验："空路径 + 有/无文本"都不许过，否则发出 voice_ref: ""
    for blank in ["", "   ", "\t"] {
        let err = VoiceClone::new(blank, "有文本").unwrap_err();
        assert!(
            err.to_string().contains("参考音频路径"),
            "空路径要报「没给路径」：{err}"
        );
    }
}

#[test]
fn project_without_reference_text_sends_voice_ref_only_on_every_sentence() {
    // index-tts2：参考文本可选 —— 只发 voice_ref，每句都该正常出音频。
    // 改前这里是"一个请求都不发 + 整轮 Err"（全局硬要求，用户被冤枉拦下）。
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("no-text");

    let mut prj = project(Some("/tmp/ref.wav"), None);
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, |_, _| {})
        .unwrap();

    assert_eq!(failed, 0, "缺文本的 index-tts2 克隆必须放行");
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2, "两句各一次请求");
    for b in &bodies {
        assert!(b.contains(r#""voice_ref":"/tmp/ref.wav""#), "{b}");
        assert!(!b.contains("reference_text"), "空文本不该发该字段：{b}");
    }
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
