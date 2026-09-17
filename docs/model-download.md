# 模型下载器（M4-P7）

> 2026-09-17。第一段（队列 / 校验 / 断点续传）见 §3–§4，对应 issue thread
> `cc-ai-audio-workshop-model-download`；第二段（**下载源**）见 §1–§2，对应
> `cc-ai-audio-workshop-model-sources`。
> 只写**已经实现**的行为与边界，不写愿景。按机器推荐量化档 / 并行 / 镜像仍明确没做。

## 1. 入口与数据来源

「全局设置」抽屉的 **模型** 区是「可下载模型」列表，**每一行都会出现**，三类：

| 行的状态 | 含义 | 按钮 |
|---|---|---|
| 未下载 / 排队 / 下载中 / 校验中 / 已完成 / 失败 / 已取消 / 已就位 | 真的能下（或已经下过） | 下载 / 取消 / 重下 / 重试 |
| **没有下载源** | 下不了，且**写明为什么** | **不显示**（不给点了必然失败的入口） |

下载地址有两个来源，**按行的 `detail` 前缀如实标出**：

1. `来源：server.json（服务清单）` —— 服务清单里**显式**给了 `url`，**以服务为准**
   （服务侧可以覆盖内置清单：内网镜像、自建仓库）。
2. `来源：内置下载清单` —— 服务清单没给 `url` 时，查随应用分发的
   `config/model-downloads.json`（`include_str!` 打进二进制）。

### 1.1 内置清单从哪来、怎么生成

上游 `audio.cpp` 的 `model_specs/*.json` 是下载链接的 source of truth（CHARTER §6），
它不在用户机器上，所以由 `tools/gen_model_downloads.py` 投影成一份**提交进仓库**的
`config/model-downloads.json`：

```sh
python3 tools/gen_model_downloads.py           # 生成（AUDIOCPP_DIR 可覆盖上游位置）
python3 tools/gen_model_downloads.py --check   # 与上游比对，不一致退出 1（发现"改了 spec 忘了重生成"）
```

- **不联网**：`size` / `sha256` 一律留空（生成必须离线可复现），下载器退化成按响应
  `Content-Length` 校验大小。
- **逐字节稳定**：固定键序 + 固定缩进 + 行尾换行；同一份输入跑两次 `diff` 为空。

### 1.2 映射规则：**按落点，不按 family**

产品 id → 上游包的映射**不是**「family → 上游默认包」。上游 `packages[].default` 在几个
本产品登记过的 family 上指向**别的权重**（对着 spec 实测）：

| 产品 id | family | 上游 default 包 | 本产品 `path` 指向 | 只按 family 会下到 |
|---|---|---|---|---|
| audio8-tts | audio8_tts | 0.6B q8_0 | 0.6B | ✅ 一致 |
| index-tts2 | index_tts2 | **2.0** q8_0 | **2.5** | ❌ 2.0 |
| qwen3-asr | qwen3_asr | **1.7B** q8_0 | **0.6B** | ❌ 1.7B |
| stable-audio-small-music | stable_audio | **medium** q8_0 | **small-music** | ❌ medium |

（这正是 `tools/model_fetch.py` 现有口径在本产品上会取错的那几处——生成脚本因此改用
产品自己的 ground truth，理由写在脚本头部注释里。）

规则：`config/models.schema.yaml` 里该模型 `path` 去掉 `${models_root}/` 的那一段，
必须等于包的 `target_directory` + （`files[i]` 去掉 `strip_prefix`）——后者与上游
`model_manager_v2.py::stripped_path` 逐字一致，就是上游决定文件落在哪一层的规则。

- 相等 → 就是它（能区分 0.1B/0.6B、2.0/2.5、medium/small-music）。
- `path` 正好等于 `target_directory`（gen 类模型 path 指目录）→ 目录匹配；同处多个量化档
  时按 `precision_preference` → `q8_0` 的顺序挑，**挑的理由写进该行的 `note`**。
- 匹配不上 / 上游包没有公开下载源（`kind: unsupported`，如 `audio8-asr` 是
  CC-BY-NC-4.0 需本地转换）→ `status = "no-source"`，界面显示原因，**不猜地址**。

### 1.3 服务清单的字段

P7 给每个模型加了三个可选字段：

```json
{
  "id": "audio8-tts",
  "task": "tts",
  "family": "audio8",
  "path": "/Users/me/models/audio8-tts.gguf",
  "url": "https://example.com/audio8-tts.gguf",
  "sha256": "……64 位十六进制……",
  "size": 2147483648
}
```

- **`url` 缺失**：退到内置下载清单；内置清单也没有 → 该行显示「没有下载源」+ 原因，**不给按钮**。
- **`sha256` 缺失**：退化成按大小校验（见 §3）。
- **`size` 缺失**：按响应的 `Content-Length` 校验。

## 2. 下载到哪 + 落点校验

目标一律是 **`<模型目录>/<相对落点>`**，`<模型目录>` 取全局设置的「模型目录」
（`model_dir()`，默认 `<应用工作目录>/models`），目录不存在会自动创建。**相对落点**：

- 内置清单知道这个模型 → 用它的 `local_paths[0]`（上游 `target_directory + strip_prefix`
  的布局，与 `server.json` 的 `path` 同源）；
- 否则用服务清单 `path` 相对模型目录的那一段；再没有就取 `path` 的文件名、
  `url` 末段（剥 query），最后 `<id>.gguf`。

**落点校验（`⚠ 落点与 server.json 对不上`）**：算出来的落点与 `server.json` 声明的 `path`
按**真实路径**（解析 `..` 与软链）比，不一致时把两边都写在行上，例如：

```
来源：内置下载清单 · /Users/me/models/Qwen3-ASR-0.6B-GGUF/qwen3-asr-0.6b-q8_0.gguf
· ⚠ 落点与 server.json 对不上：清单 path 是 /Volumes/DataExt/models/…，
  本次会下到 /Users/me/models/… —— 下完服务仍可能加载不到（把「模型目录」指到清单所在的位置，
  或把清单 path 改成实际落点）
```

`path` 指目录（gen 类模型，如 `Stable-Audio-3-Small-Music-GGUF`）时，文件落在这个目录里
**不算冲突**。没写 `path` 就没有可比的声明，不报。

> 注意：下载只是把权重放到模型目录。要让 `audiocpp_server` 加载它，清单里的 `path` 得指向这个文件
> （或服务侧重新扫描）；本批不做"下载完自动改服务清单"。

## 3. 三个可核的落盘承诺

1. **先写 `<目标>.part`，校验通过才 `rename` 成正式文件**（提交点，同 `docs/robustness.md`）。
   正式路径要么没有，要么是完整的——中途失败/被取消都只留 `.part`。
2. **断点续传如实**：本地有 `.part` 时发 `Range: bytes=<已下载>-`。
   - 服务器回 **206** → 追加写，算续传，状态里写「从 N 字节断点续传」。
   - 服务器回 **200** → 说明它不认 Range，**从 0 重下**，状态里如实写「服务器不支持续传，已从 0 重新下载」——
     不会把本地旧字节当续传拼进去。
   - 服务器回 **416**（`.part` 比文件还大，常见于换源/文件变小）→ 丢弃 `.part` 重请求一次。
3. **校验失败丢 `.part`**：sha256 不匹配或大小对不上 → 删 `.part`、状态失败、带上期望值/实际值；
   正式路径从未被创建。取消**保留** `.part`（那是可续传的断点，不是半成品）。

另外发请求时钉了 `Accept-Encoding: identity`：透明 gzip 会让续传的字节偏移全错。

**416 补救（复核后修好的一处真 bug）**：`ureq` 把 4xx 当 `Err(Error::Status)` 返回，
早期 `send_get` 又把它直接翻成错误——于是"`.part` 比文件还大 → 丢弃重下"这条补救分支
是**死代码**，用户只会看到"服务器返回 HTTP 416"。现在 `send_get` 把 4xx/5xx 的响应本体
交回上层：416 走补救，其它 4xx/5xx 读 body 把原因带进错误文案（截 200 字）。

## 4. 队列语义

- 多条任务**串行**：一次只下一个，其余是「排队」。
- 单任务状态机：`排队 → 下载中 → 校验中 → 已完成 | 失败 | 已取消`。
- **可取消（协作式）**：点「取消」置一个原子标志，读循环下一轮就退出；
  排队中的任务在轮到时直接跳过，一个网络请求都不发。
  - **网络超时**：连接 15s、**单次 socket 读 30s**（`Timeouts`）。ureq 2 默认没有任何
    超时，读会一直阻塞——所以"取消"最多再等一个读超时/一个 64KiB 块就生效，不会无限等。
    只设读超时、不设 ureq 的总超时：总超时会把"下 2 GB 权重"整条判失败。
  - **取消未收尾期间不能重下**：取消是协作式的，旧任务可能还在写 `.part`。所以点取消
    **不摘** `download_ids`，界面继续显示「取消」，再点只会得到「正在取消，请稍候」；
    只有 worker 的终态快照才摘 id——避免两条任务抢同一个 `.part`（只给 size 不给 sha256
    的条目只按大小校验，"内容坏但长度恰好对上"会被当成好文件）。
  - 按钮文案：在跑 → 「取消」，已完成 → 「重下」，失败 → 「重试」，其余 → 「下载」。
- 进度按 `已下载 / 总字节` 上报（总长未知时退化成"已下载 X"）。
- 队列线程与 UI 之间走既有的 tick 消息泵（`Msg::DownloadUpdate`），
  该消息在 `message_ignores_revision` 白名单里——**下载权重与工程版本无关**，改稿不该把进度静默丢掉。

## 5. 验证

- 单测（`src/download.rs`，本地 mock HTTP 用 `std::net::TcpListener`，不引第三方 server）：
  续传（断言收到的 `Range` 头 + 最终字节）、服务器不支持 Range 时从 0 重下、
  416 丢弃坏断点重下、sha256 不匹配丢弃、大小不匹配丢弃、取消（开始前 / 下载中）、
  取消后 416 不再补发请求、服务端卡住时读超时生效、目标目录不存在时创建、
  队列串行**顺序**（第二条的 Queued 要晚于第一条的 Done）、排队中取消被跳过、
  `cancel` 如实区分 首次请求取消 / 已在取消中 / 其实已收尾。
  另有 UI 侧的点击语义测试（取消未收尾期间再点不会再排一条）。
- 生成脚本：输出逐字节稳定（同输入跑两次 `diff` 为空）、`--check` 能发现被改过的产物。
- 规划纯函数（`src/model_sources.rs`）四类各有能红的用例：
  **服务清单带 url 时以服务为准**、**gated / 没有源不给入口**、**内置清单读不出来 ≠ 没有源**、
  **映射歧义**（同一个 `audio8_tts` family 下 0.6B 有包、0.1B 没有；`index_tts2` 选 2.5 而非
  2.0；`qwen3_asr` 选 0.6B 而非 1.7B；`stable_audio` 选 small-music 而非 medium）。
  另有落点校验（清单 path 在模型目录外 → 报冲突；path 指目录 → 不报）。
- UI 冒烟：`AW_UI_STATE=drawer cargo run`（模型区渲染）、`AW_UI_STATE=downloads cargo run`
  （灌下载中/排队/已完成/失败/落点警告/没有下载源六种状态核对进度与按钮）、
  `AW_UI_STATE=model-sources cargo run`（**真机态**：按 `server.json ∪ 内置清单` 实渲，
  状态行直接报"几个有入口、几个标了没有源"）。
- 门禁：`cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`。

## 6. 本批没做（别按已实现宣传）

- **按机器推荐量化档**（读 RAM / 显存）——内置清单里**已经带上每个 family 的全部可下载
  量化档**（`packages[]`），但界面还不会按机器挑、也只给**单文件包**下载入口；
  多文件包（safetensors 等）不在本批范围。
- **下载完自动改服务清单 / 重启服务**：下载只是把文件放到模型目录（见 §2 的落点校验）。
- `size` / `sha256` 内置清单里**没有**（生成不联网），所以校验退化成按 `Content-Length` 核大小；
  要强校验只能由 `server.json` 显式给 `sha256`。
- **并行多任务**：现在刻意串行。
- **下载源镜像选择**；断点下载也没有"定时重试/弱网重试"。
- **下载完自动改服务清单 / 重启服务**。
- 下载**没有**走任务中心台账（不占配音/BGM 的 worker 队列），状态只在这块列表里。
- M1「基础下载器（免费）」与 P7「增强下载器」的许可边界未变：**发行版不含任何权重**，一律由用户侧下载。
