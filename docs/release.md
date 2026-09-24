# macOS 打包与发版

> 2026-09-18。本文只写**实测过的**流程与边界。

## 一条命令

```sh
./release.sh          # 打包 → 签名 → 公证（.app 与 DMG 各一次）→ 装订
```

产物（`dist/`）：

| 文件 | 说明 |
|---|---|
| `音频作坊.app` | 已签名 + 已公证 + 已装订（目录名中文没问题，它不进 HTTP 文件名） |
| `AudioWorkshop-<version>.dmg` | 拖进 Applications 的安装镜像，**自身也签了名、公证过、装订过** |

只要 `.app`（比如自己用、不对外分发）：`./package.sh` 即可 —— 它会签 Developer ID
（自动从 Keychain 挑），但不提交公证。

## 为什么 DMG 叫 `AudioWorkshop-…` 而 app 叫 `音频作坊.app`

**显示名**和**分发文件名**是两件事，早期版本把它们用一个变量串起来，踩了坑：

> v0.1.0 发 Release 时实测 —— `dist/音频作坊-0.1.0.dmg` 上传后资产名变成 **`-0.1.0.dmg`**
> （中文被剥掉），下载链接跟着坏。只能删掉重传。

所以现在：`.app` 目录名与 `CFBundleDisplayName` 用中文（用户看到的名字），而**要上传的文件名**
（DMG、公证用的 zip）一律 ASCII：`AudioWorkshop-<version>.dmg`。
`package.sh` 里有一条 `assert_ascii` 断言，非 ASCII 直接红 —— 宁可本地红，也别等上传完才发现。

## 版本号从哪来

`Cargo.toml` 的 `version` → `CFBundleShortVersionString`；`CFBundleVersion` 默认取
时间戳（`BUILD_NUMBER=42` 可覆盖，必须是单调递增的字符串）。

**为什么不写死在 Info.plist 里**：写死必然漂移 —— 改了 `Cargo.toml` 忘了改 plist，
就会出现「关于本机显示 0.1.0、检查更新却按 0.2.0 比」这种最难查的错。

## 凭据在哪

签名身份与公证凭据都是**机器级的秘密**，仓库里一个字都不放：

| 要什么 | 在哪 |
|---|---|
| 签名身份 | 登录 Keychain（`security find-identity -v -p codesigning`） |
| 公证 profile | Keychain，名字 `audio-workshop-notary`（怎么建见下） |
| Apple ID / App 专用密码 / Team ID / p12 备份 | `gqf2008/sqb-private`（本地 `/Volumes/DataExt/GitHub/sqb-private`），见其 `accounts/05-ssh-certs.md` |

```sh
# 建 profile（值从 sqb-private 取，别写进任何仓库）
xcrun notarytool store-credentials audio-workshop-notary \
  --apple-id <apple-id> --team-id <team-id> --password <app-specific-password>
# 验证（联网，会列出历史提交）
xcrun notarytool history --keychain-profile audio-workshop-notary
```

公证复用机器级通用脚本 `~/scripts/notarize.sh`（`NOTARIZE_SH=` 可覆盖）——**不在本仓库复制
一份**，否则两处实现要同步改。注意它的默认 profile 是 `voicecall-notary`，本项目必须显式传。

## 两个容易踩的点（都是实测踩出来的）

### 1. `disable-library-validation` 是必须的

人声分离走 `ort` → ONNX Runtime。**动态链接泄漏时期**（把 homebrew 的 dylib 链进发布包）开了
hardened runtime 后，macOS 拒绝加载**不是本 Team ID 签的**库：

```
dyld: Library not loaded: /opt/homebrew/opt/onnxruntime/lib/libonnxruntime.1.dylib
Reason: mapping process and mapped file (non-platform) have different Team IDs
```

debug 版没签名，所以这个问题**只在打包后才暴露**。见 `packaging/entitlements.plist`
（`disable-library-validation` 保留在 entitlements 里作防御）。

**这个 dyld 报错已是历史**：现在发布包强制**静态链接**——`package.sh` 强制
`LIBONNXRUNTIME_NO_PKG_CONFIG=1` 并用 otool 做硬门禁（出现任何非系统库直接红），CI 的
macOS job 对 `.app` 里的两个二进制同样断言（见下文「两个必须记住的前提」第 2 条）。
**目标机不再需要安装 onnxruntime**（不用 `brew install`，也没有那 ~90 个 homebrew dylib
要逐个 re-sign 的问题）。

### 2. DMG 要单独公证一次，而且在 `.app` 装订之后重建

用户拿到的是 DMG，双击挂载时 Gatekeeper 判的是 **DMG 自己**的签名与票据。只公证 `.app`
的话，实测 `spctl -a -t open` 给出 `rejected / no usable signature`。

而且 DMG 必须**在 `.app` staple 之后**再造一次，否则装进去的是没票据的那份 `.app`。
（`release.sh` 里第 2 步就是为此存在的；DMG 生成逻辑抽在 `packaging/make_dmg.sh`。）

## 验收记录（2026-09-18 本机实测）

```
cargo build --release            → 退出 0（首编 13m32s）
codesign --verify --strict       → valid on disk / satisfies its Designated Requirement
notarytool submit (.app)         → Accepted  （id 36301cd1-a6ac-4f55-859e-0f8d481c40a3）
notarytool submit (.dmg)         → Accepted  （id d82cf0c1-40fb-4e6f-98d1-7b6c15944bd2）
stapler staple (.app / .dmg)     → worked
spctl --assess --type execute    → accepted (source=Notarized Developer ID)
spctl -a -t open (DMG)           → accepted (source=Notarized Developer ID)
启动 .app（装订后）              → 进程存活；用户 settings.json 的 sha256 与 mtime 未变
```

## 已知限制（别按"能发给所有人"理解）

- **三平台产物自 v0.1.4 起发布**：macOS `.dmg`、`linux-x64.tar.gz`、`windows-x64.zip` +
  `windows-x64-setup.exe`（v0.1.4 Release 的资产实测即这四类，见下文「各平台的产物形态」；
  平台移植过程见 `docs/m3-platform-status.md`）。
- **目标机不需要安装 onnxruntime**：人声分离的 ONNX Runtime 已静态链接进包（见上文第 1 点）。
- **检查更新（发现新版本 + 打开发布页）已端到端验过**（2026-09-18）：仓库 public（匿名 API 200）+
  正式 Release → `releases/latest` 实测 **200**。真链路用例 `real_default_manifest_is_parseable`
  拿到过 tag/url/**sha256**/**size**（见 `docs/update.md`）；**自动下载/静默安装不在 v1**。
- **未做公证后的"全新机器"验证**：本机验证覆盖了签名/公证/装订/启动，但没有在
  一台干净机器上试过。
- **当前正式 Release 是 `v0.1.7`**（`v0.1.0` 起沿用同一套步骤，见下面一节）。
  其中 v0.1.5 的 macOS 资产未签名（发布漏了第 4 步的公证替换，用户下载会被 Gatekeeper 拦），
  v0.1.6 起修正。

## 怎么发一个版本

按这个顺序做，每一步都实测过（`v0.1.0` 就是这么发出去的）：

```sh
# 1) 改版本号并让锁文件跟上（Info.plist 与 DMG 文件名都从它来，见上文「版本号从哪来」）
$EDITOR Cargo.toml            # version = "X.Y.Z"
cargo build --release         # 改了版本号必须重编一次，Cargo.lock 才会跟上
git add Cargo.toml Cargo.lock && git commit -m "chore(release): X.Y.Z"
git push origin main

# 2) 打包 + 签名 + 公证 + 装订（.app 与 DMG 各一次公证）
#    **必须在第 1 步之后**：release.sh 读 Cargo.toml 决定 DMG 文件名与 Info.plist 版本号
./release.sh                  # 需要 Keychain profile audio-workshop-notary

# 3) 打 tag 并推到**两个**远端（walgit 与 GitHub）
#    GitHub 的 tag 触发 release 流水线：三平台构建完成后**自动创建** Release
git tag -a vX.Y.Z -m "音频作坊 vX.Y.Z"
git push origin vX.Y.Z
git push github vX.Y.Z

# 4) **等 release 流水线绿**，再用第 2 步的本地公证 DMG 覆盖 CI 上传的那份
#    （CI 没有签名凭据，它上传的 macOS DMG 未签名未公证；原因见「三平台发布流程」后的说明）
gh release upload vX.Y.Z dist/AudioWorkshop-X.Y.Z.dmg --clobber \
  --repo gqf2008/audio-workshop

# 5) 核实「检查更新」这条链路真的通（会真打 GitHub API）
cargo test --bin audio-workshop real_default_manifest -- --ignored --nocapture
```

### 三个坑（都踩过）

1. **资产名必须 ASCII**：`音频作坊-0.1.0.dmg` 上传后会被 GitHub 平台 sanitize 成
   `-0.1.0.dmg`（中文剥掉、下载链接跟着坏）。`package.sh` 里有 `assert_ascii` 拦这一条。
2. **DMG 要在 `.app` 装订之后重建**：否则装进去的是没有公证票据的那份 `.app`。
   `release.sh` 的第 2 步就是为此存在。
3. **公证要显式传 entitlements**：通用脚本会重新签名，不带 `--entitlements` 会把
   `disable-library-validation` 洗掉 —— 结果是"公证过了但一启动就 dyld 报错"。

---

# 随包推理引擎（2026-09-19 起）

从这一版开始，安装包里**自带 `audiocpp_server`**：用户下载即用，不需要自己编译、也不需要
先跑一个服务。壳在启动时按下面的顺序决定用哪个服务（实现见 `src/engine_supervisor.rs`）：

1. 用户**显式**配了地址（`AW_SERVER`，或全局设置里写了 host/port）→ 一律不拉内置引擎
   （可能连的是别的机器，不能自作主张）；
2. 该地址上已有服务在响应 `/health` → 复用；
3. 否则拉起包内引擎（只在回环地址、且引擎文件确实存在时）。

## 引擎从哪来

引擎不在这条流水线里编（它是 C++/CMake 工程，要 Metal/CUDA 工具链与 2 小时级 runner），
产物由上游那套流水线出，这里只按 `engine-lock.json` **校验 sha256 后取件**：

```json
{
  "source": { "repo": "0xShug0/audio.cpp", "tag": "v0.8.2", "base": "0xShug0/audio.cpp v0.8.2" },
  "artifacts": { "macos-arm64": { "asset": "...", "sha256": "..." } }
}
```

**2026-09-24 起取的是上游官方 `v0.8.2`**：本仓库此前维护的 fork `gqf2008/audio.cpp`
（tag `v0.8.2-metalbf16` = 上游 v0.8.1 + 三个 **Metal BF16** 补丁：BreezeTTS 2 的 bf16 激活与
bf16 KV cache，含 `f16<->bf16` 拷贝内核）**已退役**——那些补丁作为上游 PR #554 于 2026-09-19
并入官方，官方 `v0.8.2` 自带这份能力，所以随包取件回到上游官方产物。
改引擎就是改这个文件，然后**重跑三平台验收**。

## 各平台的产物形态

| 平台 | 产物 | 引擎落点 |
|---|---|---|
| macOS | `AudioWorkshop-<v>.dmg` | `<App>.app/Contents/Resources/engine/audiocpp_server` |
| Windows | `...-windows-x64.zip` + `...-windows-x64-setup.exe`（per-user Inno Setup，免 UAC） | `engine\audiocpp_server.exe`（exe 同级） |
| Linux | `...-linux-x64.tar.gz` | `engine/audiocpp_server`（exe 同级） |

## 引擎不是"一个二进制"：同级运行库必须一起随包（2026-09-24 实测）

上游归档里，`audiocpp_server` **旁边**还摆着它要加载的运行库。原来 `fetch_engine.sh` 只解出
「二进制 + LICENSE」，结果是 **Linux/Windows 的包里引擎根本起不来**（macOS 那份只链系统框架，
所以本机一直没暴露）。现在取件改成"二进制 + LICENSE + 同级运行库"，并在取件末尾跑
`tools/check_engine_deps.py` —— 它按**二进制的真实依赖**（PE 导入表 / ELF `DT_NEEDED` /
`otool -L`）判，凡是系统不提供的依赖都必须在引擎目录里找到同名文件：

| 平台 | 引擎的真实依赖 | 缺了会怎样 |
|---|---|---|
| Windows | `MSVCP140.dll` / `VCRUNTIME140.dll` / `VCRUNTIME140_1.dll` / `VCOMP140.DLL` / `MSVCP140_CODECVT_IDS.dll`（PE 导入表实测，上游随 zip 提供） | 启动即"找不到 MSVCP140.dll" —— 而本仓库对外承诺"用户不用装 VC++ Redist" |
| Linux | `libggml.so.0` / `libggml-base.so.0`（`DT_NEEDED`，且 `RUNPATH=$ORIGIN`） | `error while loading shared libraries: libggml.so.0` |
| macOS | 只有 `/usr/lib`、`/System` 系统库 | 无（属"没有同级依赖"的正例，不是"不用检查"） |

**Linux 真机/容器实测（2026-09-24，colima + `ubuntu:24.04` amd64）**：

```
# 取件后的 engine/ 目录（含 libggml*.so*）
audio.cpp 0.8.2 / git: 4d88768f 2026-09-23 / build: Release, gcc 13.3.0, Linux x86_64 / backends: cpu   rc=0
# 只放二进制的旧形态
./audiocpp_server: error while loading shared libraries: libggml.so.0: cannot open shared object file   rc=127
```

**Linux 目标机要求（随包引擎的编译基线，实测）**：glibc **≥ 2.38** 与 `libgomp1`
（引擎用 `debian:bookworm-slim`＝glibc 2.36 起不来：`version GLIBC_2.38 not found`、
`GLIBCXX_3.4.32 not found`；裸 `ubuntu:24.04` 缺 `libgomp.so.1`，装上才起来）。
即 Ubuntu 24.04+ / Debian 13+ 一路可用；更老的发行版需要自行升级或换用系统里的引擎。
（同类判据见 `LESSON_随包第三方工具需校验minOS或glibc及依赖与签名状态.md`。）

## 两个必须记住的前提

1. **引擎没有模型就拒绝启动**（上游 `app/server/config.cpp:275`，实测）——所以"装完即用"
   仍然要求用户先下载至少一个模型。壳会在**模型下载完成后**自动拉起引擎（不要求重启应用）。
   一个模型都没有时，壳如实显示"没有服务"，不假装能跑。
2. **人声分离的 ONNX Runtime 走静态链接**（`LIBONNXRUNTIME_NO_PKG_CONFIG=1`）。
   `ort-sys` 的 build.rs 是"pkg-config 优先 → 失败才下载官方预编译包"，开发机装了
   homebrew onnxruntime 时会静默把它链进发布包 → 用户机器一启动就 dyld 报错、得自己
   `brew install` 才能跑（强制这个环境变量就是为了拦这条漏链）。
   `package.sh` / `packaging/package_linux.sh` 里有硬门禁（otool / ldd）：出现任何非系统库直接红；
   CI 的 macOS job 同样断言。（README 的「发行包现状」与上文「两个容易踩的点」第 1 点同口径。）

## 三平台发布流程

命令与顺序以「怎么发一个版本」为**唯一出处**（要改任何一步只改那一处）。本节曾经与它并存
第二份命令块且互相矛盾——v0.1.5 就是照着漏掉第 4 步的那份发的，结果 macOS 资产未签名未公证
（`codesign: not signed at all`、`spctl: rejected / source=no usable signature`）。本节只解释
为什么第 4 步不能省：

### 为什么第 4 步不能省：CI 的 macOS 产物**没有签名**

CI 的 macOS job 只跑 `./package.sh`，而签名身份与公证凭据都是机器级的秘密（Keychain 里、
不在 CI）——那边上传的 DMG 实测 **not signed at all**（`.app` 只是 ad-hoc 签），
`gh release download` 下来实测：

```
spctl -a -t open --context context:primary-signature -v AudioWorkshop-X.Y.Z.dmg
→ rejected / source=no usable signature
```

用户拿到的就是 Gatekeeper 直接拦下的包。`gh release upload --clobber` 用本地
`./release.sh` 的产物（已签名 + 公证 + 装订）覆盖同一个资产名即可，覆盖后复查：

```sh
gh release download vX.Y.Z -p "*.dmg" -D /tmp/rel -R gqf2008/audio-workshop --clobber
spctl -a -t open --context context:primary-signature -v /tmp/rel/AudioWorkshop-X.Y.Z.dmg  # 期望 accepted / Notarized Developer ID
xcrun stapler validate /tmp/rel/AudioWorkshop-X.Y.Z.dmg                                  # 期望 The validate action worked!
```

v0.1.4 发布时实测就是这条：CI 资产 `rejected`，覆盖后 `accepted (source=Notarized Developer ID)`。
v0.1.5 则是**漏掉这条**的反例：Release 里的 DMG `not signed at all`、`stapler validate` 无票据，
用户下载会被 Gatekeeper 拦——所以第 4 步不是可选项。
Windows/Linux 资产由 CI 直接出没问题（Windows 侧另有 `tools/check_windows_icon.py` 的
图标 + GUI 子系统断言兜底）。

### 这一块踩过的坑

1. **Windows 上必须显式设 UTF-8**：`cargo metadata` 的 UTF-8 输出经管道进
   `ConvertFrom-Json` 时，Windows PowerShell 按系统代码页解码，报
   `UnicodeDecodeError: 'charmap' codec can't decode byte 0x8f`。
   三处一起设：`[Console]::OutputEncoding`、`$OutputEncoding`、读 Cargo.toml 用 `ReadAllText(..., UTF8)`。
   只 `chcp 65001` 不够（子进程输出仍按旧编码解码）。
2. **嵌套可执行文件必须先签**：引擎是 `.app` 里的嵌套二进制，顺序必须是"先签引擎、再签
   `.app`"，反了公证报 `nested code is not signed`。
3. **壳被强杀会留孤儿引擎**：`SIGTERM`/崩溃时 `ui.run()` 不返回、`Drop` 不执行，实测包内
   引擎继续占着端口。现在壳侧另起一个监视进程（`--engine-monitor`），每秒探壳是否还在，
   壳没了先 SIGTERM、宽限 1s 再 SIGKILL。Windows 侧暂为空实现（`kill(pid,0)` 无等价物），
   P1 用 Job Object 补。
4. **`LSMinimumSystemVersion` 跟引擎走**：上游产物的 `minos` 实测 13.3，比壳自己需要的
   12.0 高，所以取 13.3；改了引擎要同步核 `otool -l` 的 `LC_BUILD_VERSION`。

---

# Windows 包（Inno Setup，2026-09-19 真机反馈后重做）

`v0.1.3` 的 Windows 安装包在真机上暴露了五个问题，逐条对应的修法如下（都在本仓库内）：

| 真机现象 | 根因 | 修法 |
|---|---|---|
| 启动多一个控制台黑窗 | 二进制是 console 子系统 | `src/main.rs` 的 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`（debug 保留控制台，否则 `cargo test` 输出会被吞） |
| 应用没有图标 | exe 里没有 PE 图标资源（资源管理器/任务栏只看这个） | `build.rs` 用 embed-resource 把 `assets/icon.ico` 编进资源段 |
| 桌面图标也没有 | 快捷方式没指定图标（旧 NSIS 脚本没写 `IconFile`） | `.iss` 的 `SetupIconFile` + `[Icons].IconFilename` + 图标随包装一份 |
| 中文乱码 | `.nsi` 是不带 BOM 的 UTF-8，makensis 按系统 ANSI（中文 Windows = GBK）解释源码 | 换 Inno Setup 6 —— `.iss` 默认按 UTF-8 读（BOM 可有可无）；本文件内的中文写坏就会立刻看出来 |
| 下载模型界面卡死 | ① 每读一跳就推一条进度并重建整张下载表（实测本地 4 MiB 就 512 条）；② 每次重建都在 UI 线程 spawn 一个 `powershell` 查物理内存 | ① `download::PROGRESS_INTERVAL`（100 ms）节流，状态变化不节流；② `model_sources::physical_memory_bytes()` 进程内只探一次 + `CREATE_NO_WINDOW`（GUI 子系统下不会再闪黑窗） |

## 装什么、怎么装

产物形态见上文「各平台的产物形态」表：Windows 出
`AudioWorkshop-<v>-windows-x64.zip`（绿色版）与 `AudioWorkshop-<v>-windows-x64-setup.exe`。
安装器形态与 `../abb` 的 `app-assets/ABB.iss` 一致：**per-user**（`{localappdata}\Programs\AudioWorkshop`，
免 UAC）、`PrivilegesRequired=lowest`、卸载只删自己。目录名与旧 NSIS 版相同，**升级是原地覆盖**；
`.iss` 里额外删掉旧版的 `uninstall.exe` 与手写的卸载注册表项，免得"应用和功能"里出现两条。

```powershell
# 本机（Windows）打包
pwsh -File packaging/package_windows.ps1 -MakeInstaller   # 需要 ISCC.exe（choco install innosetup）
```

`packaging/windows-installer.iss` 的五个变量都由脚本用 `/D` 传入（`MyAppVersion` / `SourceDir` /
`IconFile` / `OutDir` / `LangFile`，一律绝对路径），文件里的 `#ifndef` 默认值只给手工跑留。

### 中文向导的消息文件（Inno 不带）

**Inno Setup 官方安装包不含中文消息文件**（2026-09-19 CI 实测：choco 装的 Inno Setup 6.7.1
里没有 `Languages\ChineseSimplified.isl`），所以 `package_windows.ps1` 会：

1. 先在 `Program Files` / `Program Files (x86)` / `LOCALAPPDATA\Programs` 下的
   `Inno Setup 6\Languages\` 找现成的那份（版本与安装的 Inno 匹配，优先用）；
2. 都没有就从 `jrsoftware/issrc` 取一份放到 `dist\setup-lang\`（构建产物目录，不进仓库、
   不进发布资产），取不到就按失败处理（不静默降级成英文向导）；
3. 把这份 **.isl 的绝对路径**按 `/DLangFile=` 交给 ISCC。

离线环境或想固定某一份翻译时用 `-InnoLangFile <path>` 显式指定。

为什么传绝对路径、而不是让脚本写 `compiler:Languages\...`：choco 装的 Inno 在 PATH 上的
`ISCC.exe` 是 **shim**，"ISCC 旁边"不是安装目录 —— 第一版就是照 `Split-Path $iscc -Parent`
去补文件，结果 .isl 落进了 `C:\ProgramData\Chocolatey\bin\Languages\`，ISCC 仍去
`Program Files (x86)` 找，报 `Couldn't open include file`（CI 日志实测）。

## 三条守卫（都能红，别删）

1. **图标真的嵌进 PE 了吗 + 是不是 GUI 子系统** ——
   `python tools/check_windows_icon.py <exe> [<setup.exe>]`。
   `release.yml` 的 Windows 任务与 `package_windows.ps1` 都会跑；直接读 PE 资源目录，
   不看"能不能提取出图标"（`ExtractAssociatedIcon` 在没图标时会返回系统默认图标，那是假绿）。
   同时断言可选头的 Subsystem 是 2（WINDOWS_GUI）—— 这条对应"启动多一个控制台黑窗"，
   同样是"没人报错、只有用户看得见"。2026-09-19 本机实测：带图标 + GUI 的 PE 过；
   不带图标的红；带图标但控制台子系统的红（`--allow-console` 可显式放宽）。
2. **`.iss` 必须是 UTF-8** —— `file packaging/windows-installer.iss` 期望 `UTF-8 Unicode text`。
   存成 GBK 会让中文应用名/快捷方式名又变乱码（旧 NSIS 版就是这么坏的）。
3. **嵌不进去必须红** —— `build.rs` 的 `embed_windows_icon` 用的是
   `manifest_required()`（不是 `manifest_optional()`）：找不到资源编译器时直接编译失败。
   2026-09-19 实测（`RC=/nonexistent/rc.exe cargo build --target x86_64-pc-windows-gnu`）：
   `嵌入 Windows 图标资源失败（compilation not attempted: Couldn't execute /nonexistent/rc.exe）`，
   退出码 101。改成 optional 就会静默产出没图标的包 —— 那正是用户抱怨的那个包。

`tools/check_windows_icon.py` 自带阳性/阴性对照：`tools/tests/test_check_windows_icon.py` 用本机
mingw-w64 真编两个 PE（一个带图标、一个不带），前者必须过、后者必须红；没有 mingw 的机器会打印
原因后跳过（发布流水线对真产物跑同一条命令，那里是硬失败的步骤）。

## 必须知道的代价

- **release 版没有控制台，诊断也写不出去**：`eprintln!` 的引擎启动/自愈信息在 GUI 子系统下会被
  丢弃（std 把 Windows 的 `ERROR_INVALID_HANDLE` 当"丢弃"处理，不会 panic）。用户可见的状态在
  状态栏与 `/health`；要诊断就在 Windows 上跑 `cargo run`（debug，带控制台）。
- **macOS/Linux 的构建也多编一个 build-dep**（embed-resource）。理由：`embed_resource::` 必须能在
  build.rs 里写出来，而"要不要资源"是**目标**属性，运行时按 `CARGO_CFG_TARGET_OS` 判。
- **Windows 打包可能需要联网**：Inno 不带中文消息文件（见上文「中文向导的消息文件」），
  本机找不到时 `package_windows.ps1` 会去 `jrsoftware/issrc` 取一份；离线环境用
  `-InnoLangFile` 指定现成的那份。
