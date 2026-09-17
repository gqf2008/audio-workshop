# M3 平台状态

更新时间：2026-09-17

## 已完成的代码侧移植

- Rust：
  - `HOME` / `USERPROFILE` 通过 `dirs` 解析；工程与导出目录走系统 Documents。
  - `AW_SERVER_CONFIG` 优先，默认查找 macOS 旧路径、平台 config 目录
    （Linux `~/.config/audio.cpp/server.json`、Windows `%APPDATA%\audio.cpp\server.json`）。
  - 状态栏后端名从 `/health.backend` 动态读取，不再写死 Metal。
  - Linux fontconfig 使用 `fontconfig-dlopen`，避免发布包硬链特定 libfontconfig。
- Python：
  - `tools/platform_paths.py` 统一 server/eval/models/backend 发现。
  - `AW_MODELS_ROOT` / `AUDIO_WORKSHOP_MODELS_ROOT` 覆盖模型根；
    `AW_BACKEND` 可强制 metal/cuda/cpu；默认 Darwin→metal、`nvidia-smi`→cuda、其它→cpu。
  - 下载器不再调用 BSD `find`，改用纯 Python 遍历。

## 本机已验证

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 tools/platform_paths.py
AW_BACKEND=cuda python3 tools/platform_paths.py
AW_MODELS_ROOT=/tmp/aw-models-override python3 tools/audio_config.py check
```

结果：Rust 原生门禁全绿；平台路径/backend/models_root 覆盖冒烟通过。

Linux 原生（Colima aarch64 + rust:1.95-bookworm）：

```bash
docker run --rm -v "$HOME/.cache/audio-workshop-m3-snapshot:/work:ro" \
  -e CARGO_TARGET_DIR=/tmp/target -w /work rust:1.95-bookworm bash -c \
  'apt-get update -qq && apt-get install -y -qq pkg-config libx11-dev libxkbcommon-dev \
   libwayland-dev libasound2-dev libfontconfig1-dev libssl-dev clang >/dev/null && cargo check --workspace'
```

结果：通过（2026-09-16，约 2m20s；日志 `/tmp/m3-linux-docker-check2.log`）。

Windows GNU 交叉：

```bash
rustup target add x86_64-pc-windows-gnu
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
cargo check --workspace --target x86_64-pc-windows-gnu
```

结果：通过（2026-09-16，约 30s）。

## 交叉编译阻塞

```bash
RUST_FONTCONFIG_DLOPEN=1 cargo check --workspace --target x86_64-unknown-linux-gnu
cargo check --workspace --target x86_64-pc-windows-msvc
```

- Linux GNU target：fontconfig 已通过 dlopen 解决；宿主机继续到 `ring` 时缺
  `x86_64-linux-gnu-gcc`，属目标 C 工具链/系统 sysroot 未安装；已用原生 Linux 容器绕开验证。
- Windows MSVC target：`ring` 交叉编译到 MSVC 时缺 MSVC SDK 的 C 头（`assert.h`）；
  已用 Windows GNU target 验证同一源码树。

因此这两个命令不能代替目标机器构建；需要 Windows/Linux 真机或完整交叉工具链。

## M3 真机验收清单

1. 在目标机安装/启动 CUDA 版 `audiocpp_server`，确认 `/health` 的 backend 为 `cuda`。
2. 设置 `AW_SERVER_CONFIG` 指向目标机 `server.json`，或放到平台默认配置目录。
3. 设置 `AW_MODELS_ROOT` 指向目标机模型目录，运行 `audio_config.py check`。
4. 执行：
   - `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
   - `cargo test -p aw-core --test e2e_service -- --ignored --test-threads=1`
   - 桌面壳完成一次配音 → BGM → voice/bgm/mixed 导出
5. 归档系统版本、GPU 型号、CUDA 版本、`/health` 输出、三轨 WAV 与 SRT。
6. 满足“第二个平台可自用”后，才能关闭 M3。

## CI 能证明什么 / 不能证明什么

`feat/m3-ci-matrix` 补上了 `.github/workflows/ci.yml`：三平台矩阵（ubuntu / macos / windows），
每个平台跑 `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace`，以及 `python3 -m unittest discover -s tools/tests`（Windows runner 上
解释器叫 `python`，跑的是同一条命令）。

**能证明**：
- 这套代码在 Linux / Windows 上**编译得起来、clippy 干净、单测能过** —— 也就是本机因为
  没有目标 sysroot 而做不到的那部分；
- Linux 侧的系统依赖清单是完整的（pkg-config / libx11 / libxkbcommon / libwayland /
  libasound2 / libfontconfig / libssl-dev / clang），后来者照抄不会再踩一遍；
- 三平台之间没有**门禁层面**的回归。

**不能证明（所以不能拿它替代 M3 的关闭判据）**：
- 上面「M3 真机验收清单」的 6 步**一步都没被 CI 覆盖**：CI 没有 NVIDIA GPU、没有 CUDA 版
  `audiocpp_server`、不跑 `--ignored` 的真服务 e2e、也不产出配音 / BGM / 导出成品；
- Windows 跑的是 `windows-latest`（MSVC），跟「消费级 N 卡 CUDA 线」的目标机在驱动与 CUDA
  版本上仍可能有差异；
- Metal 侧的实机性能（RTF、峰值内存）CI 完全不涉及。

**成本与触发**：仓库是私有的，私有仓库的 Actions 会消耗账号额度。workflow 由
`pull_request` / `push: main` / `workflow_dispatch` 触发，并带 `concurrency` 取消同分支的旧任务；
额度策略未定之前别把它当成「每次提交都必须等绿」的门禁 —— `RULE_开发流程规范` 第 9 步本来
也只要求发布（tag/release）时远程 CI 通过。
