//! 质检模块的 parity 看门狗：`aw_core::eval::normalize_numerals` 必须与
//! **Python 权威实现**（`tools/audio_eval.py::normalize_numerals`）逐条一致。
//!
//! 为什么单独一份夹具：文本层的 parity 用例来自 aw-eval 仓库的 authority，
//! 而质检工具 audio_eval.py 就在本仓库维护 —— 两者来源不同，混在一个文件里会说不清
//! "该改哪边"。规则要改？先改 `tools/audio_eval.py`（权威），再
//! `python3 tools/gen_eval_parity_cases.py`，最后让这里变绿。

use aw_core::eval::normalize_numerals;

const CASES: &str = include_str!("eval_parity_cases.txt");

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

#[test]
fn numerals_match_the_python_authority() {
    let mut checked = 0;
    for line in CASES.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let (input, expected) = line.split_once('\t').expect("用例格式: <输入>\\t<期望>");
        let (input, expected) = (unescape(input), unescape(expected));
        assert_eq!(
            normalize_numerals(&input),
            expected,
            "输入 {input:?} 与 Python 权威实现不一致"
        );
        checked += 1;
    }
    assert!(checked >= 15, "夹具太小（{checked} 条），别让漂移漏过");
}
