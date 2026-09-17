//! 工程版本（M4-P3）：留档 / 对比 / 回滚。
//!
//! 产品口径（docs/product-plan.md §4.2 P3）：顶栏工程名的「版本历史」——留档 / 对比 / 回滚；
//! 免费侧对照是"只有一份 project.json"。
//!
//! **刻意不快照音频**：版本存的是「稿件 + 设置」（就是 `Project` 的序列化），音频靠
//! `load_resumable` 既有的"按文本继承"复用——回滚到旧稿后，文本没变的句子照样复用，
//! 变了的句子重录。这样一份版本只有几十 KB，而不是每版多几十 MB。
//!
//! 这里只放纯逻辑 + 文件读写；界面接线在 `src/main.rs`。

use std::path::{Path, PathBuf};

use aw_core::Project;

/// 版本文件的固定目录名（放在工程目录里，跟着工程一起备份/迁移）。
pub const VERSIONS_DIR: &str = ".versions";

/// 一份留档：标签 + 时间 + 当时的工程快照。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Version {
    pub label: String,
    /// Unix 毫秒
    pub created_at: u64,
    pub project: Project,
}

/// 列表里的一行（界面用；不把整份 Project 塞进 UI）。
#[derive(Clone, Debug, PartialEq)]
pub struct VersionRow {
    /// 文件名（不含扩展名），回滚/对比时用它定位
    pub id: String,
    pub label: String,
    pub created_at: u64,
    /// 这份版本里有多少句（界面上让人一眼看出稿子长短）
    pub sentences: usize,
}

impl Version {
    pub fn row(&self, id: &str) -> VersionRow {
        VersionRow {
            id: id.to_string(),
            label: self.label.clone(),
            created_at: self.created_at,
            sentences: self.project.sentences.len(),
        }
    }
}

/// 版本目录。
pub fn versions_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(VERSIONS_DIR)
}

/// 留档：把 `project` 存成一份版本，返回它的 id。
///
/// id = Unix 毫秒；同一毫秒内第二次留档会加后缀（`-1`、`-2`…），不覆盖前一份。
pub fn save(
    project_dir: &Path,
    label: &str,
    project: &Project,
    now_ms: u64,
) -> Result<String, String> {
    let dir = versions_dir(project_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建版本目录失败：{}（{e}）", dir.display()))?;
    let mut id = now_ms.to_string();
    let mut n = 1u32;
    while dir.join(format!("{id}.json")).exists() {
        id = format!("{now_ms}-{n}");
        n += 1;
    }
    let label = if label.trim().is_empty() {
        format!("版本 {now_ms}")
    } else {
        label.trim().to_string()
    };
    let v = Version {
        label,
        created_at: now_ms,
        project: project.clone(),
    };
    let body = serde_json::to_vec_pretty(&v).map_err(|e| format!("版本序列化失败：{e}"))?;
    aw_core::dub::write_atomic_explained(&dir.join(format!("{id}.json")), &body)
        .map_err(|e| format!("版本写入失败：{e}"))?;
    Ok(id)
}

/// 列出所有版本（按时间倒序）+ 坏文件数。
///
/// 坏文件**跳过但计数**：一个手改坏的版本文件不该让整个版本历史打不开，也不该被当成
/// "没有版本"——界面要把这个数说出来。
pub fn list(project_dir: &Path) -> (Vec<VersionRow>, usize) {
    let dir = versions_dir(project_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (Vec::new(), 0);
    };
    let mut rows: Vec<(VersionRow, u64)> = Vec::new();
    let mut broken = 0usize;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            broken += 1;
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            broken += 1;
            continue;
        };
        match serde_json::from_str::<Version>(&raw) {
            Ok(v) => rows.push((v.row(id), v.created_at)),
            Err(_) => broken += 1,
        }
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.id.cmp(&a.0.id)));
    (rows.into_iter().map(|(r, _)| r).collect(), broken)
}

/// 读一份版本（用于对比 / 回滚）。
pub fn load(project_dir: &Path, id: &str) -> Result<Version, String> {
    let path = versions_dir(project_dir).join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("版本读不出来：{}（{e}）", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("版本解析失败：{}（{e}）", path.display()))
}

/// 回滚：把版本里的工程写回 `project.json`，并把这份工程返回给调用方（界面据此回灌）。
///
/// 只写 `project.json`，**不动** `sentences/` 与 `out/`：音频交给下一次「开始合成」按
/// 文本继承（同一个工程目录里，文本没变的句子照样复用）。
pub fn rollback(project_dir: &Path, id: &str) -> Result<Project, String> {
    let v = load(project_dir, id)?;
    v.project
        .save(project_dir)
        .map_err(|e| format!("回滚写盘失败：{e}"))?;
    Ok(v.project)
}

/// 句级差异的一步。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineOp {
    Keep(String),
    Add(String),
    Remove(String),
}

/// 设置差异的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingChange {
    pub field: &'static str,
    pub before: String,
    pub after: String,
}

/// 两份工程的差异（`before` → `after`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectDiff {
    pub lines: Vec<LineOp>,
    pub added: usize,
    pub removed: usize,
    pub settings: Vec<SettingChange>,
}

impl ProjectDiff {
    pub fn has_changes(&self) -> bool {
        self.added > 0
            || self.removed > 0
            || !self.settings.is_empty()
            || self
                .lines
                .iter()
                .any(|l| matches!(l, LineOp::Add(_) | LineOp::Remove(_)))
    }

    /// 一句话摘要（面板标题行用）。
    pub fn summary(&self) -> String {
        if !self.has_changes() {
            return "与当前工程完全一致".to_string();
        }
        let mut parts = Vec::new();
        if self.added > 0 {
            parts.push(format!("新增 {} 句", self.added));
        }
        if self.removed > 0 {
            parts.push(format!("删除 {} 句", self.removed));
        }
        if !self.settings.is_empty() {
            parts.push(format!("设置改了 {} 项", self.settings.len()));
        }
        parts.join(" · ")
    }
}

/// 句子文本的 LCS 差异（句子级，不是字符级：配音的最小单位就是句子）。
///
/// 句数不大（几分钟口播 ≈ 几十句），O(n·m) 的 DP 足够；比"按下标逐个比"更能说明
/// "插了一句导致后面全部错位"这种情况。
pub fn diff(before: &Project, after: &Project) -> ProjectDiff {
    let a: Vec<&str> = before.sentences.iter().map(|s| s.text.trim()).collect();
    let b: Vec<&str> = after.sentences.iter().map(|s| s.text.trim()).collect();
    let n = a.len();
    let m = b.len();
    // dp[i][j] = a[i..] 与 b[j..] 的 LCS 长度
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut lines = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let (mut added, mut removed) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            lines.push(LineOp::Keep(a[i].to_string()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            lines.push(LineOp::Remove(a[i].to_string()));
            removed += 1;
            i += 1;
        } else {
            lines.push(LineOp::Add(b[j].to_string()));
            added += 1;
            j += 1;
        }
    }
    while i < n {
        lines.push(LineOp::Remove(a[i].to_string()));
        removed += 1;
        i += 1;
    }
    while j < m {
        lines.push(LineOp::Add(b[j].to_string()));
        added += 1;
        j += 1;
    }

    let mut settings = Vec::new();
    let mut push = |field: &'static str, before: String, after: String| {
        if before != after {
            settings.push(SettingChange {
                field,
                before,
                after,
            });
        }
    };
    push("引擎", before.model.clone(), after.model.clone());
    push("音色", voice_label(before), voice_label(after));
    push(
        "句间停顿",
        format!("{}ms", before.gap_ms),
        format!("{}ms", after.gap_ms),
    );
    push(
        "兜底规则",
        if before.auto_normalize { "开" } else { "关" }.to_string(),
        if after.auto_normalize { "开" } else { "关" }.to_string(),
    );

    ProjectDiff {
        lines,
        added,
        removed,
        settings,
    }
}

/// 音色在差异里怎么显示：内置默认 / 参考文件名（不泄露整条路径，用户认的是文件名）。
fn voice_label(p: &Project) -> String {
    match p.voice_ref.as_deref() {
        None => "内置默认".to_string(),
        Some(path) => Path::new(path)
            .file_name()
            .map(|n| format!("参考音 {}", n.to_string_lossy()))
            .unwrap_or_else(|| format!("参考音 {path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aw_core::{Sentence, DEFAULT_PUNCTUATION};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-versions-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn project(script: &str) -> Project {
        Project::new(
            script,
            "audio8-tts",
            250,
            831001,
            None,
            DEFAULT_PUNCTUATION,
            80,
            |t| t.to_string(),
        )
    }

    #[test]
    fn save_list_load_roundtrip_newest_first() {
        let dir = temp_dir("roundtrip");
        let id1 = save(&dir, "初稿", &project("第一句。第二句。"), 1_000).unwrap();
        let id2 = save(&dir, "改稿", &project("第一句。第二句。第三句。"), 2_000).unwrap();
        assert_ne!(id1, id2);

        let (rows, broken) = list(&dir);
        assert_eq!(broken, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "改稿", "最新的排前面");
        assert_eq!(rows[0].sentences, 3);
        assert_eq!(rows[1].label, "初稿");

        let v = load(&dir, &id1).unwrap();
        assert_eq!(v.label, "初稿");
        assert_eq!(v.project.sentences.len(), 2);
    }

    #[test]
    fn empty_label_gets_a_default_and_same_ms_does_not_overwrite() {
        let dir = temp_dir("same-ms");
        let a = save(&dir, "   ", &project("第一句。"), 5_000).unwrap();
        let b = save(&dir, "第二份", &project("第二句。"), 5_000).unwrap();
        assert_ne!(a, b, "同一毫秒也不能互相覆盖");
        let v = load(&dir, &a).unwrap();
        assert!(v.label.starts_with("版本 "), "空标签要给默认：{}", v.label);
        assert_eq!(list(&dir).0.len(), 2);
    }

    #[test]
    fn broken_files_are_counted_not_silently_dropped() {
        let dir = temp_dir("broken");
        save(&dir, "好的", &project("第一句。"), 1).unwrap();
        std::fs::write(versions_dir(&dir).join("坏的.json"), b"{ not json").unwrap();
        // 非 json 文件不算坏
        std::fs::write(versions_dir(&dir).join("README.txt"), b"x").unwrap();

        let (rows, broken) = list(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(broken, 1, "坏文件要计数并报出来");
    }

    #[test]
    fn rollback_restores_project_json() {
        let dir = temp_dir("rollback");
        let old = project("第一句。第二句。");
        let id = save(&dir, "初稿", &old, 1_000).unwrap();
        // 之后工程被改成另一份
        project("完全不同的稿子。").save(&dir).unwrap();

        let restored = rollback(&dir, &id).unwrap();
        assert_eq!(restored.sentences.len(), old.sentences.len());
        let on_disk = Project::load(&dir).unwrap();
        assert_eq!(
            on_disk
                .sentences
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>(),
            vec!["第一句。", "第二句。"],
            "回滚后磁盘上的稿件要等于快照"
        );

        // 不存在的 id 要报错（不能静默当成功）
        assert!(rollback(&dir, "不存在").is_err());
    }

    /// 句子文本不同 → diff 里能看到删除 + 新增；完全一样 → 没有变化。
    #[test]
    fn diff_reports_text_changes() {
        let before = project("第一句。第二句。");
        let same = project("第一句。第二句。");
        let d = diff(&before, &same);
        assert!(!d.has_changes(), "{d:?}");
        assert_eq!(d.summary(), "与当前工程完全一致");

        let changed = project("第一句。改过的第二句。");
        let d = diff(&before, &changed);
        assert!(d.has_changes());
        assert_eq!((d.added, d.removed), (1, 1), "{d:?}");
        assert!(d.lines.contains(&LineOp::Keep("第一句。".to_string())));
        assert!(d.lines.contains(&LineOp::Add("改过的第二句。".to_string())));
        assert!(d.lines.contains(&LineOp::Remove("第二句。".to_string())));
        assert!(d.summary().contains("新增 1 句") && d.summary().contains("删除 1 句"));

        // 插一句：后面不该被当成"全改"
        let inserted = project("第一句。插入的一句。第二句。");
        let d = diff(&before, &inserted);
        assert_eq!((d.added, d.removed), (1, 0), "插入只应报一句新增：{d:?}");
        assert_eq!(
            d.lines
                .iter()
                .filter(|l| matches!(l, LineOp::Keep(_)))
                .count(),
            2,
            "原有两句仍是 Keep"
        );
    }

    #[test]
    fn diff_reports_setting_changes() {
        let before = project("第一句。");
        let mut after = project("第一句。");
        after.model = "index-tts2".into();
        after.gap_ms = 500;
        after.auto_normalize = false;
        after.voice_ref = Some("/x/我的声线.wav".into());

        let d = diff(&before, &after);
        let fields: Vec<&str> = d.settings.iter().map(|c| c.field).collect();
        assert!(fields.contains(&"引擎"), "{fields:?}");
        assert!(fields.contains(&"句间停顿"), "{fields:?}");
        assert!(fields.contains(&"兜底规则"), "{fields:?}");
        assert!(fields.contains(&"音色"), "{fields:?}");
        let voice = d.settings.iter().find(|c| c.field == "音色").unwrap();
        assert!(
            voice.after.contains("我的声线.wav"),
            "音色显示文件名：{voice:?}"
        );
        assert!(!voice.after.contains("/x/"), "别把整条路径摊开：{voice:?}");
        assert!(d.summary().contains("设置改了 4 项"), "{}", d.summary());

        // 顺序稳定：引擎 → 音色 → 停顿 → 兜底
        assert_eq!(
            fields,
            vec!["引擎", "音色", "句间停顿", "兜底规则"],
            "差异顺序要稳定，便于读"
        );
    }

    #[test]
    fn diff_handles_empty_scripts() {
        let empty = Project {
            sentences: Vec::<Sentence>::new(),
            ..project("第一句。")
        };
        let d = diff(&empty, &project("第一句。"));
        assert_eq!((d.added, d.removed), (1, 0));
        let back = diff(&project("第一句。"), &empty);
        assert_eq!((back.added, back.removed), (0, 1));
    }
}
