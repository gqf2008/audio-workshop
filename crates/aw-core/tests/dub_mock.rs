//! 合成/拼装的**失败可见性**与默认参数（评审第 10/11 条），用进程内 mock 跑真链路：
//! - 每个请求都必须带默认 instruction（Python `cmd_synth` 恒发）
//! - 全句失败时 `synthesize` 必须返回失败句数，而不是 `Ok(())` 的假成功
//! - `assemble` 必须报出被跳过的句数，而不是静默丢句

mod support;

use aw_core::{Client, Project, DEFAULT_INSTRUCTION};
use std::time::Duration;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aw-core-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn project() -> Project {
    Project::new(
        "第一句。第二句。第三句。",
        "audio8-tts",
        100,
        831001,
        None,
        aw_core::DEFAULT_PUNCTUATION,
        80,
        |t| t.to_string(),
    )
}

fn client(base: &str) -> Client {
    Client::new(base).with_retry(2, Duration::from_millis(1))
}

#[test]
fn sends_default_instruction_and_counts_failures() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&wav)),
        (500, r#"{"error":"模型没加载"}"#.into()),
        (200, support::audio_response(&wav)),
    ]);
    let dir = temp_dir("failure-count");
    let mut prj = project();
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, None, |_, _| {})
        .unwrap();

    // 第 11 条：全句失败也要有聚合信号（这里是 1 句失败，不是 Ok(())）
    assert_eq!(failed, 1, "失败句数必须返回");
    assert_eq!(
        prj.sentences[1].status,
        "error: 服务端拒绝: HTTP 500 {\"error\":\"模型没加载\"}"
    );
    assert_eq!(prj.sentences[0].status, "done");

    // 第 10 条：默认 instruction 必须发出去（Python `cmd_synth` 恒发）
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 3);
    for b in &bodies {
        assert!(
            b.contains(&format!(r#""instruction":"{DEFAULT_INSTRUCTION}""#)),
            "请求应带默认 instruction: {b}"
        );
        assert!(b.contains(r#""seed":"#), "seed 应逐句固定可复现: {b}");
    }
    assert!(bodies[0].contains("第一句。"), "首句文本应落在请求里");

    // 拼装：成功 2 句 + 跳过 1 句，跳过数必须报出来
    let out = prj.assemble(&dir).unwrap();
    assert_eq!(out.done, 2);
    assert_eq!(out.skipped, 1, "被跳过的失败句数必须可见");
    assert!(out.duration > 0.0);
}

/// 全部句子都失败：synthesize 返回全部失败数、assemble 明确报错（不是产出空成品）
#[test]
fn all_sentences_failing_is_reported_not_silent() {
    let mock = support::Mock::start(vec![(500, r#"{"error":"模型没加载"}"#.into())]);
    let dir = temp_dir("all-failed");
    let mut prj = project();
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, None, |_, _| {})
        .unwrap();
    assert_eq!(failed, prj.sentences.len(), "全失败要如实报数");
    let err = prj.assemble(&dir).unwrap_err();
    assert!(err.contains("还没有已合成的句子"), "应明确失败: {err}");
}

/// 重录要换 seed（Python `cmd_redo`：`s["seed"] += 1000`），否则拿回同一条音频；
/// 文本改动要走文本层
#[test]
fn redo_bumps_seed_and_normalizes_new_text() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
    ]);
    let dir = temp_dir("redo");
    let mut prj = project();
    let c = client(&mock.base);
    let failed = prj
        .synthesize(&c, &dir, None, None, |_, _| {})
        .expect("首轮合成");
    assert_eq!(failed, 0);
    let seed_before = prj.sentences[1].seed;

    let failed = prj
        .redo(
            &c,
            &dir,
            1,
            Some("改后的这一句 2026 年。"),
            |t| aw_core::normalize(t, &Default::default()),
            None,
            |_, _| {},
        )
        .expect("重录调用");
    assert_eq!(failed, 0);
    assert_eq!(
        prj.sentences[1].seed,
        seed_before + aw_core::REDO_SEED_STEP,
        "重录必须换 seed"
    );
    assert_eq!(prj.sentences[1].text, "改后的这一句 2026 年。");
    assert_eq!(
        prj.sentences[1].spoken, "改后的这一句 二零二六年。",
        "重录的新文本也要过文本层"
    );
    // 只重录指定句
    assert_eq!(prj.sentences[0].seed, seed_before - 1);
    assert_eq!(mock.hit_count(), 4, "3 句首轮 + 1 句重录");

    // 重录编号不存在要报错，不能静默什么都不做
    assert!(prj
        .redo(&c, &dir, 99, None, |t| t.to_string(), None, |_, _| {})
        .is_err());
}

/// 每合成完一句，project.json 必须已经落盘（Python `cmd_synth` 逐句落盘同款）：
/// 中途被杀也能从磁盘恢复到「已完成句 done」的状态继续跑。
#[test]
fn project_is_saved_after_every_sentence() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("save-per-sentence");
    let mut prj = project();
    let dir_c = dir.clone();
    prj.synthesize(&client(&mock.base), &dir, None, None, move |idx, msg| {
        if msg.starts_with("done") {
            // 在「下一句合成完成之前」，磁盘上的 project.json 必须已反映这句 done
            let on_disk = aw_core::Project::load(&dir_c).expect("project.json 应已存在");
            assert_eq!(
                on_disk.sentences[idx].status, "done",
                "第 {idx} 句完成后应立即落盘"
            );
        }
    })
    .unwrap();
    assert!(dir.join("project.json").is_file());
}

/// 截断的句 wav 必须让拼装中止并指认（Python 同款判据：实际字节 < 头声明需要）。
/// 截断的证据不在 WAV 头里——头仍合法、头里的帧数也不被截断改写，只有字节数可信。
#[test]
fn truncated_sentence_file_aborts_assemble() {
    let wav = support::tiny_wav(&[7i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("truncated");
    let mut prj = project();
    prj.synthesize(&client(&mock.base), &dir, None, None, |_, _| {})
        .unwrap();

    // 把第 0 句截掉一半（hound 的 44 字节头还在，所以"头检查"看不出问题）
    let p0 = dir.join("sentences/000.wav");
    let full = std::fs::metadata(&p0).unwrap().len();
    let mut bytes = std::fs::read(&p0).unwrap();
    bytes.truncate((full / 2) as usize);
    std::fs::write(&p0, &bytes).unwrap();

    let err = prj.assemble(&dir).unwrap_err();
    assert!(
        err.contains("拼装中止") && err.contains("截断"),
        "应指认截断并中止: {err}"
    );
    // 半成品不得冒充成品
    assert!(!dir.join("out/final.wav").is_file());
}

/// 重录的文本/seed 修改必须先落盘再合成（Python cmd_redo 修复的坑：
/// synthesize 从磁盘重载时会丢掉未保存的修改）。
#[test]
fn redo_persists_before_resynthesis() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("redo-persist");
    let mut prj = project();
    let c = client(&mock.base);
    prj.synthesize(&c, &dir, None, None, |_, _| {}).unwrap();

    prj.redo(
        &c,
        &dir,
        2,
        Some("全新文本 2026 年。"),
        |t| aw_core::normalize(t, &Default::default()),
        None,
        |_, _| {},
    )
    .unwrap();

    // 磁盘上的工程必须已带新文本与新 seed（seed 在原值 +1000 的位置）
    let on_disk = aw_core::Project::load(&dir).expect("重录后 project.json 可读");
    assert_eq!(on_disk.sentences[2].text, "全新文本 2026 年。");
    assert_eq!(
        on_disk.sentences[2].seed,
        prj.base_seed + 2 + aw_core::REDO_SEED_STEP
    );
}

/// 成品与 SRT 的原子性：拼装完成后不得残留 .tmp 文件
#[test]
fn assemble_leaves_no_tmp_files() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("no-tmp");
    let mut prj = project();
    prj.synthesize(&client(&mock.base), &dir, None, None, |_, _| {})
        .unwrap();
    prj.assemble(&dir).unwrap();
    for entry in std::fs::read_dir(dir.join("out")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".tmp"), "不得残留临时文件: {name}");
    }
}

/// 协作取消：置 stop 位后，已完成句保留、剩余句不再发起请求
#[test]
fn stoppable_synthesize_keeps_done_and_stops() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let dir = temp_dir("stoppable");
    let mut prj = project();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let c = client(&mock.base);
    let mut done_seen = 0usize;
    prj.synthesize_stoppable(&c, &dir, None, None, Some(&stop), |_, msg| {
        if msg.starts_with("done") {
            done_seen += 1;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .unwrap();
    assert_eq!(done_seen, 1, "只应完成一句就被取消");
    assert_eq!(prj.sentences[0].status, "done");
    assert_eq!(prj.sentences[1].status, "pending");
    assert_eq!(prj.sentences[2].status, "pending");
    assert_eq!(mock.hit_count(), 1, "取消后不得再发请求");
}

/// 质检分数要能跨会话留存（写进工程），而**重新合成那一句必须把它清掉**——
/// 否则界面会拿旧分数描述新音频。
#[test]
fn eval_percent_survives_save_and_is_cleared_by_resynthesis() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let dir = temp_dir("eval-persist");
    let mut prj = project();
    // 假装上一轮质检给第 0 句打了 91.5 分
    prj.sentences[0].status = "done".into();
    prj.sentences[0].duration = Some(0.1);
    prj.sentences[0].eval_percent = Some(91.5);
    prj.save(&dir).unwrap();

    // 跨会话：重新读回来分数还在（没有这个字段的旧工程按 None 处理，`#[serde(default)]`）
    let loaded = Project::load(&dir).unwrap();
    assert_eq!(loaded.sentences[0].eval_percent, Some(91.5));
    assert_eq!(loaded.sentences[1].eval_percent, None);

    // 重新合成第 1 句 → 它的旧分数作废
    let mock = support::Mock::start(vec![(200, support::audio_response(&wav))]);
    let mut prj = loaded;
    prj.sentences[1].eval_percent = Some(50.0);
    let failed = prj
        .synthesize(&client(&mock.base), &dir, Some(&[1]), None, |_, _| {})
        .unwrap();
    assert_eq!(failed, 0);
    assert_eq!(
        prj.sentences[1].eval_percent, None,
        "音频换了，旧质检分数必须清掉"
    );
    assert_eq!(
        prj.sentences[0].eval_percent,
        Some(91.5),
        "没重合成的句子分数要保留"
    );
}

/// 重新合成**失败**时这句没有分数：aw-core 在**开始重做**时就把旧分作废并落盘
/// （先清后写，磁盘上不会出现"新音频 + 旧分数"），失败后两边都没有分数——
/// 丢一个分数比显示一个错的分数好，重跑质检可补（复核两轮后收敛到这个语义）。
#[test]
fn failed_resynthesis_leaves_no_score_on_disk() {
    let dir = temp_dir("eval-fail-clear");
    let mut prj = project();
    prj.sentences[0].status = "done".into();
    prj.sentences[0].duration = Some(0.1);
    prj.sentences[0].eval_percent = Some(88.0);
    prj.save(&dir).unwrap();

    // mock 返回 500：这一句合成失败
    let mock = support::Mock::start(vec![(500, r#"{"error":"模型没加载"}"#.into())]);
    let failed = prj
        .synthesize(&client(&mock.base), &dir, Some(&[0]), None, |_, _| {})
        .unwrap();
    assert_eq!(failed, 1);
    assert_eq!(prj.sentences[0].eval_percent, None, "内存里要清");

    let on_disk = Project::load(&dir).unwrap();
    assert_eq!(
        on_disk.sentences[0].eval_percent, None,
        "磁盘上也要清（旧分已经作废并落盘）"
    );
    assert!(
        on_disk.sentences[0].status.starts_with("error"),
        "状态应记为失败：{}",
        on_disk.sentences[0].status
    );
}
