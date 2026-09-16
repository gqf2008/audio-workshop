//! 跑一次歌曲生成：
//! `cargo run -p aw-core --example song_run -- <yue2|ace-step> <lyrics.txt> "<style>" <out-dir> [duration_s]`

use aw_core::{generate_song, Client, SongModel, SongOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let model = args
        .next()
        .expect("usage: song_run <model> <lyrics.txt> <style> <out-dir> [duration_s]");
    let lyrics_path = args.next().expect("missing lyrics file");
    let style = args.next().expect("missing style");
    let out_dir = std::path::PathBuf::from(args.next().expect("missing out dir"));
    let duration = args
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(120);
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    let options = SongOptions {
        model: if model == "ace-step" {
            SongModel::AceStep
        } else {
            SongModel::Yue2
        },
        lyrics: std::fs::read_to_string(lyrics_path).expect("read lyrics"),
        style,
        duration_seconds: duration,
        ..Default::default()
    };
    let base = std::env::var("AW_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let client = Client::new(base);
    let path = generate_song(&client, &out_dir, "song", &options).expect("generate song");
    println!("song done: {}", path.display());
}
