//! `--engine-monitor` 的 CLI 回归：非法参数必须打印用法并**非零退出**。
//!
//! 为什么是集成测试（tests/ 目录）而不是 main.rs 里的单测：`monitor_entry` 的
//! Err 分支直接调 `std::process::exit(2)`，单测直调会把测试进程一起杀掉
//! （与 `run_monitor` 的 `std::process::exit(0)` 不可直测同理），只有 spawn
//! 真二进制才能断言退出码。
//!
//! **绝不能**传两个正整数 pid——那会真的进入监视循环：进程挂着不退出，甚至会对
//! 目标 pid 发 SIGTERM/SIGKILL。所以本文件只覆盖非法输入（2026-09-20 审查 I1）。

use std::process::Command;

fn run_monitor_cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_audio-workshop"))
        .args(args)
        .output()
        .expect("spawn audio-workshop 失败")
}

/// 全部非法输入：缺 pid / 只给一个 pid / 0 / 负数 / 非数字。
/// 断言：退出码非零（实现约定 exit=2）且 stderr 含用法。
#[test]
fn engine_monitor_rejects_invalid_args_with_nonzero_exit() {
    let cases: &[&[&str]] = &[
        &["--engine-monitor"],
        &["--engine-monitor", "12"],
        &["--engine-monitor", "0", "12"],
        &["--engine-monitor", "12", "0"],
        &["--engine-monitor", "-1", "12"],
        &["--engine-monitor", "12", "-34"],
        &["--engine-monitor", "abc", "def"],
    ];
    for case in cases {
        let out = run_monitor_cli(case);
        let code = out.status.code();
        assert_eq!(
            code,
            Some(2),
            "args={case:?} 必须非零退出（实现约定 exit=2），实际 exit={code:?}；\
             stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("用法"),
            "args={case:?} 的 stderr 应含用法，实际：{stderr}"
        );
    }
}
