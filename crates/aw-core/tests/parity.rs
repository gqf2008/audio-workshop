//! 与 Python 权威实现的**对照测试**：规则漂移的看门狗。
//!
//! 为什么要有：此前 8 项单测全是同规则 happy path（自己写期望值、自己判对错），
//! 没有一条与 Python 对照——规则一漂移（本次实测 20 例差异）无人发现，全靠临时工程
//! 才暴露。这里的数据 `parity_cases.txt` 由 **Python 实现现场生成**
//! （`tools/gen_parity_cases.py`），Rust 侧只负责逐条断言一致。
//!
//! 规则要改？先改 Python（权威），再 `python3 tools/gen_parity_cases.py`，最后让这里变绿。
//! 反着来会被这个测试挡住。

use aw_core::{split_sentences, verbalize};

const CASES: &str = include_str!("parity_cases.txt");

/// 还原生成脚本的转义（`\t` `\n` `\r` `\|` `\\`）
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('|') => out.push('|'),
            Some('\\') => out.push('\\'),
            other => panic!("未知转义: \\{other:?}"),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Section {
    Verbalize,
    Split {
        punctuation: String,
        max_chars: usize,
    },
}

impl Section {
    fn parse(header: &str) -> Section {
        let inner = header
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or_else(|| panic!("坏的分节头: {header}"));
        match inner.split('|').collect::<Vec<_>>().as_slice() {
            ["verbalize"] => Section::Verbalize,
            ["split", punct, max] => Section::Split {
                punctuation: punct.to_string(),
                max_chars: max.parse().expect("max_chars 应为整数"),
            },
            _ => panic!("未知分节: {header}"),
        }
    }

    /// 分节头形如 `[verbalize]` / `[split|。！？；…|12]`。
    /// 用例行必然含制表符，所以"以 [ 开头且不含制表符"才是头
    /// （用例里可以有以 `[` 开头的输入，如 `[1234567890123456]`）。
    fn is_header(line: &str) -> bool {
        line.starts_with('[') && !line.contains('\t')
    }
}

fn all_sections() -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for line in CASES.lines() {
        if Section::is_header(line) {
            let s = Section::parse(line);
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

fn cases(section: &Section) -> Vec<(String, String)> {
    let mut cur: Option<Section> = None;
    let mut out = Vec::new();
    for line in CASES.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if Section::is_header(line) {
            cur = Some(Section::parse(line));
            continue;
        }
        if cur.as_ref() == Some(section) {
            let (input, expected) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("用例行缺少制表符分隔: {line}"));
            out.push((unescape(input), unescape(expected)));
        }
    }
    out
}

/// 断言 Rust 侧与 Python 期望逐条一致，并把**全部**差异一次性报出来
/// （改一处规则时能同时看到它在别处造成了什么漂移）
fn assert_section_matches(section: &Section, run: impl Fn(&str) -> String) {
    let all = cases(section);
    let diffs: Vec<String> = all
        .iter()
        .map(|(input, expected)| (input, expected, run(input)))
        .filter(|(_, expected, actual)| *expected != actual)
        .map(|(input, expected, actual)| {
            format!("  输入: {input:?}\n    python: {expected:?}\n    rust  : {actual:?}")
        })
        .collect();
    assert!(
        diffs.is_empty(),
        "{section:?}: {} 条用例中 {} 条与 Python 不一致（期望值由 tools/gen_parity_cases.py 生成）:\n{}",
        all.len(),
        diffs.len(),
        diffs.join("\n")
    );
}

/// 用例文件本身不能退化：少于这个量级说明生成脚本或分节解析坏了
#[test]
fn fixture_is_not_degenerate() {
    let sections = all_sections();
    let total: usize = sections.iter().map(|s| cases(s).len()).sum();
    assert!(
        total >= 50,
        "parity_cases.txt 只有 {total} 条用例（分节 {}），请检查 tools/gen_parity_cases.py",
        sections.len()
    );
    assert!(
        sections.contains(&Section::Verbalize),
        "缺少 [verbalize] 分节"
    );
    assert!(
        sections.iter().any(|s| matches!(s, Section::Split { .. })),
        "缺少 [split|…] 分节"
    );
}

#[test]
fn verbalize_matches_python() {
    assert_section_matches(&Section::Verbalize, verbalize);
}

#[test]
fn split_sentences_matches_python() {
    let splits: Vec<Section> = all_sections()
        .into_iter()
        .filter(|s| matches!(s, Section::Split { .. }))
        .collect();
    assert!(!splits.is_empty(), "parity_cases.txt 里没有 split 分节");
    for section in splits {
        let Section::Split {
            punctuation,
            max_chars,
        } = section.clone()
        else {
            unreachable!()
        };
        assert_section_matches(&section, |t| {
            split_sentences(t, &punctuation, max_chars).join("|")
        });
    }
}
