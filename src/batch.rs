//! 批量队列（M4-P1）：一次导入 N 篇稿子，每篇 = 一个工程。
//!
//! 这里只放**纯逻辑**：把用户选中的文件读成"可提交的条目"、以及批量结束时的汇总文案。
//! 执行顺序、任务台账登记、停止语义都不在这里——它们与单篇共用同一条 `Cmd` 通道
//! （`Cmd::RunBatch`，见 `src/main.rs`），所以批量不会长成第二套合成链路。
//!
//! 产品口径（docs/product-plan.md §4.2 P1）：可一次导入 N 篇稿子，排队依次成片；
//! 免费侧是"单篇串行合成"，付费侧才是这条批量队列。

use std::path::{Path, PathBuf};

/// 批量里的一条：一篇稿子 = 一个工程。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchItem {
    /// 工程名（= 稿件文件名去扩展名，再过 `file_stem` 归一）
    pub name: String,
    /// 稿件文件路径（状态栏/列表里告诉用户这条是从哪个文件来的）
    pub path: PathBuf,
    /// 稿件正文
    pub script: String,
}

/// 导入结果：能跑的条目 + 被跳过的原因。
///
/// `skipped` 是**给用户看的文案**，不是日志：每条都要说清是哪个文件、为什么没进来
/// （读不到 / 不是 UTF-8 文本 / 空稿 / 与前面某条重名），否则用户只会看到"我选了 5 个
/// 怎么只跑了 3 个"。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    pub items: Vec<BatchItem>,
    pub skipped: Vec<String>,
}

/// 把用户选中的稿件读成可提交条目。
///
/// - 目录 / 不存在的路径：跳过
/// - 非 UTF-8（不是纯文本稿子，比如误选了 wav）：跳过并说是编码问题
/// - 空稿（只有空白字符）：跳过
/// - 归一后重名：只留先选中的那条，后一条跳过并指明与谁重名——两个工程同名会互相
///   覆盖落盘目录，这比"少跑一篇"严重得多
///
/// `sanitize` 由调用方传 `crate::file_stem`：批量落盘目录必须与单篇同一套归一规则，
/// 否则同一个工程名在两条路径下会落到两个目录。
pub fn import_scripts(paths: &[PathBuf], sanitize: impl Fn(&str) -> String) -> ImportOutcome {
    let mut out = ImportOutcome::default();
    for path in paths {
        let shown = display_path(path);
        if !path.is_file() {
            out.skipped
                .push(format!("{shown}：不是文件（或已不存在），跳过"));
            continue;
        }
        let raw = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                out.skipped.push(format!("{shown}：读不到（{e}），跳过"));
                continue;
            }
        };
        let script = match String::from_utf8(raw) {
            Ok(text) => text,
            Err(_) => {
                out.skipped
                    .push(format!("{shown}：不是 UTF-8 文本稿子，跳过"));
                continue;
            }
        };
        if script.trim().is_empty() {
            out.skipped.push(format!("{shown}：稿件是空的，跳过"));
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let name = sanitize(stem);
        if let Some(existing) = out.items.iter().find(|i| i.name == name) {
            out.skipped.push(format!(
                "{shown}：工程名与 {} 撞了（都归一成「{name}」），跳过",
                display_path(&existing.path)
            ));
            continue;
        }
        out.items.push(BatchItem {
            name,
            path: path.clone(),
            script,
        });
    }
    out
}

fn display_path(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// 批量列表里一行稿子的状态（UI 列表用）。
///
/// 与任务台账的 `TaskState` 不是一回事：台账记的是"这一条任务"（排队/运行/终态），
/// 这里记的是"这一行稿件"（还没轮到 / 正在合成 / 已出片 / 出错 / 没跑）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    Waiting,
    Running,
    Done,
    Failed,
    Skipped,
}

impl ItemState {
    pub fn label(self) -> &'static str {
        match self {
            ItemState::Waiting => "待跑",
            ItemState::Running => "合成本",
            ItemState::Done => "已完成",
            ItemState::Failed => "失败",
            ItemState::Skipped => "已跳过",
        }
    }
}

/// 一条稿子跑完后，状态栏那一句汇总。`stopped` 是"用户中途停了整批"。
///
/// 口径：被停止时**已完成的仍然算完成**（产物已经在盘上），后续条目算没跑；
/// 失败与跳过分开数——"没跑成"和"压根没排上"在用户那里是两件事。
pub fn summary_text(done: usize, failed: usize, skipped: usize, stopped: bool) -> String {
    let mut parts = vec![format!("完成 {done} 篇")];
    if failed > 0 {
        parts.push(format!("失败 {failed} 篇"));
    }
    if skipped > 0 {
        parts.push(format!("跳过 {skipped} 篇"));
    }
    if stopped {
        parts.push("已停止：剩余篇目没有跑".to_string());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-batch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// 与生产同一个归一函数（`crate::file_stem`）；这里复用它才测得到"重名"那条。
    fn sanitize(name: &str) -> String {
        crate::file_stem(name)
    }

    #[test]
    fn imports_text_scripts_and_keeps_selection_order() {
        let dir = temp_dir("import");
        let a = write(&dir, "第一集.txt", "第一集正文。".as_bytes());
        let b = write(&dir, "第二集.txt", "第二集正文。".as_bytes());

        let out = import_scripts(&[a.clone(), b.clone()], sanitize);
        assert!(out.skipped.is_empty(), "不该有跳过：{:?}", out.skipped);
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.items[0].name, "第一集");
        assert_eq!(out.items[0].path, a);
        assert_eq!(out.items[0].script, "第一集正文。");
        assert_eq!(out.items[1].name, "第二集");
    }

    #[test]
    fn skips_empty_non_utf8_and_missing_with_actionable_notes() {
        let dir = temp_dir("skip");
        let empty = write(&dir, "空稿.txt", b"   \n\t ");
        let binary = write(&dir, "其实是音频.wav", &[0xff, 0xfe, 0x00, 0x01, 0x80]);
        let missing = dir.join("没这个文件.txt");
        let sub = dir.join("子目录.txt");
        std::fs::create_dir_all(&sub).unwrap();

        let out = import_scripts(&[empty, binary, missing, sub], sanitize);
        assert!(out.items.is_empty(), "一条都不该进来：{:?}", out.items);
        assert_eq!(out.skipped.len(), 4, "{:?}", out.skipped);
        assert!(out.skipped[0].contains("空稿.txt") && out.skipped[0].contains("空的"));
        assert!(out.skipped[1].contains("其实是音频.wav") && out.skipped[1].contains("UTF-8"));
        assert!(out.skipped[2].contains("没这个文件.txt") && out.skipped[2].contains("不是文件"));
        assert!(out.skipped[3].contains("子目录.txt") && out.skipped[3].contains("不是文件"));
    }

    /// 两个工程同名会写进同一个目录（后一个覆盖前一个的产物），必须在导入这一步拦住。
    #[test]
    fn skips_duplicate_project_names_and_says_which_one_won() {
        let dir = temp_dir("dup");
        let a = write(&dir, "口播.txt", "甲。".as_bytes());
        // 没有扩展名的同名稿件：file_stem 之后同样是「口播」
        let b = write(&dir, "口播", "乙。".as_bytes());

        let out = import_scripts(&[a.clone(), b], sanitize);
        assert_eq!(out.items.len(), 1, "{:?}", out.items);
        assert_eq!(out.items[0].path, a);
        assert_eq!(out.skipped.len(), 1);
        assert!(
            out.skipped[0].contains("口播") && out.skipped[0].contains("撞了"),
            "{:?}",
            out.skipped
        );
        assert!(
            out.skipped[0].contains("口播.txt"),
            "要说清是谁先占了这个名字：{:?}",
            out.skipped
        );
    }

    /// 列表文案是用户唯一看得见的状态，钉住它——写错一个字就会把"没跑"说成"失败"。
    #[test]
    fn item_state_labels_match_their_meaning() {
        assert_eq!(ItemState::Waiting.label(), "待跑");
        assert_eq!(ItemState::Running.label(), "合成本");
        assert_eq!(ItemState::Done.label(), "已完成");
        assert_eq!(ItemState::Failed.label(), "失败");
        assert_eq!(ItemState::Skipped.label(), "已跳过");
        // "跳过"与"失败"必须是两个词：排队中被取消不是错误
        assert_ne!(ItemState::Skipped.label(), ItemState::Failed.label());
    }

    #[test]
    fn summary_separates_failed_from_skipped_and_names_stop() {
        assert_eq!(summary_text(3, 0, 0, false), "完成 3 篇");
        assert_eq!(summary_text(2, 1, 0, false), "完成 2 篇 · 失败 1 篇");
        assert_eq!(
            summary_text(2, 1, 2, false),
            "完成 2 篇 · 失败 1 篇 · 跳过 2 篇"
        );
        let stopped = summary_text(1, 0, 2, true);
        assert!(stopped.contains("完成 1 篇"), "{stopped}");
        assert!(stopped.contains("跳过 2 篇"), "{stopped}");
        assert!(stopped.contains("已停止"), "{stopped}");
        assert!(stopped.contains("没有跑"), "停要说清后面的没跑：{stopped}");
    }
}
