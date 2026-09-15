//! 文本兜底层：数字读法规范化 + 发音词典。
//!
//! 与 Python 侧 tools/audio_config.py 同一套规则；上游 audio8_tts 未接文本规范化，
//! 裸数字直接进声学模型会导致读法不稳定（实测同一句三轮可懂度 83.3/98.1/94.4）。

use std::collections::BTreeMap;

const CN: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
const UNITS: [&str; 4] = ["", "十", "百", "千"];
const BIG: [&str; 3] = ["", "万", "亿"];

/// 1234 → 一千二百三十四；10086 → 一万零八十六；17 → 十七
pub fn cardinal(mut n: u64) -> String {
    if n == 0 {
        return "零".into();
    }
    fn under_10000(x: u64) -> String {
        let mut out = String::new();
        let mut zero = false;
        for i in (0..4).rev() {
            let d = (x / 10u64.pow(i)) % 10;
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
    let mut groups = Vec::new();
    while n > 0 {
        groups.push(n % 10000);
        n /= 10000;
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
    out.trim_end_matches('零').to_string()
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

/// 数字读法规范化（基数词 / 年份逐位 / 电话幺 / 小数 / 百分比）
pub fn verbalize(text: &str) -> String {
    let t = text.replace(',', ""); // 去千分位（简化处理：本层只处理纯数字串场景）
    let chars: Vec<char> = t.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let run: String = chars[start..i].iter().collect();
            // 电话：11 位以 1 开头 → 逐位（幺）
            if run.len() == 11 && run.starts_with('1') {
                out.push_str(&digits_zh(&run, true));
                continue;
            }
            // 小数：后跟 . 数字
            if i < chars.len()
                && chars[i] == '.'
                && i + 1 < chars.len()
                && chars[i + 1].is_ascii_digit()
            {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let frac: String = chars[i + 1..j].iter().collect();
                out.push_str(&cardinal(run.parse().unwrap_or(0)));
                out.push('点');
                out.push_str(&digits_zh(&frac, false));
                i = j;
                continue;
            }
            // 百分比：后跟 %
            if i < chars.len() && chars[i] == '%' {
                out.push_str("百分之");
                out.push_str(&cardinal(run.parse().unwrap_or(0)));
                i += 1;
                continue;
            }
            // 年份：4 位且后跟「年」→ 逐位（上游规范化缺这条）
            // 注意「2026 年」中间常有空格，必须跳过空白再判断，否则退回基数词读法
            if run.len() == 4 && (run.starts_with("19") || run.starts_with("20")) {
                let mut j = i;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '年' {
                    out.push_str(&digits_zh(&run, false));
                    continue;
                }
            }
            out.push_str(&cardinal(run.parse().unwrap_or(0)));
        } else {
            out.push(chars[i]);
            i += 1;
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
