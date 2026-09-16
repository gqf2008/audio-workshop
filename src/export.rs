//! 导出：单篇（当前工程）与批量（projects/ 下所有有成品）共用同一份复制实现。
//!
//! 为什么单篇与批量放在一个模块：两者的差别只在"导给谁"，落盘动作必须一模一样
//! （原子复制、覆盖语义、失败文案）。写成两份的话，"单篇导出修了、批量导出没修"
//! 是迟早的事。
//!
//! 批量导出（产品计划 §4.2 P6 的「批量导出全部工程」）故意**不经过 worker**：
//! 它只读磁盘上已经拼好的成品（`out/final.wav` 是原子写出来的），与正在跑的合成
//! 互不干扰，跑在后台线程就够，不必占用任务队列的提交守卫。

use std::path::{Path, PathBuf};

/// 单篇导出结果。未勾选格式也给出明确结果，不静默返回。
#[derive(Debug, PartialEq, Eq)]
pub enum ExportOutcome {
    Exported(PathBuf),
    NoneSelected,
    Failed(String),
}

/// 按导出开关把一份成品（wav，可选 srt）复制到导出目录。
///
/// `srt: None` 表示这份成品没有字幕文件；此时若勾了 SRT，直接给一条能照着做的
/// 失败文案，而不是让 `copy_atomic` 抛一个裸 errno。
pub fn export_one(
    name: &str,
    dir: &Path,
    wav: &Path,
    srt: Option<&Path>,
    wav_on: bool,
    srt_on: bool,
) -> ExportOutcome {
    if !wav_on && !srt_on {
        return ExportOutcome::NoneSelected;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return ExportOutcome::Failed(aw_core::dub::write_failure_note(dir, 0, &e));
    }
    let mut written: Vec<String> = Vec::new();
    if wav_on {
        let t = dir.join(format!("{name}.wav"));
        if let Err(e) = aw_core::dub::copy_atomic(wav, &t) {
            return ExportOutcome::Failed(aw_core::dub::write_failure_note(&t, 0, &e));
        }
        written.push(t.display().to_string());
    }
    if srt_on {
        let Some(srt) = srt else {
            return ExportOutcome::Failed(format!(
                "{name}：没有字幕文件（out/final.srt），这条只导出了 WAV；重新导出时把它一起带上"
            ));
        };
        let t = dir.join(format!("{name}.srt"));
        if let Err(e) = aw_core::dub::copy_atomic(srt, &t) {
            return ExportOutcome::Failed(aw_core::dub::write_failure_note(&t, 0, &e));
        }
        written.push(t.display().to_string());
    }
    let last = written
        .last()
        .cloned()
        .unwrap_or_else(|| dir.display().to_string());
    ExportOutcome::Exported(PathBuf::from(last))
}

/// 一个"有成品"的工程（批量导出的输入）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectOut {
    /// 工程名（= 目录名，也是导出到导出目录时的文件名）
    pub name: String,
    pub wav: PathBuf,
    pub srt: Option<PathBuf>,
}

/// 扫出 `projects/` 下有成品（`out/final.wav`）的工程。
///
/// 顺序按工程名排序：批量导出是"一次性把一批文件放进导出目录"，顺序不稳定会让
/// 每次跑的结果对不上（也让测试变成碰运气）。没有 `out/final.wav` 的目录直接不算
/// 候选——那是没跑完/跑失败/别的杂物，不是"导出失败"。
pub fn scan_projects(root: &Path) -> Vec<ProjectOut> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<ProjectOut> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let dir = e.path();
            let wav = dir.join("out/final.wav");
            if !wav.is_file() {
                return None;
            }
            let srt = dir.join("out/final.srt");
            Some(ProjectOut {
                name: e.file_name().to_string_lossy().into_owned(),
                wav,
                srt: srt.is_file().then_some(srt),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 批量导出汇总。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BatchExportSummary {
    /// 扫到的工程数（= exported + failures.len()）
    pub total: usize,
    pub exported: usize,
    /// 导出失败的工程（含原因）。**一条失败不影响其它篇**：某篇缺字幕不该让
    /// 另外 19 篇也导不出去。
    pub failures: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BatchExportOutcome {
    Done(BatchExportSummary),
    NoneSelected,
    Failed(String),
}

/// 把 `root` 下所有有成品的工程，按导出开关复制到 `dir`。
pub fn export_all(root: &Path, dir: &Path, wav_on: bool, srt_on: bool) -> BatchExportOutcome {
    if !wav_on && !srt_on {
        return BatchExportOutcome::NoneSelected;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return BatchExportOutcome::Failed(aw_core::dub::write_failure_note(dir, 0, &e));
    }
    let projects = scan_projects(root);
    let mut summary = BatchExportSummary {
        total: projects.len(),
        ..Default::default()
    };
    for p in projects {
        match export_one(&p.name, dir, &p.wav, p.srt.as_deref(), wav_on, srt_on) {
            ExportOutcome::Exported(_) => summary.exported += 1,
            // NoneSelected 在上面已经拦过，这里只可能出现在"开关在循环里变"那种
            // 不可能的场景；当成失败报出来，别静默吞
            ExportOutcome::NoneSelected => summary
                .failures
                .push(format!("{}：没有勾选导出格式", p.name)),
            ExportOutcome::Failed(e) => summary.failures.push(e),
        }
    }
    BatchExportOutcome::Done(summary)
}

/// 批量导出的状态行文案（成功与失败分开说；失败要能看见第一条原因）。
pub fn summary_text(summary: &BatchExportSummary, dir: &Path) -> String {
    if summary.total == 0 {
        return format!("批量导出：没有找到有成品的工程（看 {}", dir.display());
    }
    let mut text = format!("批量导出：{} 篇 → {}", summary.exported, dir.display());
    if !summary.failures.is_empty() {
        text.push_str(&format!(
            " · 失败 {} 篇（{}）",
            summary.failures.len(),
            summary.failures[0]
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-export-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个"跑完的工程"：projects/<name>/out/{final.wav,final.srt}
    fn make_project(root: &Path, name: &str, with_srt: bool) {
        let out = root.join(name).join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("final.wav"), format!("wav-{name}")).unwrap();
        if with_srt {
            std::fs::write(out.join("final.srt"), format!("srt-{name}")).unwrap();
        }
    }

    #[test]
    fn scan_lists_only_finished_projects_in_name_order() {
        let root = temp_dir("scan");
        make_project(&root, "乙工程", true);
        make_project(&root, "甲工程", true);
        // 没成品的目录：不算候选（不是"导出失败"）
        std::fs::create_dir_all(root.join("没跑完/out")).unwrap();
        std::fs::write(root.join("没跑完/out/中间物.txt"), b"x").unwrap();
        std::fs::write(root.join("随便一个文件.txt"), b"x").unwrap();

        let got = scan_projects(&root);
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        // 排序是**码位序**（`String::cmp`）：乙(U+4E59) < 甲(U+7532)，所以乙在前。
        // 这里钉的是"顺序确定、与目录遍历顺序无关"，不是"字典序"——本地化排序会随
        // 系统语言变，那正是批量导出不该有的不确定性。
        assert_eq!(names, vec!["乙工程", "甲工程"]);
        assert!(got.iter().all(|p| p.srt.is_some()));
    }

    #[test]
    fn scan_marks_missing_srt_as_none() {
        let root = temp_dir("scan-nosrt");
        make_project(&root, "只有wav", false);
        let got = scan_projects(&root);
        assert_eq!(got.len(), 1);
        assert!(got[0].srt.is_none(), "缺字幕要如实记成 None");
    }

    #[test]
    fn export_all_copies_every_finished_project() {
        let root = temp_dir("all");
        let dst = temp_dir("all-dst");
        make_project(&root, "第一集", true);
        make_project(&root, "第二集", true);
        make_project(&root, "没跑完", false);
        std::fs::remove_file(root.join("没跑完/out/final.wav")).unwrap();

        match export_all(&root, &dst, true, true) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.total, 2, "只算有成品的那两个");
                assert_eq!(s.exported, 2);
                assert!(s.failures.is_empty(), "{:?}", s.failures);
            }
            other => panic!("应导出成功：{other:?}"),
        }
        assert_eq!(
            std::fs::read(dst.join("第一集.wav")).unwrap(),
            "wav-第一集".as_bytes()
        );
        assert_eq!(
            std::fs::read(dst.join("第一集.srt")).unwrap(),
            "srt-第一集".as_bytes()
        );
        assert!(dst.join("第二集.wav").is_file());
        assert!(!dst.join("没跑完.wav").exists());
    }

    /// 某篇缺字幕只该让那篇失败：其它篇照常导出（失败隔离）。
    #[test]
    fn export_all_isolates_failures() {
        let root = temp_dir("isolate");
        let dst = temp_dir("isolate-dst");
        make_project(&root, "有字幕", true);
        make_project(&root, "没字幕", false);

        match export_all(&root, &dst, true, true) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.total, 2);
                assert_eq!(s.exported, 1);
                assert_eq!(s.failures.len(), 1, "{:?}", s.failures);
                assert!(s.failures[0].contains("没字幕"), "{:?}", s.failures);
                assert!(s.failures[0].contains("字幕"), "{:?}", s.failures);
            }
            other => panic!("{other:?}"),
        }
        assert!(dst.join("有字幕.wav").is_file());
        assert!(dst.join("有字幕.srt").is_file());

        // 只导 WAV 时，缺字幕那篇不该算失败（没勾 SRT）
        let dst_wav = temp_dir("isolate-wav");
        match export_all(&root, &dst_wav, true, false) {
            BatchExportOutcome::Done(s) => {
                assert_eq!(s.exported, 2, "只导 WAV 时两篇都该成");
                assert!(s.failures.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn export_all_needs_at_least_one_format_and_does_not_create_dir() {
        let root = temp_dir("none");
        let dst = root.join("不该被创建");
        assert_eq!(
            export_all(&root, &dst, false, false),
            BatchExportOutcome::NoneSelected
        );
        assert!(!dst.exists(), "没选格式不该建目录");
    }

    /// 重复导出：覆盖旧文件，且不留下 .tmp 残渣（copy_atomic 的那条不变量）。
    #[test]
    fn export_all_overwrites_and_leaves_no_temp_files() {
        let root = temp_dir("overwrite");
        let dst = temp_dir("overwrite-dst");
        make_project(&root, "重导", true);
        assert!(matches!(
            export_all(&root, &dst, true, true),
            BatchExportOutcome::Done(_)
        ));
        // 换掉源内容再导一次
        std::fs::write(root.join("重导/out/final.wav"), b"wav-new").unwrap();
        assert!(matches!(
            export_all(&root, &dst, true, true),
            BatchExportOutcome::Done(_)
        ));
        assert_eq!(std::fs::read(dst.join("重导.wav")).unwrap(), b"wav-new");
        let leftovers: Vec<String> = std::fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留临时文件：{leftovers:?}");
    }

    #[test]
    fn export_one_reports_missing_srt_actionably() {
        let dir = temp_dir("one");
        let wav = dir.join("final.wav");
        std::fs::write(&wav, b"wav").unwrap();
        let dst = dir.join("dst");
        match export_one("工程", &dst, &wav, None, true, true) {
            ExportOutcome::Failed(e) => {
                assert!(e.contains("字幕"), "{e}");
                assert!(e.contains("final.srt"), "要说清缺的是哪个文件：{e}");
            }
            other => panic!("缺字幕时要报失败：{other:?}"),
        }
        // WAV 已经写出去了：这条不变量要说清楚（不能假装什么都没发生）
        assert!(dst.join("工程.wav").is_file());
    }

    #[test]
    fn summary_text_says_zero_and_names_first_failure() {
        let dir = PathBuf::from("/tmp/out");
        let empty = BatchExportSummary::default();
        assert!(summary_text(&empty, &dir).contains("没有找到有成品"));
        let s = BatchExportSummary {
            total: 3,
            exported: 2,
            failures: vec!["B：没有字幕文件".into()],
        };
        let text = summary_text(&s, &dir);
        assert!(text.contains("2 篇"), "{text}");
        assert!(text.contains("失败 1 篇"), "{text}");
        assert!(text.contains("没有字幕文件"), "第一条原因要能看见：{text}");
    }
}
