//! 对已有配音工程跑完整 BGM 链路：
//! `cargo run -p aw-core --example bgm_run -- <project-dir> "<prompt>" [base-url]`

use aw_core::{assemble_bgm, generate_segments, mix_project, BgmOptions, Client};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .expect("usage: bgm_run <project-dir> <prompt> [base-url]");
    let prompt = args.next().expect("missing prompt");
    let base = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let dir = std::path::PathBuf::from(dir);
    let voice = dir.join("out/final.wav");
    let duration = aw_core::dub::wav_duration(&std::fs::read(&voice).expect("read final.wav"))
        .expect("decode final.wav");
    let options = BgmOptions {
        prompt,
        target_seconds: duration,
        ..Default::default()
    };
    let client = Client::new(base);
    let segments = generate_segments(&client, &dir, &options, |done, total, note| {
        eprintln!("  BGM [{done}/{total}] {note}");
    })
    .expect("generate BGM");
    assemble_bgm(&dir, &options).expect("assemble BGM");
    let artifacts = mix_project(&dir, &options).expect("mix BGM");
    println!(
        "BGM done: segments={segments} duration={:.1}s mixed={}",
        artifacts.duration,
        artifacts
            .mixed
            .as_ref()
            .expect("混音成功必然有 mixed 轨")
            .display()
    );
}
