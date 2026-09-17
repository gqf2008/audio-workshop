//! 发音词典库（M4-P5）：多套词典的保存 / 切换 / 词条批量导入导出。
//!
//! 产品口径（docs/product-plan.md §4.2 P5）：词典抽屉里管理多套词典，支持词条的批量
//! 导入导出与切换；免费侧对照是"单套词典"。
//!
//! 现状：`aw_core::apply_dictionary` 早就有，但应用里三处调用全传空词典——桌面端的词典
//! 等于没接。这里把"库 + 导入导出"做起来，真正的生效在 `new_project_from_inputs` 的文本层。
//!
//! ```text
//! ~/Documents/音频作坊/dictionaries/
//!   口播专用-3ab41c7d.json    # {"name": "...", "entries": {"重庆": "崇庆"}}
//! ```
//!
//! 库内文件名的唯一性与路径安全照抄音色库复核实修的几条口径：slug + 名字哈希后缀、
//! 索引值只当文件名、读写前过白名单、软链拒绝。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 库目录名（与应用数据目录同级）。
pub const DICT_DIR: &str = "dictionaries";
/// 单个词条行的上限（字符数）：一行几千字的"词条"多半是误导入整段文本。
pub const MAX_ENTRY_CHARS: usize = 64;
/// 单次导入的文件体积上限。
pub const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024;

/// 一套词典。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Dictionary {
    pub name: String,
    #[serde(default)]
    pub entries: BTreeMap<String, String>,
    #[serde(default)]
    pub updated_at: u64,
}

/// 库内一条（列表用；不把词条全塞进 UI）。
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub file: String,
    pub count: usize,
    pub updated_at: u64,
}

/// 库目录。
pub fn library_dir(root: &Path) -> PathBuf {
    root.join(DICT_DIR)
}

/// 名字的 64 位 FNV-1a（与音色库同一套做法：同名稳定、不同名不撞文件）。
fn name_suffix(lower_name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in lower_name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 库内文件名必须是裸文件名（防手改索引/清单指向库外）。
pub fn file_is_safe(file: &str) -> bool {
    !file.is_empty()
        && !file.contains('/')
        && !file.contains('\\')
        && Path::new(file).components().count() == 1
}

/// 列出一套词典文件里的内容（校验文件名后读）。
pub fn load_file(root: &Path, file: &str) -> Result<Dictionary, String> {
    if !file_is_safe(file) {
        return Err(format!("词典文件名不合法：{file}"));
    }
    let path = library_dir(root).join(file);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| format!("词典读不出来：{}（{e}）", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!("词典不是普通文件：{}", path.display()));
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("词典读不出来：{}（{e}）", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("词典解析失败：{}（{e}）", path.display()))
}

/// 列出库里所有词典（按更新时间倒序）。坏文件跳过并计数（不静默当"没有词典"）。
pub fn list(root: &Path) -> (Vec<Entry>, usize) {
    let dir = library_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (Vec::new(), 0);
    };
    let mut rows: Vec<Entry> = Vec::new();
    let mut broken = 0usize;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(file) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            broken += 1;
            continue;
        };
        match load_file(root, &file) {
            Ok(d) => rows.push(Entry {
                name: d.name,
                file,
                count: d.entries.len(),
                updated_at: d.updated_at,
            }),
            Err(_) => broken += 1,
        }
    }
    rows.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.file.cmp(&a.file))
    });
    (rows, broken)
}

/// 保存一套词典（同名覆盖）：**写新文件 → 落盘成功 → 才清理旧文件**。
///
/// 顺序与音色库一致：索引/清单写失败时旧资产一个字节都不动，最多多一个哑文件。
pub fn save(
    root: &Path,
    name: &str,
    entries: BTreeMap<String, String>,
    now_ms: u64,
    sanitize: impl Fn(&str) -> String,
) -> Result<Entry, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("先给词典起个名字".into());
    }
    let dir = library_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("建词典库目录失败：{}（{e}）", dir.display()))?;
    let lower = name.to_lowercase();
    let base = format!("{}-{}.json", sanitize(name), &name_suffix(&lower)[..8]);
    let mut file = base.clone();
    let mut n = 2u32;
    while dir.join(&file).exists() {
        let stem = base.strip_suffix(".json").unwrap_or(&base);
        file = format!("{stem}-{n}.json");
        n += 1;
    }
    let dict = Dictionary {
        name: name.to_string(),
        entries,
        updated_at: now_ms,
    };
    let body = serde_json::to_vec_pretty(&dict).map_err(|e| format!("词典序列化失败：{e}"))?;
    let dst = dir.join(&file);
    aw_core::dub::write_atomic_explained(&dst, &body)
        .map_err(|e| format!("词典写入失败（{}）：{e}", dst.display()))?;

    // 清理同名旧文件（只删"库内、普通文件、名字合法"的那种）
    let (rows, _) = list(root);
    for old in rows.iter().filter(|r| r.name.eq_ignore_ascii_case(name)) {
        if old.file == file || !file_is_safe(&old.file) {
            continue;
        }
        let old_path = dir.join(&old.file);
        if let Ok(meta) = std::fs::symlink_metadata(&old_path) {
            if !meta.file_type().is_symlink() && meta.is_file() {
                let _ = std::fs::remove_file(&old_path);
            }
        }
    }
    Ok(Entry {
        name: name.to_string(),
        file,
        count: dict.entries.len(),
        updated_at: now_ms,
    })
}

/// 词条导入的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedEntry {
    pub from: String,
    pub to: String,
}

/// 导入结果：解析出的词条 + 被跳过的原因（每条都要说清第几行、为什么）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    pub entries: Vec<ParsedEntry>,
    pub skipped: Vec<String>,
}

/// 解析词条文件：每行 `词条<分隔>替换`，分隔支持 **Tab / 逗号 / 等号 / 全角等号**。
///
/// 规则（每条都要能对用户说清）：
/// - 去 BOM、统一 CRLF/CR 为 LF（Windows 导出的文件一定是 CRLF）；
/// - 空行、以 `#` 开头的注释行直接跳过（不计入"跳过原因"）；
/// - 没有分隔符 / 有一侧为空 / 词条超过 [`MAX_ENTRY_CHARS`] → 记一条跳过原因；
/// - 同一个"词条"出现多次：**后出现的覆盖前面的**，并记一条"重复"说明（词典本身就是 map）。
pub fn parse_entries(text: &str) -> ImportOutcome {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = ImportOutcome::default();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (i, raw_line) in body
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .enumerate()
    {
        let lineno = i + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((from, to)) = split_entry(line) else {
            out.skipped.push(format!(
                "第 {lineno} 行：认不出分隔符（用 Tab / 逗号 / 等号 隔开词条与替换）"
            ));
            continue;
        };
        let (from, to) = (from.trim(), to.trim());
        if from.is_empty() || to.is_empty() {
            out.skipped
                .push(format!("第 {lineno} 行：词条或替换为空，跳过"));
            continue;
        }
        if from.chars().count() > MAX_ENTRY_CHARS || to.chars().count() > MAX_ENTRY_CHARS {
            out.skipped.push(format!(
                "第 {lineno} 行：太长了（超过 {MAX_ENTRY_CHARS} 字），看着不像词条"
            ));
            continue;
        }
        if let Some(first) = seen.get(from) {
            out.skipped.push(format!(
                "第 {lineno} 行：词条「{from}」重复（第 {first} 行已定义），以本行为准"
            ));
        }
        seen.insert(from.to_string(), lineno);
        // 后出现的覆盖前面的
        if let Some(existing) = out.entries.iter_mut().find(|e| e.from == from) {
            existing.to = to.to_string();
        } else {
            out.entries.push(ParsedEntry {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
    }
    out
}

/// 取**行内最早出现**的分隔符切分。
///
/// 不能按固定优先级找（先 Tab 再 `=` 再逗号）：`重庆,桥=大桥` 这种"逗号分隔、替换里带等号"
/// 的行，按优先级会被切成 `重庆,桥` → `大桥`（复核指出的静默错位）。取最早位置才对得上
/// 用户的直觉：第一个出现的那个就是分隔符。
fn split_entry(line: &str) -> Option<(&str, &str)> {
    const SEPS: [char; 4] = ['\t', '=', '＝', ','];
    let (idx, sep) = line.char_indices().find(|(_, c)| SEPS.contains(c))?;
    let after = idx + sep.len_utf8();
    Some((&line[..idx], &line[after..]))
}

/// 从文件读词条（大小上限先按元数据判，别先读进内存）。
pub fn import_file(path: &Path) -> Result<ImportOutcome, String> {
    if !path.is_file() {
        return Err(format!("词条文件不存在或不可读：{}", path.display()));
    }
    match std::fs::metadata(path) {
        Ok(m) if m.len() > MAX_IMPORT_BYTES => {
            return Err(format!(
                "词条文件超过 {}MB，先拆分再导入：{}",
                MAX_IMPORT_BYTES / (1024 * 1024),
                path.display()
            ));
        }
        Ok(_) => {}
        Err(e) => return Err(format!("读不到词条文件属性：{e}")),
    }
    let raw = std::fs::read(path).map_err(|e| format!("词条文件读不出来：{e}"))?;
    let text = String::from_utf8(raw)
        .map_err(|_| format!("词条文件不是 UTF-8 文本：{}", path.display()))?;
    Ok(parse_entries(&text))
}

/// 导出成 TSV（词条<tab>替换），写到 `<dest>/dict-<名字>.tsv`。
pub fn export_tsv(
    root: &Path,
    file: &str,
    dest_dir: &Path,
    sanitize: impl Fn(&str) -> String,
) -> Result<PathBuf, String> {
    let dict = load_file(root, file)?;
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("建导出目录失败：{}（{e}）", dest_dir.display()))?;
    let mut body = String::from("# 词条<Tab>替换；空行与 # 开头会被导入时忽略\n");
    for (k, v) in &dict.entries {
        body.push_str(&format!("{k}\t{v}\n"));
    }
    // 与库内文件名同一个道理：只归一化名字会让 `a/b` 与 `a_b` 两套词典导到同一个文件
    let path = dest_dir.join(format!(
        "dict-{}-{}.tsv",
        sanitize(&dict.name),
        &name_suffix(&dict.name.to_lowercase())[..8]
    ));
    aw_core::dub::write_atomic_explained(&path, body.as_bytes())
        .map_err(|e| format!("词条导出失败（{}）：{e}", path.display()))?;
    Ok(path)
}

/// 词条映射 → 指纹（写进工程，用于"换了词典就不复用旧音频"）。
pub fn fingerprint(entries: &BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (k, v) in entries {
        // **长度前缀**：直接拼 `k=v\n` 会让 {"a=b":"c"} 与 {"a":"b=c"} 产生同一串字节、
        // 同一个指纹 —— 换词典时就可能错误复用旧音频（复核指出）。
        h.update(format!("{}:", k.len()).as_bytes());
        h.update(k.as_bytes());
        h.update(b"\0");
        h.update(format!("{}:", v.len()).as_bytes());
        h.update(v.as_bytes());
        h.update(b"\0");
    }
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize(name: &str) -> String {
        crate::file_stem(name)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aw-dict-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn save_list_load_roundtrip() {
        let root = temp_dir("roundtrip");
        let e = save(
            &root,
            "口播专用",
            entries(&[("重庆", "崇庆"), ("2024", "二零二四")]),
            100,
            sanitize,
        )
        .unwrap();
        assert!(
            e.file.starts_with("口播专用-") && e.file.ends_with(".json"),
            "{}",
            e.file
        );

        let (rows, broken) = list(&root);
        assert_eq!(broken, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "口播专用");
        assert_eq!(rows[0].count, 2);

        let d = load_file(&root, &e.file).unwrap();
        assert_eq!(d.entries.get("重庆").map(String::as_str), Some("崇庆"));
        assert_eq!(d.updated_at, 100);
    }

    #[test]
    fn same_name_overwrite_writes_new_file_and_keeps_one_row() {
        let root = temp_dir("overwrite");
        let a = save(&root, "甲", entries(&[("a", "1")]), 1, sanitize).unwrap();
        let b = save(&root, "甲", entries(&[("b", "2")]), 2, sanitize).unwrap();
        assert_ne!(a.file, b.file, "每次保存写新文件，旧的不被提前改写");
        let (rows, broken) = list(&root);
        assert_eq!(broken, 0);
        assert_eq!(rows.len(), 1, "同名只留一条：{rows:?}");
        assert_eq!(rows[0].file, b.file);
        assert_eq!(rows[0].count, 1);
    }

    #[test]
    fn broken_and_foreign_files_are_counted_not_listed() {
        let root = temp_dir("broken");
        save(&root, "好的", entries(&[("a", "1")]), 1, sanitize).unwrap();
        std::fs::write(library_dir(&root).join("坏的.json"), b"{ not json").unwrap();
        std::fs::write(library_dir(&root).join("README.txt"), b"x").unwrap();
        let (rows, broken) = list(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(broken, 1, "坏文件要计数");
    }

    #[test]
    fn load_refuses_unsafe_file_names_and_symlinks() {
        let root = temp_dir("unsafe");
        std::fs::create_dir_all(library_dir(&root)).unwrap();
        for bad in ["../x.json", "/etc/passwd", "", "a/b.json"] {
            assert!(load_file(&root, bad).is_err(), "非法名必须拒绝：{bad}");
        }
        #[cfg(unix)]
        {
            let outside = root.join("外面的.json");
            std::fs::write(&outside, b"{}").unwrap();
            std::os::unix::fs::symlink(&outside, library_dir(&root).join("伪装.json")).unwrap();
            assert!(load_file(&root, "伪装.json").is_err(), "软链必须拒绝");
        }
    }

    #[test]
    fn parse_handles_bom_crlf_comments_and_three_separators() {
        let text = "\u{feff}# 注释\r\n重庆\t崇庆\r\n单于,蝉于\r\n银行=很行\r\n\r\n";
        let out = parse_entries(text);
        assert!(out.skipped.is_empty(), "{:?}", out.skipped);
        assert_eq!(out.entries.len(), 3);
        assert_eq!(out.entries[0].from, "重庆");
        assert_eq!(out.entries[0].to, "崇庆");
        assert_eq!(out.entries[1].from, "单于");
        assert_eq!(out.entries[2].to, "很行");
    }

    /// 分隔符取"最早出现"的那个：逗号分隔的行里，替换文本自带 `=` 不该被当成分隔符。
    #[test]
    fn parse_uses_the_earliest_separator() {
        let out = parse_entries("重庆,桥=大桥\n");
        assert!(out.skipped.is_empty(), "{:?}", out.skipped);
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].from, "重庆");
        assert_eq!(out.entries[0].to, "桥=大桥", "替换里的 = 要保留");

        // 反过来：等号在前、逗号在后 → 等号是分隔符
        let out = parse_entries("重庆=桥,大桥\n");
        assert_eq!(out.entries[0].from, "重庆");
        assert_eq!(out.entries[0].to, "桥,大桥");
    }

    #[test]
    fn parse_reports_each_skip_with_line_number_and_lets_later_win() {
        let text = "好的一行\t替换\n没有分隔符\n空替换=\n重庆\t崇庆\n重庆\t重庆\n";
        let out = parse_entries(text);
        assert_eq!(out.entries.len(), 2, "{:?}", out.entries);
        assert_eq!(
            out.entries.iter().find(|e| e.from == "重庆").unwrap().to,
            "重庆",
            "重复词条后出现的覆盖前面的"
        );
        assert_eq!(out.skipped.len(), 3, "{:?}", out.skipped);
        assert!(out.skipped[0].contains("第 2 行") && out.skipped[0].contains("分隔符"));
        assert!(out.skipped[1].contains("第 3 行"));
        assert!(out.skipped[2].contains("第 5 行") && out.skipped[2].contains("重复"));
    }

    #[test]
    fn parse_rejects_overlong_entries() {
        let long = "词".repeat(MAX_ENTRY_CHARS + 1);
        let out = parse_entries(&format!("{long}\t替换\n正常的\t替换\n"));
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].contains("太长"), "{:?}", out.skipped);
    }

    #[test]
    fn import_file_has_size_cap_and_utf8_check() {
        let root = temp_dir("import-file");
        let big = root.join("太大.tsv");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_IMPORT_BYTES + 1).unwrap();
        drop(f);
        let err = import_file(&big).unwrap_err();
        assert!(err.contains("超过"), "{err}");

        let binary = root.join("不是文本.tsv");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        let err = import_file(&binary).unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");

        let ok = root.join("词条.tsv");
        std::fs::write(&ok, "重庆\t崇庆\n").unwrap();
        let out = import_file(&ok).unwrap();
        assert_eq!(out.entries.len(), 1);
    }

    #[test]
    fn export_writes_tsv_that_can_be_imported_back() {
        let root = temp_dir("export");
        let e = save(
            &root,
            "口播专用",
            entries(&[("重庆", "崇庆"), ("单于", "蝉于")]),
            1,
            sanitize,
        )
        .unwrap();
        let out = root.join("导出的词条");
        let path = export_tsv(&root, &e.file, &out, sanitize).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("dict-口播专用-") && name.ends_with(".tsv"),
            "导出名 = slug + 名字哈希后缀：{name}"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let back = parse_entries(&text);
        assert!(back.skipped.is_empty(), "{:?}", back.skipped);
        assert_eq!(back.entries.len(), 2, "往返后词条数一致");
        let map: BTreeMap<String, String> = back
            .entries
            .iter()
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();
        assert_eq!(map.get("重庆").map(String::as_str), Some("崇庆"));
    }

    /// 指纹的长度前缀反例：`{"a=b":"c"}` 与 `{"a":"b=c"}` 是两套不同词典，指纹必须不同。
    #[test]
    fn fingerprint_has_no_delimiter_collision() {
        let a = fingerprint(&entries(&[("a=b", "c")]));
        let b = fingerprint(&entries(&[("a", "b=c")]));
        assert_ne!(a, b, "不同的词典映射不能有同一个指纹");
    }

    /// 两套名字归一后相同的词典：导出文件名也要区分开（否则互相覆盖）。
    #[test]
    fn export_file_names_do_not_collide_after_sanitize() {
        let root = temp_dir("export-collision");
        let a = save(&root, "a/b", entries(&[("x", "1")]), 1, sanitize).unwrap();
        let b = save(&root, "a_b", entries(&[("y", "2")]), 2, sanitize).unwrap();
        let out = root.join("out");
        let pa = export_tsv(&root, &a.file, &out, sanitize).unwrap();
        let pb = export_tsv(&root, &b.file, &out, sanitize).unwrap();
        assert_ne!(pa, pb, "两套词典不能导到同一个文件");
        assert!(pa.is_file() && pb.is_file());
        assert!(std::fs::read_to_string(&pa).unwrap().contains("x\t1"));
        assert!(std::fs::read_to_string(&pb).unwrap().contains("y\t2"));
    }

    #[test]
    fn fingerprint_changes_with_entries_and_is_stable_otherwise() {
        let a = fingerprint(&entries(&[("a", "1")]));
        assert_eq!(a, fingerprint(&entries(&[("a", "1")])), "同样内容稳定");
        assert_ne!(a, fingerprint(&entries(&[("a", "2")])), "替换变了要变");
        assert_ne!(
            a,
            fingerprint(&entries(&[("a", "1"), ("b", "2")])),
            "加词条要变"
        );
        assert_ne!(a, fingerprint(&BTreeMap::new()), "有词条与空词典不能同指纹");
    }
}
