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
//! 策略类断言（重试/失败计数/请求参数白名单）在 tests/client_retry.rs 与
//! tests/dub_mock.rs 里用进程内 mock 确定性验证，不依赖真机。

use aw_core::{Client, Project, VoiceSource};
use std::collections::BTreeMap;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// 质检链路真机：合成一句 → ASR 回读 → 可懂度。
///
/// 锚定"应用内质检的口径在真机上成立"——特别是**数字读法差异不该算错**
/// （参考文本写「二零二六」，ASR 常回读成 2026，归一后必须算一致）。
#[test]
#[ignore = "需要本机 audiocpp_server + audio8-tts + qwen3-asr；用 -- --ignored 显式跑"]
fn eval_roundtrip_end_to_end() {
    let base = env_or("AW_SERVER", "http://127.0.0.1:8080");
    let model = env_or("AW_TTS_MODEL", "audio8-tts");
    let client = Client::new(&base);
    assert!(
        client.healthy(),
        "服务不可用: {base}/health（先 audio-service server ensure）"
    );

    let text = "质检用例：二零二六年共十七人。";
    let dict = BTreeMap::new();
    let mut prj = Project::new(
        text,
        &model,
        200,
        831001,
        None,
        "。！？；…",
        80,
        |t| aw_core::normalize(t, &dict),
    );
    let dir = std::env::temp_dir().join("aw-core-eval-e2e");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let failed = prj
        .synthesize(&client, &dir, None, |i, s| eprintln!("  [{i}] {s}"))
        .expect("合成调用本身不应失败");
    assert_eq!(failed, 0, "不该有失败句");

    let wav = dir.join("sentences/000.wav");
    let hypothesis = match client.asr(&wav) {
        Ok(text) => text,
        Err(e) => {
            // 真机复核要看的是**应用侧用户可见文案**，不是 Debug 里的原始 body：
            // OOM 会被 ClientError::Display 转成含模型/内存/三个动作的可执行提示。
            panic!("ASR 应返回文本；应用侧把 503 转成了：{e}");
        }
    };
    let score = aw_core::intelligibility(text, &hypothesis);
    eprintln!(
        "  参考: {text}\n  回读: {hypothesis}\n  可懂度 {:.1}%（编辑距离 {} / {} 字）",
        score.percent, score.distance, score.total
    );
    assert!(
        score.percent >= 80.0,
        "真机回读可懂度应 ≥80%（数字读法差异不该算错）：{:.1}%",
        score.percent
    );
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
        .synthesize(&client, &dir, None, |i, s| eprintln!("  [{i}] {s}"))
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

/// 音色克隆真机：`voice_ref` **必须**与 `reference_text` 成对下发。
///
/// 锚定的是那个"招牌功能 100% 跑不通"的缺陷：修复前 `synth` 只发 `voice_ref`，
/// audio8-tts 直接 500 —— 用户按 README 做克隆，每一句都失败。
/// 这里用真服务跑三个对照：
///   ① 内置音色（`synth` 不传 clone）→ 200，作为"换了音色"的基准产物；
///   ② **裸 HTTP** 只给 `voice_ref`、不给 `reference_text`（复刻修复前的报文，
///      `Client::synth` 现在不可能这么调）→ 期望**失败**；
///   ③ 成对给（走 `Client::synth`）→ 期望 200，且产物与 ① **不同**。
#[test]
#[ignore = "需要本机 audiocpp_server + audio8-tts；用 -- --ignored 显式跑"]
fn voice_clone_reference_text_end_to_end() {
    let base = env_or("AW_SERVER", "http://127.0.0.1:8080");
    let model = env_or("AW_TTS_MODEL", "audio8-tts");
    let client = Client::new(&base);
    assert!(client.healthy(), "服务不可用: {base}/health");

    // 参考音频：优先用环境变量，其次用示例工程拆出来的人声轨
    let reference = std::env::var("AW_VOICE_REF").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/Documents/音频作坊/projects/示例工程 · 频道口播/stems/示例工程 · 频道口播_vocals.wav")
    });
    assert!(
        std::path::Path::new(&reference).is_file(),
        "参考音频不存在：{reference}（用 AW_VOICE_REF 指定）"
    );
    let reference_text = env_or(
        "AW_VOICE_REF_TEXT",
        "这是一段用来验证人生分离链路是否跑得通的测试音频。",
    );
    let text = "这是一句克隆音色的真机验证。";

    // ① 内置音色（不带两个字段）：拿到基准产物
    let builtin = client
        .synth(&model, text, Some(831001), VoiceSource::BuiltIn)
        .unwrap_or_else(|e| panic!("内置音色合成失败（{model}）：{e}"));

    // ② 裸 HTTP 负对照：只给 voice_ref。修复前 `synth` 发的就是这个报文，
    // 服务端 500 `requires reference_text option`（真机实测）。
    let request = serde_json::json!({
        "model": model,
        "request": { "text": text, "voice_ref": reference },
    });
    let resp = ureq::post(&format!("{base}/v1/tasks/run"))
        .timeout(std::time::Duration::from_secs(120))
        .send_json(request);
    let (code, body) = match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("裸 HTTP 负对照的传输层失败（不该发生）：{e}"),
    };
    eprintln!("  只给 voice_ref（裸 HTTP）: HTTP={code} {body}");
    assert_ne!(
        code, 200,
        "只给 voice_ref 必须失败（服务端要求 reference_text 成对）"
    );

    // ③ 成对克隆：走真正的代码路径
    let clone = aw_core::VoiceClone::new(&reference, &reference_text).expect("参考文本非空");
    let cloned = client
        .synth(&model, text, Some(831001), VoiceSource::Clone(clone))
        .unwrap_or_else(|e| panic!("克隆合成失败（{model} + reference_text）：{e}"));

    eprintln!(
        "  内置音色: {} 字节\n  克隆音色: {} 字节（参考音 {}）",
        builtin.len(),
        cloned.len(),
        reference
    );
    assert!(!cloned.is_empty(), "克隆产物不能为空");
    assert_ne!(
        builtin, cloned,
        "给了参考音+参考文本后产物必须与内置音色不同（否则说明 reference_text 没生效）"
    );
}
