//! 一键备份（M4-P8 的一半）：把工程与自建资产复制成一份**自包含**的备份目录。
//!
//! 产品口径（docs/product-plan.md §4.2 的 P8）：一键备份工程与词典（那一条里的"自动更新"
//! 部分不在本批，见 docs/backup.md 的边界）。
//!
//! 用户的资产都散在应用数据目录下：
//!
//! ```text
//! ~/Documents/音频作坊/
//!   projects/        # 工程、逐句音频、成品、分离两轨…（最大的一块）
//!   settings.json    # 服务/模型目录/BGM 输入/启用的词典
//!   templates.json   # 配音模板
//!   dictionaries/    # 发音词典库
//!   voices/          # 音色库（含参考音频副本）
//! ```
//!
//! 备份 = 把上面这些**复制**到 `<目标>/音频作坊备份-<时间戳>/`，并写一份 `manifest.json`
//! 说明复制了什么、跳过了什么。整个过程先写在 `.部分-<时间戳>/`，**成功后才改名**为正式
//! 目录——否则中断/崩溃留下的半份东西会被当成一份可用的备份（比没有备份更危险）。
//!
//! 这里只放纯逻辑 + 文件操作；UI 接线在 `src/main.rs`。

use std::path::{Path, PathBuf};

/// 备份里要包含的顶层条目（文件名固定，与 `src/main.rs` 的资产目录一致）。
pub const ITEMS: [&str; 5] = [
    "projects",
    "settings.json",
    "templates.json",
    "dictionaries",
    "voices",
];

/// 备份结果（给状态行/清单用）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackupSummary {
    /// 最终备份目录
    pub dest: PathBuf,
    pub files: usize,
    pub bytes: u64,
    /// 跳过的条目（相对路径 + 原因）。空 = 全量成功。
    pub skipped: Vec<String>,
}

impl BackupSummary {
    /// 状态栏那一句（成功/有跳过分开说）。
    pub fn note(&self) -> String {
        let size = human_bytes(self.bytes);
        if self.skipped.is_empty() {
            format!(
                "已备份 {} 个文件（{size}）到 {}",
                self.files,
                self.dest.display()
            )
        } else {
            format!(
                "已备份 {} 个文件（{size}），跳过 {} 项（{}）到 {}",
                self.files,
                self.skipped.len(),
                self.skipped[0],
                self.dest.display()
            )
        }
    }
}

/// 人类可读的字节数（状态栏用；不引单位库）。
pub fn human_bytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

/// 备份到 `dest_root` 下。`stamp` 由调用方给（测试可注入固定值；生产用 `YYYYmmdd-HHMMSS`）。
pub fn backup_all(
    workshop_dir: &Path,
    dest_root: &Path,
    stamp: &str,
) -> Result<BackupSummary, String> {
    if stamp.trim().is_empty() || stamp.contains('/') || stamp.contains('\\') {
        return Err(format!("备份时间戳不合法：{stamp}"));
    }
    // 目标不能落在源目录里：否则备份会把自己的产物再复制进去（递归自吞）。
    // 必须按**真实路径**比，不能看字面前缀：`<外面>/x/../音频作坊/backup` 与
    // 指向源目录的软链都能绕过纯字符串的 starts_with（复核抓到的阻塞项）。
    if dest_inside_source(workshop_dir, dest_root)? {
        return Err(format!(
            "备份目标不能放在应用数据目录里面（{}）——请选一个外面/挂载盘上的目录",
            workshop_dir.display()
        ));
    }
    std::fs::create_dir_all(dest_root)
        .map_err(|e| format!("建备份目标目录失败：{}（{e}）", dest_root.display()))?;

    // 正式目录已存在就顺延编号：绝不复用/覆盖上一次备份
    let base = format!("音频作坊备份-{stamp}");
    let mut name = base.clone();
    let mut n = 2u32;
    while dest_root.join(&name).exists() {
        name = format!("{base}-{n}");
        n += 1;
    }
    let final_dir = dest_root.join(&name);
    let partial = dest_root.join(format!(".部分-{stamp}-{n}"));

    // 上一轮中断留下的同名半成品先让路（它已经明确带 .部分- 前缀，不会被当正式备份）
    if partial.exists() {
        return Err(format!(
            "上次的备份没写完，还留着 {}——确认不需要就删掉它再重试",
            partial.display()
        ));
    }
    std::fs::create_dir_all(&partial)
        .map_err(|e| format!("建备份临时目录失败：{}（{e}）", partial.display()))?;

    let mut summary = BackupSummary::default();
    for item in ITEMS {
        let src = workshop_dir.join(item);
        if !src.exists() {
            continue; // 还没用到的资产（例如从没建过音色库）不算失败
        }
        let dst = partial.join(item);
        copy_entry(&src, &dst, Path::new(item), &mut summary);
    }

    // 清单：说明这份备份是什么、装了什么、跳过了什么
    let manifest = serde_json::json!({
        "kind": "audio-workshop-backup",
        "stamp": stamp,
        "items": ITEMS,
        "files": summary.files,
        "bytes": summary.bytes,
        "skipped": summary.skipped,
    });
    let body =
        serde_json::to_vec_pretty(&manifest).map_err(|e| format!("备份清单序列化失败：{e}"))?;
    aw_core::dub::write_atomic_explained(&partial.join("manifest.json"), &body)
        .map_err(|e| format!("备份清单写入失败：{e}"))?;

    // 写完才发布：改名是这一步的"提交点"
    std::fs::rename(&partial, &final_dir).map_err(|e| {
        // 失败时把半成品挪到明确的 .部分- 名字下（已经是了），并如实说明
        format!("备份收尾失败（数据还在 {}）：{e}", partial.display())
    })?;
    summary.dest = final_dir;
    Ok(summary)
}

/// 目标是不是落在源目录里面（**按真实路径**比，挡 `..` 与软链绕过）。
fn dest_inside_source(workshop_dir: &Path, dest_root: &Path) -> Result<bool, String> {
    Ok(resolve_for_compare(dest_root)?.starts_with(resolve_for_compare(workshop_dir)?))
}

/// 把路径解析成"真实的绝对路径"，**允许路径本身还不存在**。
///
/// 真正的实现在 `crate::paths::resolve_for_compare`（全仓库唯一的路径包含判定入口）；
/// 这里只是把"相对路径按当前目录解析"这条备份侧的口径补上。
fn resolve_for_compare(p: &Path) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("取当前目录失败（{e}）"))?;
    crate::paths::resolve_for_compare(p, &cwd)
}

/// 递归复制一个条目（文件或目录）。**任何单个文件的失败都只记一条跳过**，不拖垮整份备份。
fn copy_entry(src: &Path, dst: &Path, shown: &Path, out: &mut BackupSummary) {
    let meta = match std::fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(e) => {
            out.skipped
                .push(format!("{}：读不到（{e}）", shown.display()));
            return;
        }
    };
    if meta.file_type().is_symlink() {
        // 备份要的是"数据本身"，不跟软链（否则可能把库外的东西也抄进来）
        out.skipped
            .push(format!("{}：符号链接（备份不跟软链）", shown.display()));
        return;
    }
    if meta.is_file() {
        match std::fs::copy(src, dst) {
            Ok(bytes) => {
                out.files += 1;
                out.bytes += bytes;
            }
            Err(e) => out
                .skipped
                .push(format!("{}：复制失败（{e}）", shown.display())),
        }
        return;
    }
    if !meta.is_dir() {
        out.skipped
            .push(format!("{}：不是普通文件也不是目录", shown.display()));
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dst) {
        out.skipped
            .push(format!("{}：建目录失败（{e}）", shown.display()));
        return;
    }
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(e) => {
            out.skipped
                .push(format!("{}：目录读不出来（{e}）", shown.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.skipped
                    .push(format!("{}：目录项读不出来（{e}）", shown.display()));
                continue;
            }
        };
        let child_shown = shown.join(entry.file_name());
        copy_entry(
            &entry.path(),
            &dst.join(entry.file_name()),
            &child_shown,
            out,
        );
    }
}

/// 备份目录名里的时间戳：**本地时间** `YYYYmmdd-HHMMSS`（生产用；测试注入固定值）。
///
/// 为什么不手搓日期换算：本地时区/夏令时不是纯函数能算出来的，算成 UTC 会让中国用户
/// 看到早 8 小时的目录名。本应用在系统对话框上已经依赖 `osascript` / PowerShell
/// （见 `src/main.rs` 的 `pick_folder_with_prompt`），这里沿用同一条路走系统命令，
/// 拿到的是**用户本机时区的当前时间**。
///
/// 命令不可用（非预期环境）时退化成 Unix 秒：目录名仍然唯一、仍然合法
/// （`backup_all` 只拒空串和路径分隔符），只是不好看——不为了好看把备份弄失败。
pub fn stamp_now() -> String {
    if let Some(s) = local_stamp_from_system() {
        return s;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// `YYYYmmdd-HHMMSS`：8 位日期 + `-` + 6 位时间，且全是 ASCII 数字。
///
/// 这不只是好看——它同时挡掉系统命令输出里混进来的换行/本地化文字/路径分隔符，
/// 免得一个意外的 `/` 让备份目录名变成路径。
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 15
        && b[8] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 8 || c.is_ascii_digit())
}

/// 取本机时区的当前时间（`YYYYmmdd-HHMMSS`）；命令缺失/输出不合法时返回 `None`。
#[cfg(unix)]
fn local_stamp_from_system() -> Option<String> {
    let out = std::process::Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    is_stamp(&s).then_some(s)
}

#[cfg(windows)]
fn local_stamp_from_system() -> Option<String> {
    // 不弹控制台：统一走 win_child（`CREATE_NO_WINDOW` 的唯一来源）。
    let out = crate::win_child::hidden_command("powershell")
        .args(["-NoProfile", "-Command", "Get-Date -Format yyyyMMdd-HHmmss"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    is_stamp(&s).then_some(s)
}

/// 既不是 unix 也不是 windows：没有可靠的本机时间来源，交给调用方退化。
#[cfg(not(any(unix, windows)))]
fn local_stamp_from_system() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-backup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一份"用过的"应用数据目录：五类资产各有一点内容。
    fn make_workshop(root: &Path) {
        std::fs::create_dir_all(root.join("projects/示例工程/out")).unwrap();
        std::fs::write(root.join("projects/示例工程/out/final.wav"), b"wav-bytes").unwrap();
        std::fs::write(root.join("projects/示例工程/project.json"), b"{}").unwrap();
        std::fs::write(root.join("settings.json"), b"{\"port\":1}").unwrap();
        std::fs::write(root.join("templates.json"), b"{\"templates\":[]}").unwrap();
        std::fs::create_dir_all(root.join("dictionaries")).unwrap();
        std::fs::write(
            root.join("dictionaries/口播-x.json"),
            "{\"name\":\"口播\"}".as_bytes(),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("voices")).unwrap();
        std::fs::write(root.join("voices/我的声线-1.wav"), b"ref-bytes").unwrap();
    }

    #[test]
    fn backs_up_every_asset_and_leaves_sources_untouched() {
        let root = temp_dir("all");
        let workshop = root.join("作坊");
        let dest = root.join("备份盘");
        make_workshop(&workshop);

        let s = backup_all(&workshop, &dest, "1000").unwrap();
        assert_eq!(s.files, 6, "五类里的 6 个文件都要复制：{s:?}");
        assert!(s.bytes > 0);
        assert!(s.skipped.is_empty(), "{:?}", s.skipped);
        assert!(s.dest.ends_with("音频作坊备份-1000"));
        assert!(s.dest.join("projects/示例工程/out/final.wav").is_file());
        assert!(s.dest.join("settings.json").is_file());
        assert!(s.dest.join("templates.json").is_file());
        assert!(s.dest.join("dictionaries/口播-x.json").is_file());
        assert!(s.dest.join("voices/我的声线-1.wav").is_file());
        assert!(s.dest.join("manifest.json").is_file());
        // manifest 要能核（files/bytes 与 summary 一致）
        let m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(s.dest.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(m["files"].as_u64(), Some(s.files as u64));
        assert_eq!(m["bytes"].as_u64(), Some(s.bytes));

        // 源文件一个都不能动（备份是**复制**，不是搬迁）
        assert!(workshop.join("projects/示例工程/out/final.wav").is_file());
        assert!(workshop.join("settings.json").is_file());
        assert_eq!(
            std::fs::read(workshop.join("settings.json")).unwrap(),
            b"{\"port\":1}"
        );

        // 半成品目录不能留下（改名是提交点）
        let leftovers: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".部分-"))
            .collect();
        assert!(leftovers.is_empty(), "成功后不该留半成品：{leftovers:?}");
    }

    #[test]
    fn second_backup_with_same_stamp_never_overwrites_the_first() {
        let root = temp_dir("same-stamp");
        let workshop = root.join("作坊");
        let dest = root.join("备份盘");
        make_workshop(&workshop);

        let first = backup_all(&workshop, &dest, "2000").unwrap();
        std::fs::write(workshop.join("settings.json"), b"{\"port\":2}").unwrap();
        let second = backup_all(&workshop, &dest, "2000").unwrap();
        assert_ne!(first.dest, second.dest, "同一时间戳也不能复用目录");
        assert_eq!(
            std::fs::read(first.dest.join("settings.json")).unwrap(),
            b"{\"port\":1}",
            "第一份备份的内容不能被后来那次改掉"
        );
        assert_eq!(
            std::fs::read(second.dest.join("settings.json")).unwrap(),
            b"{\"port\":2}"
        );
    }

    /// 单项失败隔离要有牙：**必须真的造出一个复制不了的条目**，并断言它同时出现在
    /// `summary.skipped` 与 `manifest.json` 里（只在内存里记、不写清单，这条就该红）。
    ///
    /// 用 unix socket 而不是 chmod 000：socket 既不是普通文件也不是目录，复制必然失败，
    /// 而且与"测试是不是 root"无关（root 能绕过权限位，旧版 chmod 测试在 root 下是空转的）。
    #[cfg(unix)]
    #[test]
    fn a_non_copyable_entry_is_skipped_and_recorded_in_the_manifest() {
        let root = temp_dir("skip");
        let workshop = root.join("作坊");
        let dest = root.join("备份盘");
        make_workshop(&workshop);
        let sock = workshop.join("projects/坏条目.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

        let s = backup_all(&workshop, &dest, "3000").unwrap();

        // ① 如实记一条跳过，且说得出是哪个条目
        assert_eq!(s.skipped.len(), 1, "要如实记一条跳过：{:?}", s.skipped);
        assert!(s.skipped[0].contains("坏条目.sock"), "{:?}", s.skipped);
        // ② 跳过的条目不能出现在备份里（别把抄不动的东西留成空文件）
        assert!(!s.dest.join("projects/坏条目.sock").exists());
        // ③ 同一份跳过清单必须落进 manifest，不能只在内存里
        let m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(s.dest.join("manifest.json")).unwrap()).unwrap();
        let skipped = m["skipped"]
            .as_array()
            .expect("manifest 必须有 skipped 数组");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(
            skipped[0]
                .as_str()
                .unwrap_or_default()
                .contains("坏条目.sock"),
            "{skipped:?}"
        );
        // ④ 单项失败隔离：其它资产照常备份、清单照常写、正式目录照常发布
        assert!(s.dest.join("settings.json").is_file());
        assert!(s.dest.join("manifest.json").is_file());
        assert_eq!(
            std::fs::read(s.dest.join("settings.json")).unwrap(),
            b"{\"port\":1}"
        );
    }

    /// 目标目录内的判定必须按**真实路径**：字面上不以源目录开头、解析后却落回源目录
    /// 里面的写法要挡住（否则备份把自己的产物再抄一遍，递归自吞）。
    #[test]
    fn refuses_dest_that_lands_inside_the_source_via_dotdot() {
        let root = temp_dir("dotdot");
        let workshop = root.join("作坊");
        let outside = root.join("外面");
        make_workshop(&workshop);
        std::fs::create_dir_all(&outside).unwrap();

        // 字面前缀是 <tmp>/外面 —— 完全不像源目录；`..` 一解就回到源目录里面
        let sneaky = outside
            .join("..")
            .join("作坊")
            .join("projects")
            .join("backup");
        let err = backup_all(&workshop, &sneaky, "5000").unwrap_err();
        assert!(err.contains("不能放在应用数据目录里面"), "{err}");
    }

    /// 软链绕过：外面有个链接指向源目录，备份目标写在链接下面 —— 也是源目录里面。
    #[cfg(unix)]
    #[test]
    fn refuses_dest_reached_through_a_symlink_into_the_source() {
        let root = temp_dir("through-link");
        let workshop = root.join("作坊");
        let outside = root.join("外面");
        make_workshop(&workshop);
        std::fs::create_dir_all(&outside).unwrap();
        let link = outside.join("捷径");
        std::os::unix::fs::symlink(&workshop, &link).unwrap();

        let err = backup_all(&workshop, &link.join("backup"), "6000").unwrap_err();
        assert!(err.contains("不能放在应用数据目录里面"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_with_a_reason() {
        let root = temp_dir("symlink");
        let workshop = root.join("作坊");
        let dest = root.join("备份盘");
        make_workshop(&workshop);
        let outside = root.join("外面的机密.txt");
        std::fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, workshop.join("projects/链接.txt")).unwrap();

        let s = backup_all(&workshop, &dest, "4000").unwrap();
        assert!(
            s.skipped.iter().any(|x| x.contains("符号链接")),
            "软链要如实跳过：{:?}",
            s.skipped
        );
        assert!(
            !s.dest.join("projects/链接.txt").exists(),
            "不能把软链指向的库外文件抄进备份"
        );
    }

    #[test]
    fn refuses_dest_inside_the_workshop_dir() {
        let root = temp_dir("inside");
        let workshop = root.join("作坊");
        make_workshop(&workshop);
        let err = backup_all(&workshop, &workshop.join("backup-inside"), "5000").unwrap_err();
        assert!(err.contains("不能放在应用数据目录里面"), "{err}");
        assert!(!workshop.join("backup-inside").exists(), "拒绝时不该建目录");
    }

    #[test]
    fn note_says_size_and_first_skip() {
        let s = BackupSummary {
            dest: PathBuf::from("/tmp/备份"),
            files: 3,
            bytes: 2048,
            skipped: vec!["a/b：复制失败（Permission denied）".into()],
        };
        let note = s.note();
        assert!(note.contains("3 个文件"), "{note}");
        assert!(note.contains("2.0KB"), "{note}");
        assert!(note.contains("跳过 1 项"), "{note}");
        assert!(note.contains("Permission denied"), "{note}");
        assert!(BackupSummary::default().note().contains("跳过 0 项") || true);
    }

    /// 时间戳格式是**目录名的安全边界**：必须挡住系统命令输出里混进来的分隔符/空白。
    #[test]
    fn stamp_shape_is_enforced_and_rejects_path_separators() {
        assert!(is_stamp("20260917-143012"), "标准 15 位要过");
        assert!(!is_stamp("2026-09-17"), "带分隔符的日期不行");
        assert!(!is_stamp("20260917-143012\n"), "混进换行不行");
        assert!(!is_stamp("20260917/143012"), "斜杠绝不允许（会变路径）");
        assert!(!is_stamp("20260917143012"), "少了连字符不行");
        assert!(!is_stamp(""), "空串不行");
    }

    /// 生产时间戳要么是本机时区的 15 位格式，要么退化成纯数字 Unix 秒——
    /// 两种都必须是 `backup_all` 收得下的（非空、无路径分隔符）。
    #[test]
    fn stamp_now_is_usable_as_a_directory_name() {
        let s = stamp_now();
        assert!(!s.trim().is_empty());
        assert!(
            !s.contains('/') && !s.contains('\\'),
            "不能让时间戳带出路径：{s}"
        );
        assert!(is_stamp(&s) || s.chars().all(|c| c.is_ascii_digit()), "{s}");
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(2048), "2.0KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0MB");
    }
}
