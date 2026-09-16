mod support;

use aw_core::{generate_cover, generate_song, Client, SongModel, SongOptions};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aw-song-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(model: SongModel) -> SongOptions {
    SongOptions {
        model,
        lyrics: "[Verse]\n凌晨的厨房还亮着灯".into(),
        style: "Mandarin Chinese R&B slow jam".into(),
        duration_seconds: 60,
        ..Default::default()
    }
}

#[test]
fn yue2_request_contains_lyrics_style_and_seed() {
    let wav = support::tiny_wav(&[1; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("yue2");
    let out = generate_song(&client, &dir, "song", &options(SongModel::Yue2)).unwrap();
    assert!(out.is_file());

    let body = &mock.bodies()[0];
    assert!(body.contains(r#""model":"yue2""#), "{body}");
    assert!(body.contains("凌晨的厨房还亮着灯"), "{body}");
    assert!(body.contains("Mandarin Chinese R&B slow jam"), "{body}");
    assert!(body.contains(r#""cot":"off""#), "{body}");
    assert!(body.contains(r#""seed":"831001""#), "{body}");
}

#[test]
fn ace_step_request_uses_duration_route() {
    let wav = support::tiny_wav(&[1; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("ace-step");
    generate_song(&client, &dir, "song", &options(SongModel::AceStep)).unwrap();

    let body = &mock.bodies()[0];
    assert!(body.contains(r#""model":"ace-step""#), "{body}");
    assert!(body.contains(r#""task_route":"text2music""#), "{body}");
    assert!(body.contains(r#""duration_seconds":"60""#), "{body}");
    assert!(body.contains(r#""language":"zh""#), "{body}");
}

#[test]
fn cover_runs_sheetsage_then_yue2_melody() {
    let wav = support::tiny_wav(&[2; 800]);
    let mock = support::Mock::start(vec![
        (200, r#"{"abc":"X:1\nK:C\nCDEF"}"#.into()),
        (200, support::audio_response(&wav)),
    ]);
    let client = Client::new(&mock.base).with_retry(1, std::time::Duration::from_millis(1));
    let dir = temp_dir("cover");
    let source = dir.join("source.wav");
    std::fs::write(&source, support::tiny_wav(&[0; 800])).unwrap();
    let out = generate_cover(&client, &dir, &source, "cover", &options(SongModel::Yue2)).unwrap();
    assert!(out.is_file());

    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[0].contains(r#""model":"sheetsage2""#),
        "{}",
        bodies[0]
    );
    assert!(bodies[0].contains("source.wav"), "{}", bodies[0]);
    assert!(bodies[1].contains(r#""model":"yue2""#), "{}", bodies[1]);
    assert!(bodies[1].contains(r#""cot":"melody""#), "{}", bodies[1]);
    assert!(bodies[1].contains("CDEF"), "{}", bodies[1]);
}
