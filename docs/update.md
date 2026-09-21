# 检查更新（M4-P8 的另一半）

> 产品口径：`docs/product-plan.md` §4.2 的 P8「自动更新与备份」。
> 备份那一半见 `docs/backup.md`。
> **本批的 v1 只做「知道」与「拿到」**：比对版本 + 给发布页链接（边界见文末）。

## 它解决什么

应用此前没有任何版本出口：用户不知道自己跑的是哪一版，也不知道有没有新版。
发行走 GitHub Releases（`github.com/gqf2008/audio-workshop`），但没有检查入口。

## 怎么用

设置抽屉 → 「更新」→（可留空地址）→「检查更新」。

- **当前版本**直接显示在区里，来自编译期常量 `env!("CARGO_PKG_VERSION")`——与比对用的是**同一个值**，
  不会出现"界面写着 0.1.0、比对按别的版本比"。
- **清单地址留空 = 官方地址** `https://api.github.com/repos/gqf2008/audio-workshop/releases/latest`。
  内网/镜像可以把地址换成自己的清单（只填一次，点检查时写进 `settings.json` 的 `update_url`）；
  把框清空再检查，就回落官方地址（清空时存回 `null`，是"跟着默认走"而不是"钉住当前默认值"）。
- 有新版时状态行说「有新版本 …（安装包大小）——点「打开发布页」看更新说明」，
  旁边的「打开发布页」按钮才可点。没有新版时说「已是最新版本 <当前版本>」。

检查走**后台线程**（界面不冻），检查中按钮显示「检查中…」且禁用（防连点）。

## 清单的两种形状

默认地址返回的是 GitHub Release API 的响应，写法与自建清单不同，两种都接受：

```jsonc
// 1. GitHub releases/latest（默认）
{ "tag_name": "v0.2.0", "body": "更新说明", "html_url": "https://github.com/…/releases/tag/v0.2.0",
  "assets": [ { "name": "AudioWorkshop-0.2.0-linux-x64.tar.gz", "size": …, "digest": "sha256:…" },
              { "name": "AudioWorkshop-0.2.0.dmg", "size": …, "digest": "sha256:…" },
              { "name": "AudioWorkshop-0.2.0-windows-x64-setup.exe", "size": …, "digest": "sha256:…" },
              { "name": "AudioWorkshop-0.2.0-windows-x64.zip", "size": …, "digest": "sha256:…" } ] }

// 2. 自建/内网镜像清单
{ "version": "0.2.0", "notes": "更新说明", "url": "https://…/releases/0.2.0",
  "sha256": "…", "size": 50331648 }
```

**必填只有 `version`（或 `tag_name`）与 `url`（或 `html_url`）**——前者决定"是不是新版"，
后者是「打开发布页」唯一能点的东西。`notes` / `sha256` / `size` 只是展示与将来的校验信息，
缺了不影响判断，所以缺省而不是报错；但**给了却类型不对**同样报错（不静默吞掉类型错误）。

缺承重字段 / 字段类型不对 / 顶层不是对象 / 根本不是 JSON —— 一律给
**指名道姓 + 下一步**的错误（例：`发布清单缺少字段 url（顶层字段：tag_name, body）——这个地址可能不是发布清单`）。
**绝不在这些情况下说「已是最新」**：把"没读到"伪装成"没有新版"是这一类功能最危险的静默失败。

**GitHub 形状的 `assets` 是多平台资产数组**（顺序不保证，字母序第一个往往是 Linux 包）。
展示用的 `sha256` / `size` 取的是**按当前平台挑中的那个资产**：macOS → 名字以 `.dmg` 结尾
（大小写不敏感）、Windows → 优先 `-setup.exe`、其次 `.zip`、Linux → `.tar.gz`；
**挑不到就回落第一个资产**（实现 `src/update.rs::pick_asset`，单测把三平台各挑一遍）。
所以 macOS 上状态行显示的是 dmg 的体积，而不是排第一的 Linux 包的体积。

## 版本比对

- 按**数字段**比，不是字典序：`1.2.10 > 1.2.9`、`1.9.9 < 1.10.0`；
- 段数不同按 0 补齐：`1.2 == 1.2.0`；
- 支持 `v` / `V` 前缀（`v1.2.1`）；
- 数字段相同时，正式版 > 预发布版（`1.2.0 > 1.2.0-beta.1`）；两边都是预发布按点分段比
  （数字段按数字，其余按字典序）；`+构建元数据` 不参与比较；
- **非法版本串一律报错**（当前版本或清单任一侧），不当作"不是新版"——`1.x`、`1..2`、``、`v` 都会报错。

## 网络失败要能分清是哪一种

状态行的失败原因按类分开，每条都给下一步：

| 情况 | 文案要点 |
|---|---|
| 域名解析失败 / 连接被拒 | 「连不上：…——检查网络/代理，或把「清单地址」换成内网镜像」 |
| 单次读超时 | 「超时：N 秒内没读到数据——网络慢或被墙，可换内网镜像清单」 |
| 代理连不上 | 「代理连不上：…——检查系统代理设置」 |
| HTTP 状态码 | 「服务器返回 HTTP 404——地址不对、仓库不是匿名可读（GitHub 对私有仓库也回 404）、或还没有 Release；确认「清单地址」指向一个匿名可读的已发布 Release」/ 403、429 提示可能被限流 |
| 清单坏了 | 「发布清单不是合法 JSON…」「发布清单缺少字段 `version`…」 |

**超时只设 connect（15s）与 per-read（30s），刻意不设 ureq 的总超时**：
总超时把 DNS+连接+读完 body 全算进去，慢网络/代理下会把一次本来能成功的正常响应整条判失败
（见 `LESSON_自动更新大文件下载须per-read超时且应用替换禁止cp_R嵌套.md`）。
这两条常量是 `src/update.rs` 的 `CONNECT_TIMEOUT` / `READ_TIMEOUT`，改它们要按 `RULE_阈值变更.md` 独立提交。

## 已知前提：仓库可匿名读 + 有正式 Release —— **两条都已满足**（2026-09-18）

默认地址走的是 **未认证**请求（不给 token）。现状实测：

```console
$ curl -s -o /dev/null -w '%{http_code}\n' https://api.github.com/repos/gqf2008/audio-workshop
200                     # 匿名可读：仓库已公开
$ curl -s -o /dev/null -w '%{http_code}\n' \
    https://api.github.com/repos/gqf2008/audio-workshop/releases/latest
200                     # 正式 Release v0.1.0 已发布（资产 AudioWorkshop-0.1.0.dmg）
```

**这条链路已经端到端验过**，不是"应该能"：`real_default_manifest_is_parseable`
（`#[ignore]`，默认不跑）真打了一次线上 API，拿到的正是：

```
线上最新 = v0.1.0（notes 1474 字，url .../releases/tag/v0.1.0，
           sha256 Some("be892dd6…"), size Some(10678738)）
```

跑法：

```console
cargo test --bin audio-workshop real_default_manifest -- --ignored --nocapture
```

注意 `releases/latest` 只认 **非 draft、非 prerelease** 的 Release —— 发成 draft 或
prerelease 时它照样 404，上面那条用例会红，所以发版别发成 draft。

> 历史留档（2026-09-17 → 2026-09-18）：当时仓库是 private，不带凭据的 API 一律 404。
> 用户 2026-09-18 选了「把仓库/Release 变成匿名可读」；同日转 public 并发出 v0.1.0。
> 另一条路（带凭据访问清单）仍然不做：把 token 塞进"检查更新"的默认路径会引入凭据分发/
> 存储问题，而 v1 的目标只是"知道有新版本"。

## 安全边界：发布页只认 http(s)

**清单是外部输入**（默认 GitHub API，也支持自建/内网镜像），清单里的 URL 是不可信输入。
「打开发布页」交给系统打开器时，地址先过 `update::is_http_url`
（**唯一一处判据**：清单校验、拉取、打开三处都走它）。`file://`、`/tmp/x`、自定义 scheme 一律拒绝——
否则一个被改过的清单就能让应用去打开本地路径/任意协议。

三平台打开器语义（`src/main.rs::open_external_url`，与这里的边界同源）：

- **macOS / Linux**：直接 `open` / `xdg-open`，URL 按 argv 参数传，不经 shell；
- **Windows**：`ShellExecuteW` 直接交给 Shell，**不经 cmd**——`cmd.exe /C` 会把 URL 里的
  `&` 等元字符再解析一遍（`https://x/?a=1&b=2` 会把 `b=2` 当命令执行，命令注入 + 常见 URL
  截断），argv 级别的转义在 cmd 这一层不成立。
- 除 http(s) 前缀外，含控制字符（`\n`/`\r`/`\0` 等）的地址也不是合法 URL，一并拒绝。

## 明确不做（边界）

- **不自动下载安装包、不静默安装、不重启应用**：v1 只到"打开发布页"，安装由用户自己走发行包。
- **不做增量更新、不做渠道/灰度**。
- **不校验安装包的 sha256**：清单里带了就如实显示（提示下一版下载时用它），v1 没有下载动作所以没有可校验的对象。
- **不缓存检查结果、不后台定时检查**：每次都是用户点一下查一次，不驻留、不联网后台跑。
- 清单里的 `notes` 目前只在 `Release` 里带着，没有专门的"更新说明"面板——状态行只显示版本与体积，
  说明内容要点「打开发布页」看。

## 实现位置

- 纯逻辑 + 一次只读 HTTP GET：`src/update.rs`（`parse_release` / `is_newer` / `check` / `UpdateCheck` / `Release`），
  单测在同一文件里（进程内 mock TCP 服务器，**不依赖真实外网**）。
- UI 接线：`src/main.rs`（设置抽屉的「更新」区、后台线程 `spawn_update_check`、`Msg::UpdateCheckDone`、
  `update_refusal`/`refresh_update_availability` 那份判据与它的 UI 投影）。
- 组件：`ui/dub_workbench.slint`（`WorkbenchDrawer` 的「更新」区）+ `ui/app.slint`（属性/回调转发）。
