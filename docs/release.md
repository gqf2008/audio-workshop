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
