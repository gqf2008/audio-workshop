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
        // id 不合法的文件不列出来（列出来也会被 `load` 拒绝，点一下才知道是坏的）
        if !is_valid_id(id) {
            broken += 1;
            continue;
        }
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

/// 版本 id 是否合法：只接我们自己生成的形式（`<毫秒>` 或 `<毫秒>-<序号>`）。
///
/// 界面回传的 id 本来就来自 `list()`，但**不校验就等于把路径拼接交给调用方**——
/// `../..` 这类 id 能读到工程目录之外的文件。宁可多一道判断。
fn is_valid_id(id: &str) -> bool {
    let mut parts = id.split('-');
    let Some(ms) = parts.next() else {
        return false;
    };
    if ms.is_empty() || !ms.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && parts.next().is_none(),
    }
}

/// 读一份版本（用于对比 / 回滚）。
pub fn load(project_dir: &Path, id: &str) -> Result<Version, String> {
    if !is_valid_id(id) {
        return Err(format!("版本 id 不合法：{id}"));
    }
    let path = versions_dir(project_dir).join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("版本读不出来：{}（{e}）", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("版本解析失败：{}（{e}）", path.display()))
}

/// 回滚第一步：读出这份版本（**不写盘**）。
///
/// 为什么不在这里直接覆盖 `project.json`：版本快照里的句子都是 `pending`（它是"稿件 + 设置"，
/// 不含音频也不含句级状态）。直接写回去，下一次 `load_resumable` 会因为文本/设置匹配走快路径、
/// 原样返回这份全 pending 工程——"文本相同的句子自动复用"就落空了（复核抓到）。
/// 所以回滚由调用方分两步做：`load_for_rollback` 出版本 → 用**当前磁盘工程**按文本继承
/// 音频与句级状态后再落盘。
pub fn load_for_rollback(project_dir: &Path, id: &str) -> Result<Project, String> {
    Ok(load(project_dir, id)?.project)
}

/// 回滚第二步：把（已经按文本继承过状态的）工程写回 `project.json`。
pub fn commit_rollback(project_dir: &Path, project: &Project) -> Result<(), String> {
    project
        .save(project_dir)
        .map_err(|e| format!("回滚写盘失败：{e}"))
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
    // 音色单独处理（它比的是 ref + hash，显示还要标"内容已变"），所以整个循环里
    // 不再用闭包去 borrow `settings`，避免两处可变借用的冲突。
    if before.model != after.model {
        settings.push(SettingChange {
            field: "引擎",
            before: before.model.clone(),
            after: after.model.clone(),
        });
    }
    if let Some((b, a)) = voice_change(before, after) {
        settings.push(SettingChange {
            field: "音色",
            before: b,
            after: a,
        });
    }
    if before.gap_ms != after.gap_ms {
        settings.push(SettingChange {
            field: "句间停顿",
            before: format!("{}ms", before.gap_ms),
            after: format!("{}ms", after.gap_ms),
        });
    }
    if before.auto_normalize != after.auto_normalize {
        settings.push(SettingChange {
            field: "兜底规则",
            before: if before.auto_normalize { "开" } else { "关" }.to_string(),
            after: if after.auto_normalize { "开" } else { "关" }.to_string(),
        });
    }

    ProjectDiff {
        lines,
        added,
        removed,
        settings,
    }
}

/// 音色差异：**比的是 `voice_ref` 与 `voice_ref_hash`**，不是显示用的文件名。
///
/// 只用文件名比会漏两种情况：同一路径的参考音被换了内容（hash 变）、以及不同目录下
/// 同名的两个参考音——这两种在产物里就是换了音色（复核指出）。
fn voice_change(before: &Project, after: &Project) -> Option<(String, String)> {
    if before.voice_ref == after.voice_ref && before.voice_ref_hash == after.voice_ref_hash {
        return None;
    }
    let content_changed = before.voice_ref.is_some()
        && before.voice_ref == after.voice_ref
        && before.voice_ref_hash != after.voice_ref_hash;
    let mark = |p: &Project| {
        let base = voice_label(p);
        if content_changed {
            format!("{base}（内容已变）")
        } else {
            base
        }
    };
    Some((mark(before), mark(after)))
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
        // JSON 合法但 id 不合法（手改的文件名）：也不列出来，计入坏文件
        let good = serde_json::to_vec(&Version {
            label: "名字不合法".into(),
            created_at: 2,
            project: project("第二句。"),
        })
        .unwrap();
        std::fs::write(versions_dir(&dir).join("名字不合法.json"), good).unwrap();

        let (rows, broken) = list(&dir);
        assert_eq!(rows.len(), 1, "只列得出来的那一份：{rows:?}");
        assert_eq!(broken, 2, "坏 JSON + 非法 id 都要计数");
    }

    /// 版本 id 不合法（路径穿越、绝对路径、空）一律拒绝：不能把工程目录之外的
    /// 文件当版本读进来。
    #[test]
    fn rejects_path_traversal_ids() {
        let dir = temp_dir("bad-id");
        let outside = dir
            .parent()
            .unwrap()
            .join(format!("aw-versions-outside-{}.json", std::process::id()));
        std::fs::write(
            &outside,
            "{\"label\":\"外面的\",\"created_at\":1,\"project\":{}}".as_bytes(),
        )
        .unwrap();
        for bad in [
            "../x",
            "../../etc/passwd",
            "/etc/passwd",
            "",
            "abc",
            "1-2-3",
        ] {
            assert!(load(&dir, bad).is_err(), "非法 id 必须拒绝：{bad}");
        }
        // 合法形式（我们自己生成的）能过
        let id = save(&dir, "好的", &project("第一句。"), 42).unwrap();
        assert!(load(&dir, &id).is_ok());
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn rollback_restores_project_json() {
        let dir = temp_dir("rollback");
        let old = project("第一句。第二句。");
        let id = save(&dir, "初稿", &old, 1_000).unwrap();
        // 之后工程被改成另一份
        project("完全不同的稿子。").save(&dir).unwrap();

        let restored = load_for_rollback(&dir, &id).unwrap();
        assert_eq!(restored.sentences.len(), old.sentences.len());
        commit_rollback(&dir, &restored).unwrap();
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
        assert!(load_for_rollback(&dir, "不存在").is_err());
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

        // 同一路径、参考音内容变了 → 也要报"音色变了"（hash 比路径更能说明问题）
        let mut same_path = project("第一句。");
        same_path.voice_ref = Some("/x/我的声线.wav".into());
        same_path.voice_ref_hash = Some("hash-a".into());
        let mut changed_content = same_path.clone();
        changed_content.voice_ref_hash = Some("hash-b".into());
        let d = diff(&same_path, &changed_content);
        let voice = d
            .settings
            .iter()
            .find(|c| c.field == "音色")
            .expect("要报音色变化");
        assert!(voice.after.contains("内容已变"), "{voice:?}");
        assert!(
            voice.before.contains("内容已变"),
            "两边都要标，免得看不出来谁变了：{voice:?}"
        );

        // 不同目录、同名参考音 → 也是换音色
        let mut same_name = same_path.clone();
        same_name.voice_ref = Some("/y/我的声线.wav".into());
        same_name.voice_ref_hash = Some("hash-a".into());
        let d = diff(&same_path, &same_name);
        assert!(
            d.settings.iter().any(|c| c.field == "音色"),
            "同名不同路径也要报：{d:?}"
        );

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
