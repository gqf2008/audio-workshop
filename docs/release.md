# macOS 打包与发版

> 2026-09-18。本文只写**实测过的**流程与边界。

## 一条命令

```sh
./release.sh          # 打包 → 签名 → 公证（.app 与 DMG 各一次）→ 装订
```

产物（`dist/`）：

| 文件 | 说明 |
|---|---|
| `音频作坊.app` | 已签名 + 已公证 + 已装订 |
| `音频作坊-<version>.dmg` | 拖进 Applications 的安装镜像，**自身也签了名、公证过、装订过** |

只要 `.app`（比如自己用、不对外分发）：`./package.sh` 即可 —— 它会签 Developer ID
（自动从 Keychain 挑），但不提交公证。

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
- **自动更新还没配好**：默认清单地址是私有仓库的 GitHub Release API，匿名访问必然 404
  （见 `docs/update.md`）。打包出来的 app 点「检查更新」会如实报 404，而不是假装成功。
- **未做公证后的"全新机器"验证**：本机验证覆盖了签名/公证/装订/启动，但没有在
  一台没装过 homebrew 的干净机器上试过（那台机器大概率会因为缺 onnxruntime 起不来）。
- **没有版本化的 Release 流程**：目前产物在 `dist/`，还没打 tag、没上传到任何 Release。
