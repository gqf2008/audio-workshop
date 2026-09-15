mod support;

use aw_core::{assemble_bgm, generate_segments, mix_project, BgmOptions, Client, Project};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aw-bgm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wav(samples: &[i16], spec: hound::WavSpec) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut buf, spec).unwrap();
        for s in samples {
            writer.write_sample(*s).unwrap();
        }
        writer.finalize().unwrap();
    }
    buf.into_inner()
}

fn mono_8k(samples: &[i16]) -> Vec<u8> {
    wav(
        samples,
        hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
}

#[test]
fn generating_two_segments_sends_duration_and_fixed_seeds() {
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&mono_8k(&vec![100; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![200; 8000]))),
    ]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("generate");
    let options = BgmOptions {
        prompt: "温暖科技感，无人声".into(),
        segment_seconds: 1.0,
        target_seconds: 2.0,
        base_seed: 900,
        ..Default::default()
    };

    let count = generate_segments(&client, &dir, &options, |_, _, _| {}).unwrap();
    assert_eq!(count, 2);
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains(r#""duration_seconds":"1""#));
    assert!(bodies[0].contains(r#""seed":"900""#));
    assert!(bodies[1].contains(r#""seed":"901""#));

    let bgm = assemble_bgm(&dir, &options).unwrap();
    let reader = hound::WavReader::open(bgm).unwrap();
    assert_eq!(reader.duration(), 16_000, "2s @ 8k mono");
}

#[test]
fn changing_prompt_invalidates_cached_segments() {
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&mono_8k(&vec![100; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![100; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![200; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![200; 8000]))),
    ]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("cache-key");
    let a = BgmOptions {
        prompt: "prompt A".into(),
        segment_seconds: 1.0,
        target_seconds: 2.0,
        base_seed: 1,
        ..Default::default()
    };
    generate_segments(&client, &dir, &a, |_, _, _| {}).unwrap();
    generate_segments(&client, &dir, &a, |_, _, _| {}).unwrap();
    assert_eq!(mock.hit_count(), 2, "同 manifest 应命中缓存");

    let b = BgmOptions {
        prompt: "prompt B".into(),
        ..a
    };
    generate_segments(&client, &dir, &b, |_, _, _| {}).unwrap();
    assert_eq!(mock.hit_count(), 4, "prompt 变化必须重新请求所有段");
    assert!(mock.bodies()[3].contains("prompt B"));
}

#[test]
fn failed_prompt_change_invalidates_old_manifest() {
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&mono_8k(&vec![100; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![100; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![200; 8000]))),
        (500, r#"{"error":"forced failure"}"#.into()),
        (200, support::audio_response(&mono_8k(&vec![100; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![100; 8000]))),
    ]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("cache-pending");
    let a = BgmOptions {
        prompt: "prompt A".into(),
        segment_seconds: 1.0,
        target_seconds: 2.0,
        base_seed: 1,
        ..Default::default()
    };
    generate_segments(&client, &dir, &a, |_, _, _| {}).unwrap();

    let b = BgmOptions {
        prompt: "prompt B".into(),
        ..a.clone()
    };
    assert!(generate_segments(&client, &dir, &b, |_, _, _| {}).is_err());

    // 切回 A 时旧 manifest 已 pending，不允许把已经覆盖成 B 的第 0 段当缓存。
    generate_segments(&client, &dir, &a, |_, _, _| {}).unwrap();
    assert_eq!(mock.hit_count(), 6);
    let bodies = mock.bodies();
    assert!(bodies[4].contains("prompt A"));
    assert!(bodies[5].contains("prompt A"));
}

#[test]
fn zero_frame_segment_is_rejected() {
    let mock = support::Mock::start(vec![(200, support::audio_response(&mono_8k(&[])))]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("zero-frame");
    let options = BgmOptions {
        prompt: "zero".into(),
        segment_seconds: 1.0,
        target_seconds: 1.0,
        ..Default::default()
    };
    let err = generate_segments(&client, &dir, &options, |_, _, _| {}).unwrap_err();
    assert!(err.to_string().contains("0 帧"), "应拒绝零帧段: {err}");
}

#[test]
fn ducking_lowers_bgm_only_around_voice_timeline() {
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&mono_8k(&vec![1000; 8000]))),
        (200, support::audio_response(&mono_8k(&vec![1000; 8000]))),
    ]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("duck");
    let options = BgmOptions {
        prompt: "测试 BGM".into(),
        segment_seconds: 1.0,
        target_seconds: 2.0,
        base_seed: 1,
        duck_gain: 0.2,
        fade_ms: 100,
        ..Default::default()
    };
    generate_segments(&client, &dir, &options, |_, _, _| {}).unwrap();
    assemble_bgm(&dir, &options).unwrap();

    std::fs::create_dir_all(dir.join("out")).unwrap();
    std::fs::write(dir.join("out/final.wav"), mono_8k(&vec![0; 16_000])).unwrap();
    std::fs::write(
        dir.join("out/final.srt"),
        "1\n00:00:00,000 --> 00:00:01,000\n测试\n",
    )
    .unwrap();
    let mut project = Project::new(
        "测试句。",
        "audio8-tts",
        0,
        1,
        None,
        aw_core::DEFAULT_PUNCTUATION,
        80,
        |t| t.to_string(),
    );
    project.sentences[0].start = Some(0.25);
    project.sentences[0].duration = Some(0.5);
    project.save(&dir).unwrap();

    let artifacts = mix_project(&dir, &options).unwrap();
    let mut mixed = hound::WavReader::open(artifacts.mixed).unwrap();
    assert_eq!(mixed.spec().channels, 2);
    let samples: Vec<i16> = mixed.samples::<i16>().collect::<Result<_, _>>().unwrap();
    // 帧 4000 位于人声段正中：1000 × 0.2 = 200（左右声道相同）
    assert_eq!(samples[4000 * 2], 200);
    assert_eq!(samples[4000 * 2 + 1], 200);
    // 帧 100 在人声起点前 0.2375s，已离开 100ms fade：BGM 保持原音量
    assert_eq!(samples[100 * 2], 1000);
    assert_eq!(samples[100 * 2 + 1], 1000);
}

#[test]
fn mix_requires_existing_srt() {
    let mock = support::Mock::start(vec![(
        200,
        support::audio_response(&mono_8k(&vec![1000; 8000])),
    )]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("missing-srt");
    let options = BgmOptions {
        prompt: "srt".into(),
        segment_seconds: 1.0,
        target_seconds: 1.0,
        ..Default::default()
    };
    generate_segments(&client, &dir, &options, |_, _, _| {}).unwrap();
    assemble_bgm(&dir, &options).unwrap();
    std::fs::create_dir_all(dir.join("out")).unwrap();
    std::fs::write(dir.join("out/final.wav"), mono_8k(&vec![0; 8000])).unwrap();
    let mut project = Project::new(
        "句。",
        "audio8-tts",
        0,
        1,
        None,
        aw_core::DEFAULT_PUNCTUATION,
        80,
        |t| t.to_string(),
    );
    project.sentences[0].start = Some(0.0);
    project.sentences[0].duration = Some(1.0);
    project.save(&dir).unwrap();
    let err = mix_project(&dir, &options).unwrap_err();
    assert!(err.contains("final.srt"), "应指出 SRT 缺失: {err}");
}
