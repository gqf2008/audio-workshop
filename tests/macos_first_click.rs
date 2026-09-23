//! macOS「应用非激活时第一次点击被吃掉」的回归守卫（2026-09-23 用户报告）。
//!
//! 现象：应用不是前台 / 窗口刚被激活时，第一次点击 Tab（或按钮、选目录）不生效，要点第二次；
//! 点击的 hover 反馈正常，第二次点击起一切正常。
//!
//! 根因：窗口级 `FocusScope` 铺满窗口、且声明在所有内容**之后**（Slint 里最后声明=最上层），
//! 而 `FocusScope` 默认 `focus-on-click: true`：只要自己还没有焦点，它就把「左键按下」这一下
//! **接受走**去拿键盘焦点（`InputEventResult::EventAccepted`），按下不再向下派发；目标控件只收到
//! 松开 → `clicked` 不触发。第二次点击时它已有焦点 → 不再拦截，于是"要点两次"。
//!
//! 本文件盯**行为**（不是源码形状）：新建窗口后（此时窗口级 FocusScope 还没有焦点）在 BGM Tab 上
//! 模拟一次「移动 + 按下 + 松开」，断言页面切过去；同时断言启动后不点任何东西也能用 ↑/↓ 快捷键。
//! 把 FocusScope 放回最上层、或去掉 `init` 里的 focus()，对应断言即报红。

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle as _, LogicalPosition, LogicalSize, WindowSize};

slint::include_modules!();

const WINDOW_WIDTH: f32 = 1200.0;
const WINDOW_HEIGHT: f32 = 880.0;

/// 主 Tab 行：根 VerticalLayout 有 2px padding，其上 34px 标题栏、再 38px Tab 行；
/// 5 个 Tab 在 `root.width - 4px` 内均分（PixelTabs 用 stretch 均分）。
fn tab_center(index: f32) -> LogicalPosition {
    let tab_width = (WINDOW_WIDTH - 4.0) / 5.0;
    LogicalPosition::new(
        2.0 + tab_width * (index + 0.5),
        2.0 + 34.0 + 19.0, // 标题栏高 34 + Tab 行中部
    )
}

fn click(ui: &MainWindow, position: LogicalPosition) {
    for event in [
        WindowEvent::PointerMoved { position },
        WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        },
    ] {
        ui.window().dispatch_event(event);
    }
}

/// 核心守卫：非激活状态下**第一次**点击 Tab 就要切页。
#[test]
fn first_click_switches_tab() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().expect("创建 MainWindow");
    ui.window().set_size(WindowSize::Logical(LogicalSize::new(
        WINDOW_WIDTH,
        WINDOW_HEIGHT,
    )));
    assert_eq!(ui.get_scene(), 0, "初始应在「配音」页");

    click(&ui, tab_center(1.0)); // BGM

    assert_eq!(
        ui.get_scene(),
        1,
        "第一次点击「BGM」就应切页：若窗口级 FocusScope 又在最上层，它会把这次「按下」吃掉，\
         页面停在第 0 页（用户看到的就是「要点两次」）"
    );
}

/// 焦点策略守卫：不点任何东西，启动后窗口级 FocusScope 就该拿到键盘焦点（快捷键可用）。
///
/// 断言的是 `keyboard-focus` 这个投影（= keyboard-scope.has-focus），不依赖 Rust 侧按键接线。
#[test]
fn keyboard_scope_has_focus_at_startup() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().expect("创建 MainWindow");
    ui.window().set_size(WindowSize::Logical(LogicalSize::new(
        WINDOW_WIDTH,
        WINDOW_HEIGHT,
    )));

    assert!(
        ui.get_keyboard_focus(),
        "启动后不点任何东西，窗口级 FocusScope 就应是焦点项（init => self.focus()）：\
         少了这条，↑/↓/空格/Esc 在窗口层面全都收不到"
    );
}

/// 反向守卫：可点控件拿到点击时，**不该**靠"抢焦点"来生效——即点完 Tab 后焦点仍在窗口级 FocusScope。
#[test]
fn clicking_a_control_keeps_keyboard_focus() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().expect("创建 MainWindow");
    ui.window().set_size(WindowSize::Logical(LogicalSize::new(
        WINDOW_WIDTH,
        WINDOW_HEIGHT,
    )));

    click(&ui, tab_center(1.0));

    assert!(
        ui.get_keyboard_focus(),
        "点 Tab 之后窗口级 FocusScope 应保持焦点：若这一下是被 FocusScope 抢走的，\
         它才会「拿到」焦点，而那正是「第一次点击被吃掉」的病征"
    );
}
