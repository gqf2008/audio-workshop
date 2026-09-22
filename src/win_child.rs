//! Windows 子进程的「不弹控制台」统一入口。
//!
//! 壳在 release 下是 GUI 子系统（`windows_subsystem = "windows"`，**没有控制台**）。
//! Windows 在 GUI 父进程里 CreateProcess 一个控制台程序（powershell、引擎 exe…）时，
//! 系统会为新进程**分配并显示一个控制台窗口**——用户看到黑窗闪烁，或者引擎一跑起来
//! 就常驻一个黑窗（2026-09-22 用户真机：打开文件选择对话框就弹控制台）。
//!
//! 修法只有一条：给子进程加 `CREATE_NO_WINDOW`（0x0800_0000）。所有起控制台子进程的
//! 地方都必须走 [`hidden_command`]，别各自内联常量（`src/model_sources.rs` 曾内联过一份，
//! 本模块就是把它收成唯一来源）。

use std::ffi::OsStr;
use std::process::Command;

/// Windows 控制台子进程的创建标志：不为子进程分配控制台窗口。非 Windows 平台无此概念。
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 起一个「不弹控制台」的子进程。
///
/// Windows 上加 `CREATE_NO_WINDOW`；其它平台与 `Command::new` 完全等价，参数透传不变。
pub fn hidden_command(program: impl AsRef<OsStr>) -> Command {
    let cmd = Command::new(program);
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt as _;
        let mut cmd = cmd;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    };
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参数透传与 `Command::new` 等价（跨平台断言；Windows 的创建标志无法从
    /// `Command` 读回，行为由源码守卫 + Windows CI 构建 + 真机确认）。
    #[test]
    fn hidden_command_preserves_program_and_args() {
        let cmd = hidden_command("powershell");
        assert_eq!(cmd.get_program(), "powershell");
        let mut cmd = hidden_command("powershell");
        cmd.args(["-NoProfile"]);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![OsStr::new("-NoProfile")]);
    }
}
