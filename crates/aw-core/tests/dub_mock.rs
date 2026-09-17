//! 合成/拼装的**失败可见性**与默认参数（评审第 10/11 条），用进程内 mock 跑真链路：
//! - 请求体的 `options` 只许出现后端 spec 声明过的键（`instruction` 不在其中）
//! - 全句失败时 `synthesize` 必须返回失败句数，而不是 `Ok(())` 的假成功
//! - `assemble` 必须报出被跳过的句数，而不是静默丢句

mod support;

use aw_core::{Client, Project};
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
fn request_options_stay_whitelisted_and_failures_are_counted() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let mock = support::Mock::start(vec![
        (200, support::audio_response(&wav)),
        (500, r#"{"error":"模型没加载"}"#.into()),
        (200, support::audio_response(&wav)),
    ]);
    let dir = temp_dir("failure-count");
    let mut prj = project();
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, |_, _| {})
        .unwrap();

    // 第 11 条：全句失败也要有聚合信号（这里是 1 句失败，不是 Ok(())）
    assert_eq!(failed, 1, "失败句数必须返回");
    assert_eq!(
        prj.sentences[1].status,
        "error: 服务端拒绝: HTTP 500 {\"error\":\"模型没加载\"}"
    );
    assert_eq!(prj.sentences[0].status, "done");

    // 第 10 条：发出去的每个 options 键都必须在后端 spec 的白名单里。
    // 这一条钉的是**真链路**（mock 收到的那份报文），不是 `build_synth_request` 的自证：
    // 真机曾经在这里发 `instruction`，而后端 spec 根本没这个旋钮 → 改了 instruction
    // 输出字节也完全相同（空转）。把 instruction 加回去 → 本用例红。
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 3);
    for b in &bodies {
        let body: serde_json::Value = serde_json::from_str(b).expect("mock 收到的请求体是 JSON");
        // 客户端发的是 `{"model":…, "request":{…}}`，options 在 request 里
        let options = body["request"]["options"]
            .as_object()
            .expect("request.options 应是对象");
        // 这里**故意写死字面量**、不引用 `SYNTH_REQUEST_OPTIONS`：拿被测代码自己的常量去校验
        // 被测代码，改常量时两边一起改，等于没测——变异测试里"把 instruction 放回白名单"
        // 那次就正好漏过去了。字面量依据是服务端 spec 的 `options.request`
        // （audio8_tts / index_tts2 只有 reference_text / multi_reference_cond / max_tokens /
        // text_chunk_size / text_chunk_mode / top_p / top_k / temperature / seed，没有 instruction）。
        const SPEC_DECLARED: &[&str] = &["seed", "reference_text"];
        for key in options.keys() {
            assert!(
                SPEC_DECLARED.contains(&key.as_str()),
                "请求体出现 spec 没声明的键 `{key}`（后端会静默忽略它，用户以为旋钮生效）: {b}"
            );
        }
        assert!(
            options
                .get("seed")
                .and_then(|v| v.as_str())
                .map(|s| s.parse::<u64>().is_ok())
                .unwrap_or(false),
            "seed 应逐句固定可复现（base_seed + index）: {b}"
        );
    }
    assert!(bodies[0].contains("第一句。"), "首句文本应落在请求里");

    // 拼装：成功 2 句 + 跳过 1 句，跳过数必须报出来
    let out = prj.assemble(&dir).unwrap();
    assert_eq!(out.done, 2);
    assert_eq!(out.skipped, 1, "被跳过的失败句数必须可见");
    assert!(out.duration > 0.0);
}

/// OOM 句在队列里保留 `error: oom`，不重试同一请求；重跑只喂 failed_sentence_indices，
/// done 句不再被碰。这是“释放内存后继续”路径的核心回归。
#[test]
fn oom_sentence_is_marked_and_retry_only_reruns_failed_sentence() {
    let wav = support::tiny_wav(&[0i16; 800]);
    let oom = r#"{"error":{"message":"cannot load model 'qwen3-asr': estimated 3.31 GiB + 1024 MiB headroom exceeds available host memory (3.84 GiB)","type":"insufficient_memory"}}"#;
    let mock = support::Mock::start(vec![
        (503, oom.into()),
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
        (200, support::audio_response(&wav)),
    ]);
    let dir = temp_dir("oom-retry");
    let mut prj = project();
    let c = client(&mock.base);

    let failed = prj.synthesize(&c, &dir, None, |_, _| {}).expect("首轮合成");
    assert_eq!(failed, 1, "只有内存不足的那一句失败，不能报成整轮全失败");
    assert_eq!(mock.hit_count(), 3, "OOM 不自动重试；其余两句各发一次");
    assert!(
        prj.sentences[0].status.starts_with("error: oom:"),
        "后台队列要有可识别的 error: oom 标记：{}",
        prj.sentences[0].status
    );
    assert!(prj.sentences[0].status.contains("释放模型内存"));
    assert!(prj.sentences[0].status.contains("3.84 GiB"));
    let on_disk = aw_core::Project::load(&dir).expect("OOM 句必须逐句落盘");
    assert!(
        on_disk.sentences[0].status.starts_with("error: oom:"),
        "磁盘上的队列标记也要可识别：{}",
        on_disk.sentences[0].status
    );
    assert!(on_disk.failed_sentence_indices().contains(&0));
    assert_eq!(prj.sentences[1].status, "done");
    assert_eq!(prj.sentences[2].status, "done");

    let retry = prj.failed_sentence_indices();
    assert_eq!(retry, vec![0], "继续时只选失败句");
    let failed = prj
        .synthesize(&c, &dir, Some(&retry), |_, _| {})
        .expect("只重跑失败句");
    assert_eq!(failed, 0);
    assert_eq!(
        mock.hit_count(),
        4,
        "只应再发失败那一句的请求，done 句不能重跑"
    );
    assert_eq!(prj.sentences[0].status, "done");
    assert_eq!(prj.sentences[1].status, "done");
    assert_eq!(prj.sentences[2].status, "done");
}

/// 全部句子都失败：synthesize 返回全部失败数、assemble 明确报错（不是产出空成品）
#[test]
fn all_sentences_failing_is_reported_not_silent() {
    let mock = support::Mock::start(vec![(500, r#"{"error":"模型没加载"}"#.into())]);
    let dir = temp_dir("all-failed");
    let mut prj = project();
    let failed = prj
        .synthesize(&client(&mock.base), &dir, None, |_, _| {})
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
    let failed = prj.synthesize(&c, &dir, None, |_, _| {}).expect("首轮合成");
    assert_eq!(failed, 0);
    let seed_before = prj.sentences[1].seed;

    let failed = prj
        .redo(
            &c,
            &dir,
            1,
            Some("改后的这一句 2026 年。"),
            |t| aw_core::normalize(t, &Default::default()),
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
        .redo(&c, &dir, 99, None, |t| t.to_string(), |_, _| {})
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
    prj.synthesize(&client(&mock.base), &dir, None, move |idx, msg| {
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
    prj.synthesize(&client(&mock.base), &dir, None, |_, _| {})
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
    prj.synthesize(&c, &dir, None, |_, _| {}).unwrap();

    prj.redo(
        &c,
        &dir,
        2,
        Some("全新文本 2026 年。"),
        |t| aw_core::normalize(t, &Default::default()),
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
    prj.synthesize(&client(&mock.base), &dir, None, |_, _| {})
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
    prj.synthesize_stoppable(&c, &dir, None, Some(&stop), |_, msg| {
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
        .synthesize(&client(&mock.base), &dir, Some(&[1]), |_, _| {})
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
        .synthesize(&client(&mock.base), &dir, Some(&[0]), |_, _| {})
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
