//! 跑一次翻唱：
//! `cargo run -p aw-core --example song_cover -- <source.wav> <lyrics.txt> "<style>" <out-dir>`

use aw_core::{generate_cover, Client, SongOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let source = args
        .next()
        .expect("usage: song_cover <source.wav> <lyrics.txt> <style> <out-dir>");
    let lyrics_path = args.next().expect("missing lyrics file");
    let style = args.next().expect("missing style");
    let out_dir = std::path::PathBuf::from(args.next().expect("missing out dir"));
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    let options = SongOptions {
        lyrics: std::fs::read_to_string(lyrics_path).expect("read lyrics"),
        style,
        ..Default::default()
    };
    let base = std::env::var("AW_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let path = generate_cover(
        &Client::new(base),
        &out_dir,
        std::path::Path::new(&source),
        "cover",
        &options,
    )
    .expect("generate cover");
    println!("cover done: {}", path.display());
}
