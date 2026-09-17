//! 系统文件/目录选择器的**唯一入口**：把「用户取消」与「选择器起不来」严格分开。
//!
//! 之前每个选择器都收在一条 `.output().ok()?` 上：命令不存在、没有可执行权限、
//! 系统拒绝授权、跑完但退出码非 0——全都塌成 `None`，调用方又把 `None` 说成
//! "已取消"。用户点了没反应，状态栏却说"取消了选择"，无从下手。Linux 上更是必然
//! 踩到：这些对话框依赖 `zenity`，而它此前没出现在任何依赖清单里。
//!
//! 这里把一次选择定成三态（选到 / 取消 / 起不来），并把"起不来"的分类与文案
//! （含各平台的安装、授权指引）收在一处。调用方必须 switch 三种情形，
//! **不要**再抽一个 `Option` 把两种"没选到"合并——那正是本批要修的 bug
//! （见 `LESSON_同一语义两处实现必然漂移回显需与真实行为同源.md`）。
//!
//! 平台差异只有两处：拉哪个程序（`Spec`）与"取消"在该平台长什么样
//! （`cancel_exit_code` / `empty_output_is_cancel`）。判定本身是一份纯函数，
//! 三个平台的契约都能在本机测（见 `mod tests`）。

use std::path::PathBuf;

/// 一次选择的结果。**三态，不是 `Option`**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome<T> {
    /// 用户选到了（路径已规范化）
    Picked(T),
    /// 用户主动取消：关窗 / Esc / 点取消。这是正常操作，不是错误。
    Cancelled,
    /// 选择器本身没能正常工作——必须如实告诉用户，不能伪装成"取消"。
    Unavailable(Trouble),
}

/// 选择器为什么没工作。文案只在 [`Trouble::note`] 一处生成。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Trouble {
    pub(crate) kind: TroubleKind,
    /// 哪个平台（决定安装/授权指引；也是让三个平台的文案都能被单测钉住的原因）
    platform: Platform,
    /// 拉起对话框的程序名（osascript / powershell / zenity）
    pub(crate) program: String,
    /// 原始细节（系统报错原文 / 退出码），如实透出
    detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TroubleKind {
    /// 命令不存在——这条平台依赖没装上（Linux 上就是缺 zenity）
    NotInstalled,
    /// 命令在，但没有可执行权限
    NotExecutable,
    /// 系统拒绝了授权（macOS 的自动化权限、Linux 没有图形会话……）
    Denied,
    /// 进程成功退出却没吐任何结果（既不是取消也不是选到）
    NoOutput,
    /// 其它非 0 退出（不是取消）
    Failed,
}

impl Trouble {
    /// 给状态行的一句说明。安装指引 / 授权指引 / 原始报错都带出去——
    /// 用户要能据此**做点什么**，而不是只看到"取消了选择"。
    pub(crate) fn note(&self) -> String {
        match self.kind {
            TroubleKind::NotInstalled => format!(
                "系统对话框不可用：找不到 {}（没选中任何文件）。{}",
                self.program,
                self.platform.install_hint()
            ),
            TroubleKind::NotExecutable => format!(
                "系统对话框不可用：{} 没有可执行权限（{}）。{}",
                self.program,
                self.detail,
                self.platform.install_hint()
            ),
            TroubleKind::Denied => format!(
                "系统拒绝了这次选择（{}）。{}",
                self.detail,
                self.platform.permission_hint()
            ),
            TroubleKind::NoOutput => format!(
                "对话框没有返回任何结果（{}）——这既不是取消也不是成功，请重试；\
                 若一直如此，请把这个原因一并反馈：{}",
                self.program, self.detail
            ),
            TroubleKind::Failed => format!(
                "系统对话框出错（{}）：{}。这不是取消，没有选中任何文件。{}",
                self.program,
                self.detail,
                self.platform.failure_hint()
            ),
        }
    }
}

/// 平台。不用 `#[cfg]` 分叉而是运行期常量，是为了让三个平台的契约与文案
/// 都能在本机被测到（`Platform::current()` 才是真实平台）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Windows,
    Linux,
}

impl Platform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            Platform::MacOs => {
                "macOS 的对话框由系统自带的 osascript 拉起，正常不会缺；\
                 请检查运行环境是否被裁剪过"
            }
            Platform::Windows => {
                "Windows 的对话框由系统自带的 PowerShell 拉起，正常不会缺；\
                 请检查运行环境是否被裁剪过"
            }
            Platform::Linux => {
                "Linux 的对话框依赖 zenity（本应用不随包分发）：\
                 Debian/Ubuntu 用 `sudo apt install zenity`、\
                 Fedora 用 `sudo dnf install zenity`、\
                 Arch 用 `sudo pacman -S zenity`"
            }
        }
    }

    fn permission_hint(self) -> &'static str {
        match self {
            Platform::MacOs => {
                "打开「系统设置 → 隐私与安全性 → 自动化」，允许「音频作坊」控制\
                 「系统事件 / 访达」后重试；若那里没有本应用的条目，\
                 再检查「文件与文件夹」是否禁止了它"
            }
            Platform::Windows => {
                "请检查安全软件/组策略是否拦截了 PowerShell，\
                 以及当前会话是否有桌面权限"
            }
            Platform::Linux => {
                "请检查桌面门户（xdg-desktop-portal）是否可用，\
                 以及当前会话是否有图形权限（DISPLAY / WAYLAND_DISPLAY）"
            }
        }
    }

    /// 只有 Linux 需要补这一句：非图形会话下 zenity 的报错最容易看不懂。
    fn failure_hint(self) -> &'static str {
        match self {
            Platform::Linux => "（若提示 cannot open display，说明当前会话没有图形显示）",
            _ => "",
        }
    }
}

/// 拉一次对话框要跑什么：程序 + 参数 + **该平台怎么表示"用户取消"**。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spec {
    program: &'static str,
    args: Vec<String>,
    /// 该平台把"用户取消"报成哪个退出码（`None` = 该平台不用退出码表示取消）
    cancel_exit_code: Option<i32>,
    /// 成功退出但没有任何输出，算不算用户取消
    /// （Windows 的对话框取消就是这么报的：退 0 且不输出）
    empty_output_is_cancel: bool,
}

fn folder_spec(platform: Platform, prompt: &str) -> Spec {
    match platform {
        Platform::MacOs => Spec {
            program: "osascript",
            args: vec![
                "-e".into(),
                format!("POSIX path of (choose folder with prompt \"{prompt}\")"),
            ],
            cancel_exit_code: Some(1),
            empty_output_is_cancel: false,
        },
        Platform::Windows => Spec {
            program: "powershell",
            args: vec![
                "-NoProfile".into(),
                "-Command".into(),
                format!(
                    "Add-Type -AssemblyName System.Windows.Forms | Out-Null; \
                     $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
                     $d.Description = '{prompt}'; \
                     if ($d.ShowDialog() -eq \"OK\") {{ Write-Output $d.SelectedPath }}"
                ),
            ],
            cancel_exit_code: None,
            empty_output_is_cancel: true,
        },
        Platform::Linux => Spec {
            program: "zenity",
            args: vec![
                "--file-selection".into(),
                "--directory".into(),
                format!("--title={prompt}"),
            ],
            cancel_exit_code: Some(1),
            empty_output_is_cancel: false,
        },
    }
}

/// 文件（可单选/多选）对话框的启动契约。
///
/// `label` / `patterns` 只给 Windows 与 Linux 的过滤器用；**macOS 保持不过滤**
/// （`choose file` 的 `of type` 要 UTI，改它等于换实现，本批不动）。
fn file_spec(
    platform: Platform,
    prompt: &str,
    label: &str,
    patterns: &[&str],
    multiple: bool,
) -> Spec {
    match platform {
        Platform::MacOs => {
            let args = if multiple {
                vec![
                    "-e".to_string(),
                    format!(
                        "set fs to choose file with prompt \"{prompt}\" with multiple selections allowed"
                    ),
                    "-e".into(),
                    "set out to \"\"".into(),
                    "-e".into(),
                    "repeat with f in fs".into(),
                    "-e".into(),
                    "set out to out & (POSIX path of f) & linefeed".into(),
                    "-e".into(),
                    "end repeat".into(),
                    "-e".into(),
                    "return out".into(),
                ]
            } else {
                vec![
                    "-e".into(),
                    format!("POSIX path of (choose file with prompt \"{prompt}\")"),
                ]
            };
            Spec {
                program: "osascript",
                args,
                cancel_exit_code: Some(1),
                empty_output_is_cancel: false,
            }
        }
        Platform::Windows => {
            let filter = format!("{label}|{}|所有文件|*.*", patterns.join(";"));
            let body = if multiple {
                format!(
                    "Add-Type -AssemblyName System.Windows.Forms | Out-Null; \
                     $d = New-Object System.Windows.Forms.OpenFileDialog; \
                     $d.Multiselect = $true; \
                     $d.Filter = '{filter}'; \
                     if ($d.ShowDialog() -eq \"OK\") {{ $d.FileNames | ForEach-Object {{ Write-Output $_ }} }}"
                )
            } else {
                format!(
                    "Add-Type -AssemblyName System.Windows.Forms | Out-Null; \
                     $d = New-Object System.Windows.Forms.OpenFileDialog; \
                     $d.Filter = '{filter}'; \
                     if ($d.ShowDialog() -eq \"OK\") {{ Write-Output $d.FileName }}"
                )
            };
            Spec {
                program: "powershell",
                args: vec!["-NoProfile".into(), "-Command".into(), body],
                cancel_exit_code: None,
                empty_output_is_cancel: true,
            }
        }
        Platform::Linux => {
            let mut args = vec!["--file-selection".to_string()];
            if multiple {
                args.push("--multiple".into());
                args.push("--separator=\n".into());
            }
            args.push(format!("--title={prompt}"));
            // 过滤器为空时不加（多选稿件就是这条：macOS/Linux 不过滤）
            if !patterns.is_empty() {
                args.push(format!("--file-filter={label} | {}", patterns.join(" ")));
            }
            Spec {
                program: "zenity",
                args,
                cancel_exit_code: Some(1),
                empty_output_is_cancel: false,
            }
        }
    }
}

/// 选择器进程的原始结果：把"起不来"与"跑完了"合成一个可判定的形状。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Run {
    /// 进程没能起来（命令不存在 / 没有可执行权限 / 系统不让 fork）
    SpawnFailed {
        kind: std::io::ErrorKind,
        detail: String,
    },
    /// 进程跑完了（不代表成功）
    Ran {
        success: bool,
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
}

/// 起一次选择器。**只有这一处碰 `process::Command`**。
fn run(spec: &Spec) -> Run {
    match std::process::Command::new(spec.program)
        .args(&spec.args)
        .output()
    {
        Ok(out) => Run::Ran {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(e) => Run::SpawnFailed {
            kind: e.kind(),
            detail: e.to_string(),
        },
    }
}

/// macOS / 系统级"拒绝授权"的报错特征（osascript 把 TCC 拒绝报成 -1743 之类）。
///
/// 两种语言都认：**osascript 的报错是本地化的**——本机实测"用户取消"回来的是
/// `execution error: 用户已取消。 (-128)`（中文），所以不能只匹配英文措辞。
fn looks_like_permission_denied(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("not authorized")
        || s.contains("-1743")
        || s.contains("not permitted")
        || s.contains("operation not permitted")
        || s.contains("not allowed")
        || s.contains("未获授权")
        || s.contains("未授权")
        || s.contains("不允许发送")
        || s.contains("权限")
        || s.contains("被拒绝")
}

/// "用户取消"的报错特征：osascript 把取消当错误报出来（退出码 1 + `-128`）。
///
/// **中文环境匹配的是 `-128` 而不是文字**：本机（zh-CN）实测原文是
/// `execution error: 用户已取消。 (-128)`——本地化过的文案直接认字会漏判，
/// 所以错误号才是判据，英文文字只是冗余。
fn looks_like_user_cancel(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("user canceled") || s.contains("user cancelled") || s.contains("-128")
}

/// 单个路径的规范化：去掉首尾空白与结尾的 `/`（与改动前逐字一致）。
fn normalize_path(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

/// 多选结果 → 路径表（空行与纯空白行丢掉）。
fn parse_paths(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 原始结果 → 三态结论。**唯一一处判定**（平台只作为参数，便于三平台都能测）。
fn classify<T>(
    run: Run,
    spec: &Spec,
    platform: Platform,
    parse: impl FnOnce(&str) -> T,
) -> Outcome<T> {
    let trouble = |kind: TroubleKind, detail: String| Trouble {
        kind,
        platform,
        program: spec.program.to_string(),
        detail: clip(&detail),
    };
    match run {
        Run::SpawnFailed { kind, detail } => {
            let kind = match kind {
                std::io::ErrorKind::NotFound => TroubleKind::NotInstalled,
                std::io::ErrorKind::PermissionDenied => TroubleKind::NotExecutable,
                _ => TroubleKind::Failed,
            };
            Outcome::Unavailable(trouble(kind, detail))
        }
        Run::Ran {
            success,
            code,
            stdout,
            stderr,
        } => {
            // 顺序要紧：授权被拒优先于"取消"。osascript 两种情况都是退出码 1，
            // 只有 stderr 能区分；先判取消就会把"权限被拒"说成"你取消了"。
            if looks_like_permission_denied(&stderr) {
                return Outcome::Unavailable(trouble(TroubleKind::Denied, stderr));
            }
            if looks_like_user_cancel(&stderr)
                || (code == spec.cancel_exit_code && stderr.trim().is_empty())
            {
                return Outcome::Cancelled;
            }
            if success {
                if stdout.trim().is_empty() {
                    // Windows 的取消就是"退 0 且没输出"；macOS/Linux 成功却没有
                    // 输出是异常，不能静默当成取消。
                    return if spec.empty_output_is_cancel {
                        Outcome::Cancelled
                    } else {
                        Outcome::Unavailable(trouble(
                            TroubleKind::NoOutput,
                            "进程成功退出但没有输出".to_string(),
                        ))
                    };
                }
                return Outcome::Picked(parse(&stdout));
            }
            let detail = if stderr.trim().is_empty() {
                match code {
                    Some(c) => format!("退出码 {c}（没有报错正文）"),
                    None => "进程被信号终止".to_string(),
                }
            } else {
                stderr
            };
            Outcome::Unavailable(trouble(TroubleKind::Failed, detail))
        }
    }
}

/// 状态行不该被一整屏的报错正文淹没；超出就截断（与 aw-core 的错误体口径一致）。
fn clip(s: &str) -> String {
    let t = s.trim();
    let mut out: String = t.chars().take(400).collect();
    if t.chars().count() > 400 {
        out.push('…');
    }
    out
}

/// 选一个目录。`Picked` 时路径已去掉结尾的 `/`。
pub(crate) fn pick_folder(prompt: &str) -> Outcome<String> {
    let platform = Platform::current();
    let spec = folder_spec(platform, prompt);
    classify(run(&spec), &spec, platform, normalize_path)
}

/// 选一个文件。`label` / `patterns` 只影响 Windows 与 Linux 的过滤器。
pub(crate) fn pick_file(prompt: &str, label: &str, patterns: &[&str]) -> Outcome<String> {
    let platform = Platform::current();
    let spec = file_spec(platform, prompt, label, patterns, false);
    classify(run(&spec), &spec, platform, normalize_path)
}

/// 选多个文件（空表 = 用户取消，与"选择器起不来"严格分开）。
pub(crate) fn pick_files(prompt: &str, label: &str, patterns: &[&str]) -> Outcome<Vec<PathBuf>> {
    let platform = Platform::current();
    let spec = file_spec(platform, prompt, label, patterns, true);
    classify(run(&spec), &spec, platform, parse_paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(success: bool, code: Option<i32>, stdout: &str, stderr: &str) -> Run {
        Run::Ran {
            success,
            code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn spec_for(platform: Platform) -> Spec {
        folder_spec(platform, "测试")
    }

    /// ① 三态的**取消**：三个平台各自的"用户取消"都要判成 `Cancelled`。
    ///
    /// macOS 的取消是"退出码 1 + stderr 里的 User canceled"；zenity 是"退出码 1 +
    /// 没有 stderr"；Windows 的对话框取消是"退 0 且没有输出"。三种形状必须都被认出来，
    /// 否则用户正常取消一次就会被报成"选择器坏了"。
    #[test]
    fn user_cancel_is_recognised_on_every_platform() {
        let mac = classify(
            ran(false, Some(1), "", "execution error: User canceled. (-128)"),
            &spec_for(Platform::MacOs),
            Platform::MacOs,
            normalize_path,
        );
        assert_eq!(mac, Outcome::<String>::Cancelled, "macOS 关窗/ Esc 是取消");

        // 本机（zh-CN）**真机实测**的原文就是这样：osascript 的报错被本地化了，
        // 只认英文 "User canceled" 会漏判 → 靠错误号 -128 兜住。
        let mac_zh = classify(
            ran(
                false,
                Some(1),
                "",
                "0:17: execution error: 用户已取消。 (-128)",
            ),
            &spec_for(Platform::MacOs),
            Platform::MacOs,
            normalize_path,
        );
        assert_eq!(
            mac_zh,
            Outcome::<String>::Cancelled,
            "中文环境下的取消文案（实测原文）也必须判成取消"
        );

        let linux = classify(
            ran(false, Some(1), "", ""),
            &spec_for(Platform::Linux),
            Platform::Linux,
            normalize_path,
        );
        assert_eq!(
            linux,
            Outcome::<String>::Cancelled,
            "zenity 退出码 1 是取消"
        );

        let win = classify(
            ran(true, Some(0), "", ""),
            &spec_for(Platform::Windows),
            Platform::Windows,
            normalize_path,
        );
        assert_eq!(
            win,
            Outcome::<String>::Cancelled,
            "Windows 退 0 且无输出是取消"
        );
    }

    /// ② **本批的 bug 本身**：命令不存在必须报"对话框不可用"，不能伪装成取消。
    ///
    /// 阳性对照：把这个分类改回"和取消映射成同一个值"（例如都返回 `Cancelled`），
    /// 这条立刻红。Linux 上这就是缺 zenity 的真实场景。
    #[test]
    fn missing_command_is_unavailable_not_cancelled() {
        let out: Outcome<String> = classify(
            Run::SpawnFailed {
                kind: std::io::ErrorKind::NotFound,
                detail: "No such file or directory (os error 2)".to_string(),
            },
            &spec_for(Platform::Linux),
            Platform::Linux,
            normalize_path,
        );
        match out {
            Outcome::Unavailable(t) => {
                assert_eq!(t.kind, TroubleKind::NotInstalled);
                assert_eq!(t.program, "zenity");
                let note = t.note();
                assert!(note.contains("zenity"), "必须点名缺的是 zenity：{note}");
                assert!(
                    note.contains("apt install zenity"),
                    "Linux 必须给安装指引：{note}"
                );
            }
            other => panic!("命令不存在被误判成了 {other:?}（这正是本批要修的静默失败）"),
        }
    }

    /// ③ 「命令不存在」的可注入真机路径：**真的去 spawn 一个不存在的程序**，
    /// 走完整的 `run()` → `classify()`，而不是只喂一个构造出来的 `Run`。
    ///
    /// 不去动 osascript（那是系统工具）；注入点就是"进程名"这个参数本身。
    #[test]
    fn real_spawn_of_a_missing_program_lands_in_not_installed() {
        let spec = Spec {
            program: "audio-workshop-picker-does-not-exist-9f3a",
            args: vec![],
            cancel_exit_code: Some(1),
            empty_output_is_cancel: false,
        };
        let out: Outcome<String> = classify(run(&spec), &spec, Platform::current(), normalize_path);
        match out {
            Outcome::Unavailable(t) => assert_eq!(t.kind, TroubleKind::NotInstalled),
            other => panic!("真的起不来的进程应当报 NotInstalled，实际 {other:?}"),
        }
    }

    /// ④ 权限被拒（macOS 自动化 / TCC）：既要与取消分开，也要给出**去哪开**。
    #[test]
    fn permission_denied_gets_authorization_guidance() {
        let out: Outcome<String> = classify(
            ran(
                false,
                Some(1),
                "",
                "execution error: Not authorized to send Apple events to System Events. (-1743)",
            ),
            &spec_for(Platform::MacOs),
            Platform::MacOs,
            normalize_path,
        );
        match out {
            Outcome::Unavailable(t) => {
                assert_eq!(t.kind, TroubleKind::Denied, "不能当成用户取消");
                let note = t.note();
                assert!(note.contains("隐私与安全性"), "要给出授权路径：{note}");
                assert!(note.contains("自动化"), "要指明是自动化权限：{note}");
            }
            other => panic!("权限被拒被误判成了 {other:?}"),
        }
    }

    /// ⑤ 其它非 0 退出既不是取消也不是成功：如实报错并带上原始正文。
    #[test]
    fn other_failures_are_reported_with_their_own_reason() {
        let out: Outcome<String> = classify(
            ran(false, Some(2), "", "zenity: 参数不合法"),
            &spec_for(Platform::Linux),
            Platform::Linux,
            normalize_path,
        );
        match out {
            Outcome::Unavailable(t) => {
                assert_eq!(t.kind, TroubleKind::Failed);
                assert!(t.note().contains("zenity: 参数不合法"), "要带原始原因");
            }
            other => panic!("非 0 退出被误判成了 {other:?}"),
        }
    }

    /// ⑥ macOS/Linux 上"成功退出却没输出"是异常，不是取消
    /// （Windows 的取消恰恰长这样，见 `user_cancel_is_recognised_on_every_platform`）。
    #[test]
    fn empty_output_is_only_a_cancel_where_the_platform_says_so() {
        for platform in [Platform::MacOs, Platform::Linux] {
            let out: Outcome<String> = classify(
                ran(true, Some(0), "", ""),
                &spec_for(platform),
                platform,
                normalize_path,
            );
            match out {
                Outcome::Unavailable(t) => assert_eq!(t.kind, TroubleKind::NoOutput),
                other => panic!("{platform:?} 上「成功但没输出」应当是异常，实际 {other:?}"),
            }
        }
    }

    /// ⑦ 选到的路径要按老口径规范化（去空白、去结尾 `/`），多选按行拆、丢空行。
    #[test]
    fn picked_paths_are_parsed_and_normalised() {
        let one: Outcome<String> = classify(
            ran(true, Some(0), "/tmp/声音/\n", ""),
            &spec_for(Platform::MacOs),
            Platform::MacOs,
            normalize_path,
        );
        assert_eq!(one, Outcome::Picked("/tmp/声音".to_string()));

        let many: Outcome<Vec<PathBuf>> = classify(
            ran(true, Some(0), "/a/1.txt\n\n  /a/2.txt  \n", ""),
            &file_spec(Platform::Linux, "选择稿件", "稿件", &[], true),
            Platform::Linux,
            parse_paths,
        );
        assert_eq!(
            many,
            Outcome::Picked(vec![PathBuf::from("/a/1.txt"), PathBuf::from("/a/2.txt")]),
            "空行不能变成一个空路径（那会被导入记成「跳过 1 篇」）"
        );
    }

    /// ⑧ 各平台的启动契约：程序名、取消表示法、以及 Linux 的 zenity 依赖本身。
    #[test]
    fn platform_specs_match_each_dialog_tool() {
        let mac = folder_spec(Platform::MacOs, "选择目录");
        assert_eq!(mac.program, "osascript");
        assert_eq!(mac.cancel_exit_code, Some(1));
        assert!(!mac.empty_output_is_cancel);

        let win = folder_spec(Platform::Windows, "选择目录");
        assert_eq!(win.program, "powershell");
        assert_eq!(win.cancel_exit_code, None);
        assert!(win.empty_output_is_cancel, "Windows 取消是退 0 + 空输出");

        let linux = folder_spec(Platform::Linux, "选择目录");
        assert_eq!(linux.program, "zenity");
        assert!(linux.args.iter().any(|a| a == "--directory"));
        assert_eq!(linux.cancel_exit_code, Some(1));

        let linux_files = file_spec(Platform::Linux, "选择音频", "音频", &["*.wav"], true);
        assert!(linux_files.args.iter().any(|a| a == "--multiple"));
        assert!(
            linux_files
                .args
                .iter()
                .any(|a| a == "--file-filter=音频 | *.wav"),
            "过滤器要按 zenity 的写法拼：{:?}",
            linux_files.args
        );
    }

    /// ⑨ 三个平台的"装不上"文案各不相同：Linux 必须给 zenity 安装命令，
    /// macOS 不该把 Linux 的指引甩给用户。
    #[test]
    fn install_guidance_is_per_platform() {
        // 程序名按各平台自己的选择器给（否则会把 Linux 的依赖名带进 macOS 的文案，
        // 那是测试自己构造错了，不是实现的问题）
        let trouble = |platform| Trouble {
            kind: TroubleKind::NotInstalled,
            platform,
            program: match platform {
                Platform::Linux => "zenity",
                Platform::MacOs => "osascript",
                Platform::Windows => "powershell",
            }
            .to_string(),
            detail: String::new(),
        };
        let linux = trouble(Platform::Linux).note();
        assert!(linux.contains("apt install zenity"), "{linux}");
        assert!(linux.contains("dnf install zenity"), "{linux}");
        assert!(linux.contains("pacman -S zenity"), "{linux}");

        let mac = trouble(Platform::MacOs).note();
        assert!(
            !mac.contains("zenity"),
            "macOS 上不该出现 Linux 的依赖：{mac}"
        );
        assert!(mac.contains("osascript") || mac.contains("系统自带"));
    }

    /// 真机（默认 ignored）：**真的拉起 osascript**，把两条真实的系统结果过一遍
    /// 生产的 `run()` + `classify()`——用户取消（`error -128`）与成功返回路径。
    ///
    /// ```sh
    /// cargo test -p audio-workshop --bin audio-workshop \
    ///   picker::tests::live_osascript_paths_are_classified_like_the_real_thing -- --ignored --nocapture
    /// ```
    /// 这两条**锁屏时也能跑**（不弹窗）；真对话框那半见
    /// `live_folder_dialog_reports_what_happened`。
    #[test]
    #[ignore]
    fn live_osascript_paths_are_classified_like_the_real_thing() {
        // ① 真取消：osascript 对 `error number -128` 的退出码与 stderr，与用户关窗
        //    / 按 Esc 时逐字一致（本机实测：`execution error: 用户已取消。 (-128)`，exit 1）
        let cancel = Spec {
            program: "osascript",
            args: vec!["-e".into(), "error number -128".into()],
            cancel_exit_code: Some(1),
            empty_output_is_cancel: false,
        };
        let raw = run(&cancel);
        println!("真取消的原始结果：{raw:?}");
        let cancelled: Outcome<String> = classify(raw, &cancel, Platform::MacOs, normalize_path);
        println!("真取消 → {cancelled:?}");
        assert_eq!(
            cancelled,
            Outcome::Cancelled,
            "真 osascript 的取消必须是取消"
        );

        // ② 真成功：真 osascript 返回一个路径，形状与对话框选中时一致
        let ok = Spec {
            program: "osascript",
            args: vec!["-e".into(), "return \"/tmp/音频作坊/\"".into()],
            cancel_exit_code: Some(1),
            empty_output_is_cancel: false,
        };
        let raw = run(&ok);
        println!("真成功的原始结果：{raw:?}");
        let picked: Outcome<String> = classify(raw, &ok, Platform::MacOs, normalize_path);
        println!("真成功 → {picked:?}");
        assert_eq!(picked, Outcome::Picked("/tmp/音频作坊".to_string()));
    }

    /// 真机（默认 ignored）：**真弹一次系统目录对话框**，把三态结论打出来。
    ///
    /// 需要有人在键盘前（或用 HID 注入）按「选择 / 取消」，所以默认不跑：
    /// ```sh
    /// AW_PICKER_LIVE=1 cargo test -p audio-workshop --bin audio-workshop \
    ///   picker::tests::live_folder_dialog_reports_what_happened -- --ignored --nocapture
    /// ```
    /// 没设 `AW_PICKER_LIVE=1` 就直接返回，免得在 CI / 别人的机器上弹窗把测试挂住。
    ///
    /// **锁屏时不要跑这条**：对话框会照常被拉起（窗口在窗口服务器里，本机实测可见
    /// `osascript 选取文件夹`），但屏幕被 `Display 1 Shield` / loginwindow 盖住，
    /// HID 注入只会打到锁屏上——真正的点按得由在场的人做。
    /// 善后：中途 `kill` 测试进程**不会**带走 osascript 子进程，对话框会留在屏幕上，
    /// 需要再 `kill` 那个 `osascript -e POSIX path of (choose folder …)`。
    #[test]
    #[ignore]
    fn live_folder_dialog_reports_what_happened() {
        if std::env::var("AW_PICKER_LIVE").as_deref() != Ok("1") {
            eprintln!("跳过：要真弹对话框请设 AW_PICKER_LIVE=1");
            return;
        }
        let started = std::time::Instant::now();
        let out = pick_folder("真机验证：选一个目录，或点取消");
        println!(
            "真机结果（{:.1}s）：{out:?}",
            started.elapsed().as_secs_f64()
        );
        match &out {
            Outcome::Picked(p) => println!("PICKED={p}"),
            Outcome::Cancelled => println!("CANCELLED"),
            Outcome::Unavailable(t) => {
                println!("UNAVAILABLE kind={:?} note={}", t.kind, t.note())
            }
        }
    }

    /// 报错正文别把状态行淹了（超长 stderr 截到 400 字，末尾给省略号）。
    #[test]
    fn long_error_details_are_clipped() {
        let long = "x".repeat(1000);
        let out: Outcome<String> = classify(
            ran(false, Some(3), "", &long),
            &spec_for(Platform::Linux),
            Platform::Linux,
            normalize_path,
        );
        match out {
            Outcome::Unavailable(t) => {
                assert_eq!(t.detail.chars().count(), 401, "400 字 + 省略号");
                assert!(t.detail.ends_with('…'));
            }
            other => panic!("{other:?}"),
        }
    }
}
