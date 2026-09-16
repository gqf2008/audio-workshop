//! 人声分离真机跑一次（会按需下载 ~200MB 模型，之后走缓存）。
//!
//! 用法：
//!   cargo run -p aw-core --example separate_run -- <输入音频> <输出目录> [本地模型目录]

use aw_core::separate::{separate_tracks, SeparationOutcome, SeparationRequest};
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!("用法：separate_run <输入音频> <输出目录> [本地模型目录]");
        std::process::exit(2);
    };
    let model_dir = args.next().map(PathBuf::from);

    let req = SeparationRequest {
        input: PathBuf::from(&input),
        out_dir: PathBuf::from(&out_dir),
        stem: "cli".to_string(),
        model_dir,
        chunk_seconds: Some(30),
    };

    let started = std::time::Instant::now();
    let mut last_pct = -1i32;
    let result = separate_tracks(
        &req,
        |p| {
            let pct = (p.percent * 100.0).round() as i32;
            if pct != last_pct {
                last_pct = pct;
                println!("[{pct:3}%] {}", p.note);
            }
        },
        || false,
    );

    match result {
        Ok(SeparationOutcome::Done(tracks)) => {
            println!(
                "完成（{:.1}s）：\n  人声 = {}\n  伴奏 = {}",
                started.elapsed().as_secs_f32(),
                tracks.vocals.display(),
                tracks.accompaniment.display()
            );
        }
        Ok(SeparationOutcome::Stopped) => println!("已停止（未落盘）"),
        Err(e) => {
            eprintln!("失败：{e}");
            std::process::exit(1);
        }
    }
}
