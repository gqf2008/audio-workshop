use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=ui/dub_workbench.slint");
    println!("cargo:rerun-if-changed=ui/model.slint");
    println!("cargo:rerun-if-changed=ui/voice_picker.slint");
    println!("cargo:rerun-if-changed=ui/extra_tabs.slint");
    println!("cargo:rerun-if-changed=ui/bgm_workbench.slint");

    // 下游消费者标准写法：注册 `@slint_pixel` 库路径（组件库内所有 .slint 也会被监听）。
    let library_paths = slint_pixel::library_paths();
    for path in library_paths.values() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let config = slint_build::CompilerConfiguration::new().with_library_paths(library_paths);
    slint_build::compile_with_config("ui/app.slint", config).expect("编译 Slint UI 失败");

    embed_windows_icon();
}

/// 把 `assets/icon.ico` 编成 PE 资源链进 exe（仅 Windows 目标）。
///
/// 为什么必须有：Windows 上的 exe 图标、桌面/开始菜单快捷方式、任务栏与 Alt-Tab 显示的
/// 都是**可执行文件里的图标资源**，不是某个相邻的 .ico 文件。没有这段资源时用户看到的就是
/// 一个空白默认图标（2026-09-19 真机反馈："应用程序没有图标，桌面图标也没有"）。
///
/// 为什么 .rc 是运行时生成的、而不是仓库里放一个静态文件：文件名的解析基准在 rc.exe 与
/// windres 之间不一致（有的按 .rc 所在目录、有的按当前目录），写死相对路径总有一边找不到。
/// 生成到 OUT_DIR 里并写**绝对路径**（反斜杠统一成 `/`，rc.exe 认）两边都成立。
fn embed_windows_icon() {
    // 非 Windows 目标上没有 PE 资源这回事：embed-resource 自己会返回 NotWindows，
    // 但连 .ico 都不该要求存在（Linux 打包线不需要它）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest_dir.join("assets").join("icon.ico");
    assert!(
        icon.is_file(),
        "缺少 {}（生成：python3 tools/gen_app_icon.py）—— Windows 版没有它就等于没有图标",
        icon.display()
    );
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let rc = out_dir.join("app-icon.rc");
    // `1 ICON`：1 是 windows.h 里 IDI_ICON1 的值，用字面量就不用让 rc.exe 去找头文件。
    std::fs::write(
        &rc,
        format!(
            "1 ICON DISCARDABLE \"{}\"\n",
            icon.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("写 app-icon.rc 失败");

    // `manifest_required`（而不是 optional）：编译器找不到时**必须红**。optional 会把
    // "rc.exe 没找到"当成功，用户拿到的包又是没图标的 —— 正是这条修复要根除的假绿。
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .unwrap_or_else(|e| panic!("嵌入 Windows 图标资源失败（{e}）"));
}
