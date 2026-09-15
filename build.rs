fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=ui/dub_workbench.slint");
    println!("cargo:rerun-if-changed=ui/model.slint");

    // 下游消费者标准写法：注册 `@slint_pixel` 库路径（组件库内所有 .slint 也会被监听）。
    let library_paths = slint_pixel::library_paths();
    for path in library_paths.values() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let config = slint_build::CompilerConfiguration::new().with_library_paths(library_paths);
    slint_build::compile_with_config("ui/app.slint", config).expect("编译 Slint UI 失败");
}
