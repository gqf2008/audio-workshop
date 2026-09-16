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

#[test]
#[ignore = "需要本机 audiocpp_server + stable-audio-small-music；用 -- --ignored 显式跑"]
fn bgm_pipeline_end_to_end() {
    let base = env_or("AW_SERVER", "http://127.0.0.1:8080");
    let client = Client::new(&base);
    assert!(client.healthy(), "服务不可用: {base}/health");

    let dir = std::env::temp_dir().join("aw-core-bgm-e2e");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("out")).unwrap();

    // 2s 静音人声轨：BGM e2e 只验证生成/对齐/duck/mix，不为再跑一次 TTS 增加内存压力。
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    {
        let mut w = hound::WavWriter::create(dir.join("out/final.wav"), spec).unwrap();
        for _ in 0..(44_100 * 2) {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();
    }
    std::fs::write(
        dir.join("out/final.srt"),
        "1\n00:00:00,000 --> 00:00:02,000\n测试\n",
    )
    .unwrap();

    let mut project = Project::new(
        "测试句。",
        "audio8-tts",
        0,
        831001,
        None,
        "。！？；…",
        80,
        |t| t.to_string(),
    );
    project.sentences[0].start = Some(0.0);
    project.sentences[0].duration = Some(2.0);
    project.save(&dir).unwrap();

    let options = aw_core::BgmOptions {
        prompt: "温暖克制的科技感口播背景音乐，钢琴与轻电子，无人声".into(),
        segment_seconds: 30.0,
        target_seconds: 2.0,
        base_seed: 831001,
        duck_gain: 0.22,
        fade_ms: 200,
        ..Default::default()
    };
    let segments = aw_core::generate_segments(&client, &dir, &options, |done, total, _| {
        eprintln!("  BGM [{done}/{total}]");
    })
    .expect("BGM 生成调用失败");
    assert_eq!(segments, 1, "2s 目标只需 1 个 30s 段");
    let bgm = aw_core::assemble_bgm(&dir, &options).expect("BGM 对齐失败");
    let artifacts = aw_core::mix_project(&dir, &options).expect("BGM 混音失败");
    let mixed_path = artifacts.mixed.as_ref().expect("混音成功必然有 mixed 轨");
    let voice_path = artifacts.voice.as_ref().expect("混音成功必然有 voice 轨");
    assert!(bgm.is_file() && mixed_path.is_file() && voice_path.is_file());
    let mixed = hound::WavReader::open(mixed_path).unwrap();
    assert_eq!(mixed.spec().channels, 2);
    assert!((artifacts.duration - 2.0).abs() < 0.01);
    eprintln!(
        "  BGM 成品 {:.2}s → {}",
        artifacts.duration,
        mixed_path.display()
    );
}
