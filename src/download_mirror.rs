//! 下载源镜像：把 HF 官方 URL 重写成镜像前缀（M4-P7 第二段）。
//!
//! 设计约束（与 issue 一致）：
//!   · **只有** HF 官方前缀的 URL 才重写——非 HF 链接原样返回。服务清单里完全可以写
//!     内网 / 自建仓库 / `s3://` 之类，静默改写它们会指向不存在的地址。
//!   · 前缀尾斜杠 / 多斜杠归一：`https://hf-mirror.com` 与 `https://hf-mirror.com///`
//!     必须产出同一个结果（用户从浏览器复制地址常常带斜杠）。
//!   · 空前缀 = 原样（等价于"用官方"），**不是**报错也不是当成根路径。
//!   · **不许静默回退**：镜像不可达就是网络错误，如实抛给用户；本模块只做地址重写，
//!     不做"试镜像、失败偷偷走官方"——那正是最坏形态（用户以为在用镜像）。
//!
//! 这一份是**唯一**的重写实现：`mirror_note()` 的显示文案与实际请求都从它推导，
//! 免得"回显一套、行为另一套"（见 LESSON_同一语义两处实现必然漂移）。

/// 被视为"官方源"的前缀。`/resolve/` 与 `/datasets/` 等路径都挂在这两个之下。
const OFFICIAL_PREFIXES: [&str; 2] = ["https://huggingface.co", "http://huggingface.co"];

/// 归一化镜像前缀：去掉尾部所有 `/` 与空白。
///
/// 返回 `None` 表示"没有镜像"（空串 / 纯空白）——调用方据此原样返回官方 URL。
/// 非空时保留用户写的大小写（域名大小写不敏感，但路径可能有意义，不该动）。
pub fn normalize_prefix(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim();
    let stripped = trimmed.trim_end_matches('/');
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// 把一个（可能是官方的）下载 URL 按镜像前缀重写。
///
/// - 空前缀 → 原样；
/// - 非 HF 官方前缀 → 原样（内网 / 自建仓库 / 别的 CDN 不动）；
/// - 官方 → `<镜像前缀>/<官方前缀之后的全部路径与查询>`。
///
/// 匹配官方前缀时**大小写不敏感、且要求后面紧跟 `/` 或字符串结束**：
/// 这样 `https://huggingface.co.evil.example/x` 不会被误判成官方（子串匹配的经典坑）。
pub fn rewrite_url(url: &str, prefix: &str) -> String {
    let Some(mirror) = normalize_prefix(prefix) else {
        return url.to_string();
    };
    match official_suffix(url) {
        Some(rest) => format!("{mirror}/{rest}"),
        None => url.to_string(),
    }
}

/// 官方 URL 去掉官方前缀之后的余下部分（不含前导 `/`）。
///
/// `None` = 这条 URL 不是 HF 官方（原样返回）。
fn official_suffix(url: &str) -> Option<&str> {
    for official in OFFICIAL_PREFIXES {
        // 长度不同：用 byte slice 之前先确认边界，避免在 UTF-8 中间切开
        if url.len() < official.len() {
            continue;
        }
        let (head, tail) = url.split_at(official.len());
        if !head.eq_ignore_ascii_case(official) {
            continue;
        }
        // 必须紧跟 `/`（或就是空前缀本身）——防 `huggingface.co.evil.example`
        if tail.is_empty() {
            return Some("");
        }
        if let Some(rest) = tail.strip_prefix('/') {
            return Some(rest);
        }
    }
    None
}

/// 界面上「当前生效的源」那一行。
///
/// 与 [`rewrite_url`] 同源：有镜像就报镜像，没有就报官方——**不猜、不美化**。
pub fn source_note(prefix: &str) -> String {
    match normalize_prefix(prefix) {
        Some(mirror) => {
            format!("下载源：镜像 {mirror}（HF 官方地址会改写到这里；其它地址原样不动）")
        }
        None => "下载源：HuggingFace 官方（huggingface.co）".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方 URL 会被改写到镜像前缀下。
    #[test]
    fn official_urls_are_rewritten_onto_the_mirror() {
        assert_eq!(
            rewrite_url(
                "https://huggingface.co/IndexTTS2.5-GGUF/index-tts2_5-q8_0.gguf",
                "https://hf-mirror.com"
            ),
            "https://hf-mirror.com/IndexTTS2.5-GGUF/index-tts2_5-q8_0.gguf"
        );
        // 查询串必须原样带上（HF 的 resolve 链接常带 ?download=true）
        assert_eq!(
            rewrite_url(
                "https://huggingface.co/a/b.gguf?download=true",
                "https://hf-mirror.com"
            ),
            "https://hf-mirror.com/a/b.gguf?download=true"
        );
    }

    /// 非 HF 链接**原样不动**——服务清单可以写内网 / 自建仓库。
    #[test]
    fn non_hf_urls_are_left_alone() {
        let mirror = "https://hf-mirror.com";
        for url in [
            "https://example.com/weights.gguf",
            "http://10.0.0.5:8080/models/x.gguf",
            "file:///Volumes/DataExt/models/x.gguf",
            "s3://bucket/key.gguf",
        ] {
            assert_eq!(rewrite_url(url, mirror), url, "{url} 不该被改写");
        }
    }

    /// 子串匹配的经典坑：`huggingface.co.evil.example` 不是官方域。
    #[test]
    fn lookalike_hosts_are_not_treated_as_official() {
        let mirror = "https://hf-mirror.com";
        for url in [
            "https://huggingface.co.evil.example/x.gguf",
            "https://huggingface.company.internal/x.gguf",
            "https://not-huggingface.co/x.gguf",
        ] {
            assert_eq!(rewrite_url(url, mirror), url, "{url} 不该被改写");
        }
        // 反过来：官方域后面紧跟 `/` 才算
        assert_eq!(
            rewrite_url("https://huggingface.co/x", mirror),
            "https://hf-mirror.com/x"
        );
        // 大小写不敏感（URL 主机名本就大小写不敏感）
        assert_eq!(
            rewrite_url("https://HuggingFace.CO/x", mirror),
            "https://hf-mirror.com/x"
        );
    }

    /// 前缀带尾斜杠 / 多斜杠 / 空白都要归一，且与不带斜杠结果一致。
    #[test]
    fn prefix_trailing_slashes_are_normalized() {
        let official = "https://huggingface.co/org/repo/resolve/main/m.gguf";
        let expect = "https://hf-mirror.com/org/repo/resolve/main/m.gguf";
        for prefix in [
            "https://hf-mirror.com",
            "https://hf-mirror.com/",
            "https://hf-mirror.com///",
            "  https://hf-mirror.com/  ",
        ] {
            assert_eq!(rewrite_url(official, prefix), expect, "prefix={prefix:?}");
        }
    }

    /// 前缀本身带路径也要顶住（内网常把镜像挂在子路径下）。
    #[test]
    fn prefix_with_a_path_is_kept() {
        assert_eq!(
            rewrite_url(
                "https://huggingface.co/org/repo/resolve/main/m.gguf",
                "https://mirror.internal/hf/"
            ),
            "https://mirror.internal/hf/org/repo/resolve/main/m.gguf"
        );
    }

    /// 空前缀（含纯空白）= 用官方，原样返回。
    #[test]
    fn empty_prefix_means_official() {
        let url = "https://huggingface.co/org/repo/resolve/main/m.gguf";
        for prefix in ["", "   ", "/", "///"] {
            assert_eq!(rewrite_url(url, prefix), url, "prefix={prefix:?}");
        }
        assert_eq!(normalize_prefix(""), None);
        assert_eq!(normalize_prefix("  "), None);
        assert_eq!(normalize_prefix("/"), None);
    }

    /// 显示文案与实际行为同源：有镜像报镜像，没有报官方。
    #[test]
    fn source_note_matches_the_effective_source() {
        assert!(source_note("").contains("官方"));
        assert!(source_note("   ").contains("官方"));
        let note = source_note("https://hf-mirror.com/");
        assert!(note.contains("https://hf-mirror.com"), "note={note}");
        assert!(!note.contains("hf-mirror.com/（"), "尾斜杠应已归一：{note}");
    }

    /// 官方前缀后面直接结束（没有路径）也不该 panic。
    #[test]
    fn bare_official_prefix_is_handled() {
        assert_eq!(
            rewrite_url("https://huggingface.co", "https://hf-mirror.com"),
            "https://hf-mirror.com/"
        );
        assert_eq!(
            rewrite_url("https://huggingface.co/", "https://hf-mirror.com"),
            "https://hf-mirror.com/"
        );
    }
}
