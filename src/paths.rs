//! 路径比较的唯一入口。
//!
//! `Path::starts_with` 是**纯词法**比较：既不解析 `.` / `..`，也不解析软链。拿它做
//! "在不在里面"的判定会被 `…/x/../目标` 或一条指向目标的软链绕过（一键备份就实测出过：
//! 备份把自己递归抄进了源目录，直到 `File name too long` 才报错）。所以凡是"包含/落在
//! 里面"的判断都走这里，别在调用点自己比字符串前缀。
//! 见 `LESSON_路径包含判定必须按真实路径而非字面前缀.md`。

use std::path::{Path, PathBuf};

/// 把路径解析成"真实的绝对路径"，**允许路径本身还不存在**。
///
/// `fs::canonicalize` 要求整条路径都存在，而备份目标、下载落点这类路径通常还没建出来。
/// 做法：向上找到最近的、真实存在的祖先做 `canonicalize`（这一步会解开软链），再把剩下的
/// 段按词法拼回去。剩下的段都还不存在，其中的 `..` 只会在自己的尾段里回退、或退到已解析
/// 的祖先上——与内核 `create_dir_all` 的实际行为一致。
///
/// `base`：相对路径的基准目录（调用方给绝对路径更稳；给相对路径会按当前目录再解析一次）。
pub(crate) fn resolve_for_compare(p: &Path, base: &Path) -> Result<PathBuf, String> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut base = abs;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&base) {
            let mut out = real;
            for seg in tail.iter().rev() {
                let seg = seg.as_os_str();
                if seg == std::ffi::OsStr::new("..") {
                    out.pop();
                } else if seg != std::ffi::OsStr::new(".") {
                    out.push(seg);
                }
            }
            return Ok(out);
        }
        match split_last(&base) {
            Some((parent, name)) if parent != base => {
                tail.push(name);
                base = parent;
            }
            _ => return Err(format!("路径解析不了：{}", p.display())),
        }
    }
}

/// 把路径拆成「父目录 + 最后一段」。
///
/// **不能用 `parent()` / `file_name()`**：最后一段是 `..`（或 `.`）时 `file_name()` 返回
/// `None`，于是整个回退循环走到 `_ =>` 直接报"路径解析不了"——`…/models/x/..` 这种
/// 结尾是 `..` 的路径实测踩过：判定变成静默 false（"在里面"被当成"不在里面"）。
fn split_last(p: &Path) -> Option<(PathBuf, std::ffi::OsString)> {
    let mut comps: Vec<std::path::Component> = p.components().collect();
    let last = comps.pop()?;
    let mut parent = PathBuf::new();
    for c in comps {
        parent.push(c.as_os_str());
    }
    Some((parent, last.as_os_str().to_os_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `..` 要按真实路径消解：`<dir>/x/../<dir>/sub` 必须与 `<dir>/sub` 判为同一处。
    /// （字面前缀比较会把这两条判成不同——那正是备份自吞的成因。）
    #[test]
    fn dotdot_is_resolved_not_compared_literally() {
        let tmp = std::env::temp_dir().join(format!("aw-paths-{}", std::process::id()));
        let inner = tmp.join("x");
        std::fs::create_dir_all(&inner).unwrap();
        let a = resolve_for_compare(&tmp.join("x").join("..").join("sub"), &tmp).unwrap();
        let b = resolve_for_compare(&tmp.join("sub"), &tmp).unwrap();
        assert_eq!(a, b);
        assert!(b.starts_with(resolve_for_compare(&tmp, &tmp).unwrap()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 尾段是 `..` 也要能解析。
    ///
    /// 反例（修之前）：回退循环用 `parent()` / `file_name()` 往上走，而 `..` 作为最后一段时
    /// `file_name()` 是 `None` → 整条路径直接报"解析不了"。调用方拿到 Err 只能当作
    /// "不在里面"，于是"目标落在源目录里"这种判定变成**静默 false**。
    #[test]
    fn trailing_parent_dir_component_still_resolves() {
        let tmp = std::env::temp_dir().join(format!("aw-paths-dotdot-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("models/in")).unwrap();

        let ends_with_dotdot = tmp.join("models").join("in").join("..");
        assert_eq!(
            resolve_for_compare(&ends_with_dotdot, &tmp).unwrap(),
            resolve_for_compare(&tmp.join("models"), &tmp).unwrap(),
            "`…/models/in/..` 必须解析成 `…/models`"
        );

        // 中间段不存在 + 尾段是 `..`：仍要按词法回退到已解析的祖先
        let mixed = tmp.join("models").join("nope").join("..").join("in");
        assert_eq!(
            resolve_for_compare(&mixed, &tmp).unwrap(),
            resolve_for_compare(&tmp.join("models/in"), &tmp).unwrap()
        );

        // 全是不存在的段：回退到最近存在的祖先即可，不能报错
        let ghost = tmp.join("ghost").join("..").join("deep");
        assert!(resolve_for_compare(&ghost, &tmp).is_ok());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 相对路径按 `base` 解析，不是按进程当前目录。
    #[test]
    fn relative_paths_resolve_against_the_given_base() {
        let tmp = std::env::temp_dir().join(format!("aw-paths-base-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("models")).unwrap();
        let got = resolve_for_compare(Path::new("models/a.gguf"), &tmp).unwrap();
        assert_eq!(
            got,
            resolve_for_compare(&tmp, &tmp)
                .unwrap()
                .join("models/a.gguf")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
