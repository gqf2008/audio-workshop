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
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) if parent != base => {
                tail.push(name.to_os_string());
                base = parent.to_path_buf();
            }
            _ => return Err(format!("路径解析不了：{}", p.display())),
        }
    }
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
