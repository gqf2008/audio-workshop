//! 文本兜底层：数字读法规范化 + 发音词典。
//!
//! **权威实现在 Python 侧**（tools/audio_config.py 的 `verbalize()` / `cardinal()`）：
//! 上游 audio8_tts 未接文本规范化，裸数字直接进声学模型会导致读法不稳定（实测同一句
//! 三轮可懂度 83.3/98.1/94.4）。本模块是它的逐条对齐移植。
//!
//! 「对齐」不靠人眼，靠 `tests/parity_cases.txt`：那里的期望值由 **Python 实现现场
//! 生成**（`tools/gen_parity_cases.py`），本模块改规则必须同步重跑生成脚本，否则
//! parity 测试立刻变红——上一版就是因为单测全是同规则 happy path，规则漂移无人发现。
//!
//! 已经踩过的坑（parity 用例里都有正反例）：
//! 1. **不能无脑删逗号**：只删真千分位（`共3,4人` 的逗号是句读，删掉就变"共三十四人"）
//! 2. **数字两侧不能挨着拉丁字母/下划线**：否则 `iPhone15`/`MP3`/`A4纸`/`ISO9001`
//!    会被读成 iPhone十五 / MP三 / A四纸
//! 3. **12 位以上逐位读**（订单号/卡号不是数量），且分组单位要补齐 万亿/亿亿——
//!    上一版单位表只到「亿」，13 位（4 组）就索引越界 panic（pub API，UI 一调即崩）
//! 4. `2026 年` / `30 %` / `1234.56 元` 中间的空格要被吞掉，否则读成"二零二六 年"

use std::collections::BTreeMap;

const CN: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
const UNITS: [&str; 4] = ["", "十", "百", "千"];
/// 万级单位表：与「四位一组」的组数对应（最多 5 组 = 20 位，u64 上限 20 位也在此表内）。
/// Python 侧 `_BIG` 同表；缺项会 panic，不是"读得难听"这么轻。
const BIG: [&str; 5] = ["", "万", "亿", "万亿", "亿亿"];

/// 1234 → 一千二百三十四；10086 → 一万零八十六；17 → 十七
pub fn cardinal(n: u64) -> String {
    cardinal_digits(&n.to_string())
}

/// 数字串 → 基数词。与 Python `cardinal()` 同一算法。
///
/// - 前导零按数值处理（`007` → 七，等价 Python `int()`）
/// - 超过单位表上限（>20 位）时退化为**逐位读**：这类串本就是订单号/卡号，
///   逐位是唯一合理读法。Python 侧此处会 IndexError，Rust 侧绝不允许
///   panic，也绝不允许 `parse().unwrap_or(0)` 那样静默读成「零」
fn cardinal_digits(digits: &str) -> String {
    let d = digits.trim_start_matches('0');
    if d.is_empty() {
        return "零".into();
    }
    let bytes = d.as_bytes();
    let mut groups: Vec<u32> = Vec::new();
    let mut end = bytes.len();
    while end > 0 {
        let start = end.saturating_sub(4);
        groups.push(
            bytes[start..end]
                .iter()
                .fold(0u32, |acc, b| acc * 10 + u32::from(b - b'0')),
        );
        end = start;
    }
    if groups.len() > BIG.len() {
        return digits_zh(d, false);
    }

    fn under_10000(x: u32) -> String {
        let mut out = String::new();
        let mut zero = false;
        for i in (0..4).rev() {
            let d = (x / 10u32.pow(i)) % 10;
            if d == 0 {
                zero = true;
                continue;
            }
            if zero && !out.is_empty() {
                out.push('零');
            }
            zero = false;
            if d == 1 && i == 1 && out.is_empty() {
                out.push('十'); // 十七 而非 一十七
            } else {
                out.push_str(CN[d as usize]);
                out.push_str(UNITS[i as usize]);
            }
        }
        out
    }

    let mut out = String::new();
    for (i, g) in groups.iter().enumerate().rev() {
        if *g == 0 {
            if !out.is_empty() && !out.ends_with('零') {
                out.push('零');
            }
            continue;
        }
        if !out.is_empty() && *g < 1000 && !out.ends_with('零') {
            out.push('零'); // 组间补零：一万零八十六
        }
        out.push_str(&under_10000(*g));
        out.push_str(BIG[i]);
    }
    let out = out.trim_end_matches('零');
    if out.is_empty() {
        "零".into()
    } else {
        out.to_string()
    }
}

/// 逐位读；telephone=true 时首位读「幺」
pub fn digits_zh(s: &str, telephone: bool) -> String {
    s.chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| {
            if telephone && d == 1 {
                "幺".to_string()
            } else {
                CN[d as usize].to_string()
            }
        })
        .collect()
}

/// 千分位：只删真的（`(?<=\d),(?=\d{3}(?![0-9A-Za-z_]))`）。
///
/// 判定要"前一位是数字、后三位是数字且其后不再跟数字/字母/下划线"：
/// `1,234` 删；`共3,4人`（后只有 1 位）、`1,2345`（后 4 位）都不删——那两个逗号是句读。
fn strip_thousands(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, c) in chars.iter().enumerate() {
        let is_thousands = *c == ','
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && (1..=3).all(|k| chars.get(i + k).is_some_and(char::is_ascii_digit))
            && chars.get(i + 4).is_none_or(|n| !blocks(*n));
        if !is_thousands {
            out.push(*c);
        }
    }
    out
}

/// 边界字符：数字串左右紧挨着这些，说明它不是独立数字（`iPhone15`/`A4纸`/`ISO9001`）
fn blocks(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn left_free(chars: &[char], i: usize) -> bool {
    i == 0 || !blocks(chars[i - 1])
}

fn right_free(chars: &[char], i: usize) -> bool {
    chars.get(i).is_none_or(|c| !blocks(*c))
}

/// 数字串右端（不含）：chars[i..] 中第一个非数字的下标
fn run_end(chars: &[char], i: usize) -> usize {
    let mut j = i;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    j
}

/// 跳过空白（Python `\s*`：`2026 年` / `30 %` / `1234.56 元` 里的空格要被吞掉）
fn skip_blank(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// `1234.56` → 一千二百三十四点五六（Python `_money`）
fn money(digits: &str) -> String {
    match digits.split_once('.') {
        Some((int, frac)) => format!("{}点{}", cardinal_digits(int), digits_zh(frac, false)),
        None => cardinal_digits(digits),
    }
}

/// 取 chars[i..] 的数字串（可含小数点后的数字），返回 (结束下标, 串)
fn number_with_frac(chars: &[char], i: usize) -> (usize, String) {
    let int_end = run_end(chars, i);
    let mut end = int_end;
    if chars.get(int_end) == Some(&'.') && chars.get(int_end + 1).is_some_and(char::is_ascii_digit)
    {
        end = run_end(chars, int_end + 1);
    }
    (end, chars[i..end].iter().collect())
}

/// 金额①：`¥1234.56` → 一千二百三十四点五六元（吃 ¥ 与中间空格、补「元」）
fn currency_symbol(chars: &[char], i: usize) -> Option<(usize, String)> {
    if !matches!(chars[i], '¥' | '￥') {
        return None;
    }
    let start = skip_blank(chars, i + 1);
    if !chars.get(start).is_some_and(char::is_ascii_digit) {
        return None; // ¥ 后面不是数字：原样保留，不做金额处理
    }
    let (end, digits) = number_with_frac(chars, start);
    Some((end, format!("{}元", money(&digits))))
}

/// 数字规则（顺序与 Python 管线一致，顺序错就会漂移）：
/// 金额 → 电话 → 小数 → 百分比 → 年份 → 长串逐位 → 基数词
fn number_rule(chars: &[char], i: usize) -> Option<(usize, String)> {
    if !left_free(chars, i) {
        return None;
    }
    let end = run_end(chars, i);
    let run: String = chars[i..end].iter().collect();

    // 金额②：`1234.56 元` → 一千二百三十四点五六（元字保留，中间空格吞掉）
    // 必须排在电话之前：`13812345678元` 是金额不是电话号码
    {
        let (nend, digits) = number_with_frac(chars, i);
        let after = skip_blank(chars, nend);
        if chars.get(after) == Some(&'元') {
            return Some((after, money(&digits)));
        }
    }
    // 电话：11 位且以 1 开头 → 逐位（首位读幺）
    if run.len() == 11 && run.starts_with('1') && right_free(chars, end) {
        return Some((end, digits_zh(&run, true)));
    }
    // 小数：`1234.56` → 一千二百三十四点五六
    {
        let (nend, _) = number_with_frac(chars, i);
        if nend > end && right_free(chars, nend) {
            let frac: String = chars[end + 1..nend].iter().collect();
            return Some((
                nend,
                format!("{}点{}", cardinal_digits(&run), digits_zh(&frac, false)),
            ));
        }
    }
    // 百分比：`30 %` → 百分之三十（空格吞掉）
    {
        let after = skip_blank(chars, end);
        if chars.get(after) == Some(&'%') {
            return Some((after + 1, format!("百分之{}", cardinal_digits(&run))));
        }
    }
    // 年份：`2026 年` → 二零二六年（逐位；范围是 1xxx 与 20xx，不只 19xx/20xx）
    if run.len() == 4 && (run.starts_with('1') || run.starts_with("20")) {
        let after = skip_blank(chars, end);
        if chars.get(after) == Some(&'年') {
            return Some((after, digits_zh(&run, false)));
        }
    }
    // 超长数字串（≥12 位：订单号/卡号/信用代码）不是数量，逐位读
    if run.len() >= 12 && right_free(chars, end) {
        return Some((end, digits_zh(&run, false)));
    }
    // 其余：基数词
    if right_free(chars, end) {
        return Some((end, cardinal_digits(&run)));
    }
    None
}

/// 数字读法规范化（基数词 / 年份逐位 / 电话幺 / 小数 / 百分比 / 金额）
pub fn verbalize(text: &str) -> String {
    let chars: Vec<char> = strip_thousands(text).chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let hit = if chars[i].is_ascii_digit() {
            number_rule(&chars, i)
        } else {
            currency_symbol(&chars, i)
        };
        match hit {
            Some((end, s)) => {
                out.push_str(&s);
                i = end;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// 发音词典：最长匹配优先
pub fn apply_dictionary(text: &str, dict: &BTreeMap<String, String>) -> String {
    let mut entries: Vec<(&String, &String)> = dict.iter().collect();
    entries.sort_by_key(|(k, _)| std::cmp::Reverse(k.chars().count()));
    let mut out = text.to_string();
    for (k, v) in entries {
        if !k.is_empty() {
            out = out.replace(k.as_str(), v);
        }
    }
    out
}

/// 完整文本层：词典优先，再做数字规范化
pub fn normalize(text: &str, dict: &BTreeMap<String, String>) -> String {
    verbalize(&apply_dictionary(text, dict))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinal_reads_commonly_used_forms() {
        assert_eq!(cardinal(10), "十");
        assert_eq!(cardinal(17), "十七");
        assert_eq!(cardinal(20), "二十");
        assert_eq!(cardinal(1234), "一千二百三十四");
        assert_eq!(cardinal(10086), "一万零八十六");
        assert_eq!(cardinal(2026), "二千零二十六");
    }

    /// 上一版单位表只到「亿」，13 位（4 组）就索引越界 panic——这是 pub API 崩溃。
    /// 期望值取自 Python `cardinal()`（tests/parity_cases.txt 里有同类用例）
    #[test]
    fn cardinal_does_not_panic_on_big_groups() {
        assert_eq!(
            cardinal(1_234_567_890_123),
            "一万亿二千三百四十五亿六千七百八十九万零一百二十三"
        );
        assert_eq!(
            cardinal(u64::MAX),
            "一千八百四十四亿亿六千七百四十四万亿零七百三十七亿零九百五十五万一千六百一十五"
        );
    }

    /// 超过 u64::MAX 的串（金额/小数规则会走到基数词）不允许 panic，也不允许读成「零」：
    /// 20 位仍在单位表内（与 Python 同结果），更长的退化为逐位读
    #[test]
    fn cardinal_degrades_to_digits_beyond_unit_table() {
        assert_eq!(
            cardinal_digits("99999999999999999999"),
            "九千九百九十九亿亿九千九百九十九万亿九千九百九十九亿九千九百九十九万九千九百九十九"
        );
        assert_eq!(
            cardinal_digits("123456789012345678901234"),
            "一二三四五六七八九零一二三四五六七八九零一二三四"
        );
        assert_ne!(cardinal_digits("99999999999999999999999"), "零");
    }

    #[test]
    fn verbalize_handles_year_phone_price_percent() {
        assert_eq!(verbalize("2026年"), "二零二六年");
        assert_eq!(verbalize("13812345678"), "幺三八幺二三四五六七八");
        assert_eq!(verbalize("1234.56"), "一千二百三十四点五六");
        assert_eq!(verbalize("30%"), "百分之三十");
    }

    #[test]
    fn dictionary_wins_over_rules() {
        let mut d = BTreeMap::new();
        d.insert("重庆".to_string(), "chóng qìng".to_string());
        assert_eq!(
            normalize("我去重庆，2026年", &d),
            "我去chóng qìng，二零二六年"
        );
    }
}
