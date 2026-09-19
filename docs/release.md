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
就会出现「关于本机显示 0.1.0、自动更新却按 0.2.0 比」这种最难查的错。

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

人声分离走 `ort` → ONNX Runtime，链接的是 homebrew 的动态库。开了 hardened runtime 后，
macOS 拒绝加载**不是本 Team ID 签的**库：

```
dyld: Library not loaded: /opt/homebrew/opt/onnxruntime/lib/libonnxruntime.1.dylib
Reason: mapping process and mapped file (non-platform) have different Team IDs
```

debug 版没签名，所以这个问题**只在打包后才暴露**。见 `packaging/entitlements.plist`。

**这带来一个必须写清的前提**：当前发行版**要求目标机装有 onnxruntime**
（`brew install onnxruntime`）。把它连同依赖的 ~90 个 homebrew dylib 一起塞进 `.app`
是更大的工程（要逐个 re-sign、升级 onnxruntime 就得重来），暂不做。

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

- **只有 macOS 包**。Windows/Linux 安装包没有（M3 未达「第二个平台可自用」，见
  `docs/m3-platform-status.md`）。
- **目标机需要 onnxruntime**（见上文第 1 点）。
- **自动更新已端到端验过**（2026-09-18）：仓库 public（匿名 API 200）+ 第一个正式 Release
  `v0.1.0`（资产 `AudioWorkshop-0.1.0.dmg`）→ `releases/latest` 实测 **200**。
  真链路用例 `real_default_manifest_is_parseable` 拿到过 tag/url/**sha256**/**size**（见 `docs/update.md`）。
- **未做公证后的"全新机器"验证**：本机验证覆盖了签名/公证/装订/启动，但没有在
  一台没装过 homebrew 的干净机器上试过（那台机器大概率会因为缺 onnxruntime 起不来）。
- **只有一个正式 Release**（`v0.1.0`）。版本化流程见下面一节；`v0.1.1` 起沿用同一套步骤。

## 怎么发一个版本

按这个顺序做，每一步都实测过（`v0.1.0` 就是这么发出去的）：

```sh
# 1) 改版本号（Info.plist 与 DMG 文件名都从它来，见上文「版本号从哪来」）
$EDITOR Cargo.toml            # version = "X.Y.Z"
cargo build --release         # 让 Cargo.lock 跟上（改了版本号必须重编一次）

# 2) 打包 + 签名 + 公证 + 装订（.app 与 DMG 各一次公证）
./release.sh                  # 需要 Keychain profile audio-workshop-notary

# 3) 打 tag 并推到**两个**远端（walgit 与 GitHub）
git tag -a vX.Y.Z -m "音频作坊 vX.Y.Z"
git push origin vX.Y.Z
git push github vX.Y.Z

# 4) 建 Release 并上传 DMG（资产名必须 ASCII，见下文「三个坑」）
gh release create vX.Y.Z "dist/AudioWorkshop-X.Y.Z.dmg" \
  --repo gqf2008/audio-workshop --title "音频作坊 vX.Y.Z（macOS）" --notes-file /tmp/notes.md

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
  "source": { "repo": "gqf2008/audio.cpp", "tag": "v0.8.2-metalbf16", "base": "0xShug0/audio.cpp v0.8.1" },
  "artifacts": { "macos-arm64": { "asset": "...", "sha256": "..." } }
}
```

`v0.8.2-metalbf16` = 上游 v0.8.1 + 三个 **Metal BF16** 补丁（BreezeTTS 2 的 bf16 激活与
bf16 KV cache，含 `f16<->bf16` 拷贝内核）。改引擎就是改这个文件，然后**重跑三平台验收**。

## 各平台的产物形态

| 平台 | 产物 | 引擎落点 |
|---|---|---|
| macOS | `AudioWorkshop-<v>.dmg` | `<App>.app/Contents/Resources/engine/audiocpp_server` |
| Windows | `...-windows-x64.zip` + `...-setup.exe`（per-user NSIS，免 UAC） | `engine\audiocpp_server.exe`（exe 同级） |
| Linux | `...-linux-x64.tar.gz` | `engine/audiocpp_server`（exe 同级） |

## 两个必须记住的前提

1. **引擎没有模型就拒绝启动**（上游 `app/server/config.cpp:275`，实测）——所以"装完即用"
   仍然要求用户先下载至少一个模型。壳会在**模型下载完成后**自动拉起引擎（不要求重启应用）。
   一个模型都没有时，壳如实显示"没有服务"，不假装能跑。
2. **人声分离的 ONNX Runtime 走静态链接**（`LIBONNXRUNTIME_NO_PKG_CONFIG=1`）。
   `ort-sys` 的 build.rs 是"pkg-config 优先 → 失败才下载官方预编译包"，开发机装了
   homebrew onnxruntime 时会静默把它链进发布包 → 用户必须自己 `brew install`。
   `package.sh` / `package_linux.sh` 里有硬门禁：出现任何非系统库直接红。

## 三平台发布流程

```sh
# 1) 本地（macOS）：打包 + 签名 + 公证 + 装订
./release.sh                       # 引擎已随包；公证 profile: audio-workshop-notary

# 2) 三平台构建 + 打包（CI，dry-run 不带发布）
gh workflow run release --repo gqf2008/audio-workshop --ref main -f publish=false

# 3) 打 tag → 触发正式发布（会创建 GitHub Release 并附三平台产物）
git tag -a vX.Y.Z -m "音频作坊 vX.Y.Z"
git push github vX.Y.Z

# 4) 核实"检查更新"链路
cargo test --bin audio-workshop real_default_manifest -- --ignored --nocapture
```

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
