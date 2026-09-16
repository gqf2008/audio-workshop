//! 应用内质检（评估台 v1）：把合成结果回读一遍，量出"听感对不对"。
//!
//! 口径（**与 CHARTER M0 定标同一个**，别与 tools/audio_eval.py 的逐例口径混用）：
//!   可懂度 = 1 − Levenshtein(参考, ASR 回读) / 参考字数
//! M0 里 1626 字稿回读距离 2 → 99.8625%，就是这个式子。
//!
//! `tools/audio_eval.py` 的逐例可懂度用的是另一套（LCS 对齐中连续 ≥2 字匹配块的字符召回），
//! 两者数值不可直接比较——所以本模块只做归一 + Levenshtein，并在文档里写明区别。
//!
//! 归一（两侧都要做，否则标点/空格/数字读法会变成假错误）：
//!   1. 只保留 CJK、ASCII 字母数字；其余（标点、空白）丢弃；ASCII 转小写；
//!   2. 中文数字串与阿拉伯数字统一（`normalize_numerals`，规则与 Python 权威实现一致，
//!      由 parity 夹具钉住）——ASR 常把「2026」回读成「二零二六」，不归一就是假错误。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 质检结果：一句的参考文本与 ASR 回读文本的对照。
#[derive(Debug, Clone, PartialEq)]
pub struct Intelligibility {
    /// 可懂度百分比（0..=100）。参考为空时：回读也为空记 100，否则记 0。
    pub percent: f64,
    /// 归一后的编辑距离（字符级）
    pub distance: usize,
    /// 归一后的参考字数（分母）
    pub total: usize,
}

/// 只保留 CJK / 字母 / 数字，去标点空白，ASCII 转小写。
pub fn normalize_for_eval(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let c = *c;
            c.is_ascii_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&c)
        })
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn cn_digit() -> &'static HashMap<char, u64> {
    static M: OnceLock<HashMap<char, u64>> = OnceLock::new();
    M.get_or_init(|| {
        [
            ('零', 0),
            ('〇', 0),
            // 权威实现的正则字符类是 [零〇O一幺…]：**只有大写 O**。
            // normalize_for_eval 会先把字母小写，所以小写 o 在两侧都不会被当数字
            // （parity 夹具里 O五 / o五 两条就是钉这个的）。
            ('O', 0),
            ('一', 1),
            ('幺', 1),
            ('二', 2),
            ('两', 2),
            ('三', 3),
            ('四', 4),
            ('五', 5),
            ('六', 6),
            ('七', 7),
            ('八', 8),
            ('九', 9),
        ]
        .into_iter()
        .collect()
    })
}

/// 小数部分的数字类：权威实现的正则里小数尾是 `[零〇一幺二两三四五六七八九]`，
/// **不含大写 O**（首字符类才有 O）。别把两个类写成同一个，否则「三点O五」会算成 3.05
/// 而权威实现给的是 3点05 —— reviewer 加边界用例时抓到的。
fn is_frac_digit(c: char) -> bool {
    matches!(
        c,
        '零' | '〇' | '一' | '幺' | '二' | '两' | '三' | '四' | '五' | '六' | '七' | '八' | '九'
    )
}

fn cn_unit(c: char) -> Option<u64> {
    match c {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1000),
        _ => None,
    }
}

fn cn_big(c: char) -> Option<u64> {
    match c {
        '万' => Some(10_000),
        '亿' => Some(100_000_000),
        _ => None,
    }
}

/// 一段中文数字 → 阿拉伯写法；不是纯数字串就原样返回。
/// 规则与 `tools/audio_eval.py::_cn_run_to_num` 一致（含"点"的小数、逐字读、带单位读）。
fn cn_run_to_num(run: &str) -> String {
    let digit = cn_digit();
    if run.contains('点') {
        let (head, tail) = run.split_once('点').unwrap();
        let tail_is_digits = !tail.is_empty() && tail.chars().all(is_frac_digit);
        if tail_is_digits {
            let frac: String = tail
                .chars()
                .map(|c| digit.get(&c).copied().unwrap_or(0).to_string())
                .collect();
            return format!("{}.{}", cn_run_to_num(head), frac);
        }
        return run.to_string();
    }
    if run.is_empty()
        || !run
            .chars()
            .all(|c| digit.contains_key(&c) || cn_unit(c).is_some() || cn_big(c).is_some())
    {
        return run.to_string();
    }
    if run.chars().all(|c| digit.contains_key(&c)) {
        // 逐字读：二零二六 / 幺三八
        return run
            .chars()
            .map(|c| digit.get(&c).copied().unwrap_or(0).to_string())
            .collect();
    }
    // 带单位：十七 / 一百二十三 / 三千五百万
    let (mut total, mut section, mut num) = (0u64, 0u64, 0u64);
    for c in run.chars() {
        if let Some(d) = digit.get(&c) {
            num = *d;
        } else if let Some(u) = cn_unit(c) {
            section += num.max(1) * u;
            num = 0;
        } else if let Some(b) = cn_big(c) {
            total = (total + section + num) * b;
            section = 0;
            num = 0;
        }
    }
    (total + section + num).to_string()
}

/// 把文本里的中文数字串统一成阿拉伯数字（两侧都做，避免读法差异被算成错误）。
pub fn normalize_numerals(s: &str) -> String {
    // 严格照 Python 权威实现的正则语义：`[零〇O一幺二两三四五六七八九十百千万亿]+(?:点[零〇一…九]+)?`
    //   · 数字串必须由**数字字符**开头（「点五」里的点不属于数字串，原样留下）；
    //   · 「点」只有在后面至少跟一个数字字符时才吃进数字串（「五点」→「5点」）；
    //   · 小数部分只吃数字字符，不再吃单位。
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // 起始字符类与 Python 正则一致：数字**与单位/万/亿**都可以起头（「十七」的十）
        if !(cn_digit().contains_key(&c) || cn_unit(c).is_some() || cn_big(c).is_some()) {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len() {
            let k = chars[i];
            if cn_digit().contains_key(&k) || cn_unit(k).is_some() || cn_big(k).is_some() {
                i += 1;
                continue;
            }
            if k == '点' && i + 1 < chars.len() && is_frac_digit(chars[i + 1]) {
                i += 1; // 吃掉「点」
                while i < chars.len() && is_frac_digit(chars[i]) {
                    i += 1;
                }
            }
            break;
        }
        let run: String = chars[start..i].iter().collect();
        out.push_str(&cn_run_to_num(&run));
    }
    out
}

/// 质检用归一：先按字符集过滤，再统一数字写法。
pub fn normalize(s: &str) -> String {
    normalize_numerals(&normalize_for_eval(s))
}

/// 字符级 Levenshtein 距离（两行滚动数组；字符按 `char` 计，不是字节）。
fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 可懂度：参考与回读都先归一，再按字符级编辑距离打分。
pub fn intelligibility(reference: &str, hypothesis: &str) -> Intelligibility {
    let a: Vec<char> = normalize(reference).chars().collect();
    let b: Vec<char> = normalize(hypothesis).chars().collect();
    let total = a.len();
    if total == 0 {
        return Intelligibility {
            percent: if b.is_empty() { 100.0 } else { 0.0 },
            distance: b.len(),
            total: 0,
        };
    }
    let distance = levenshtein(&a, &b);
    let percent = (1.0 - distance as f64 / total as f64) * 100.0;
    Intelligibility {
        percent: percent.max(0.0),
        distance,
        total,
    }
}

/// 首个差异处的上下文片段，便于人眼快速定位读错的字。
///
/// 参考为空或完全一致时返回 None。
pub fn diff_snippet(reference: &str, hypothesis: &str) -> Option<String> {
    let a: Vec<char> = normalize(reference).chars().collect();
    let b: Vec<char> = normalize(hypothesis).chars().collect();
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        i += 1;
    }
    if i == a.len() && i == b.len() {
        return None;
    }
    let lo = i.saturating_sub(8);
    let head: String = a[lo..i].iter().collect();
    let expect: String = a[i..(i + 6).min(a.len())].iter().collect();
    let got: String = b[i..(i + 6).min(b.len())].iter().collect();
    let tail: String = a[(i + 6).min(a.len())..(i + 14).min(a.len())]
        .iter()
        .collect();
    Some(format!(
        "…{head}【应为 {}，读到 {}】{tail}…",
        if expect.is_empty() { "∅" } else { &expect },
        if got.is_empty() { "∅" } else { &got }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_cjk_alnum_and_drops_punctuation() {
        assert_eq!(
            normalize_for_eval("你好，世界！Hello, 2026."),
            "你好世界hello2026"
        );
        assert_eq!(normalize_for_eval("  "), "");
    }

    /// 逐字读、带单位读、小数三种写法都要统一（规则与 Python 权威实现一致）
    #[test]
    fn numerals_are_unified_like_the_python_authority() {
        assert_eq!(normalize_numerals("二零二六"), "2026");
        assert_eq!(normalize_numerals("幺三八"), "138");
        assert_eq!(normalize_numerals("十七"), "17");
        assert_eq!(normalize_numerals("一百二十三"), "123");
        assert_eq!(normalize_numerals("三千五百万"), "35000000");
        assert_eq!(normalize_numerals("三点一四"), "3.14");
        // 不是数字串的部分原样保留
        assert_eq!(normalize_numerals("第 3 句"), "第 3 句");
    }

    /// 读法差异不算错：参考写阿拉伯数字、ASR 回读中文数字，要算 100%
    #[test]
    fn numeral_reading_difference_is_not_counted_as_error() {
        let r = intelligibility("2026 年 17 人", "二零二六年十七人");
        assert_eq!(r.distance, 0, "归一后应完全一致：{r:?}");
        assert!((r.percent - 100.0).abs() < 1e-9);
    }

    /// 标点与空白不算错
    #[test]
    fn punctuation_and_spacing_do_not_count() {
        let r = intelligibility("第一句测试。", "第一句测试");
        assert_eq!(r.distance, 0);
        assert_eq!(r.percent, 100.0);
    }

    /// CHARTER M0 的定标数：1626 字稿、距离 2 → 99.8625%
    #[test]
    fn matches_the_charter_m0_metric() {
        let reference: String = "口".repeat(1455);
        let mut hypo: Vec<char> = reference.chars().collect();
        hypo[10] = '错';
        hypo[100] = '错';
        let hypothesis: String = hypo.into_iter().collect();
        let r = intelligibility(&reference, &hypothesis);
        assert_eq!(r.distance, 2);
        assert_eq!(r.total, 1455);
        assert!(
            (r.percent - 99.8625).abs() < 1e-3,
            "应与 CHARTER 的 99.8625% 一致，实得 {}",
            r.percent
        );
    }

    #[test]
    fn empty_and_total_mismatch_are_defined() {
        assert_eq!(intelligibility("", "").percent, 100.0);
        assert_eq!(intelligibility("", "有内容").percent, 0.0);
        assert_eq!(intelligibility("第一句", "").percent, 0.0);
    }

    #[test]
    fn diff_snippet_points_at_the_first_mismatch() {
        let s = diff_snippet("今年二零二六年，共 17 人", "今年二零二五年，共 17 人").unwrap();
        assert!(s.contains("应为"), "{s}");
        // 片段是**归一后**的文本（中文数字已变阿拉伯），差异要看得出来：2026 → 2025
        assert!(s.contains('6') && s.contains('5'), "要能看出差在哪：{s}");
        assert!(
            diff_snippet("完全一致", "完全一致。").is_none(),
            "只差标点不算差异"
        );
    }
}
