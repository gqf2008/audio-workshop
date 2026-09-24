# 模型下载器（M4-P7）

> 2026-09-17。第一段（队列 / 校验 / 断点续传）见 §3–§4，对应 issue thread
> `cc-ai-audio-workshop-model-download`；第二段（**下载源**）见 §1–§2，对应
> `cc-ai-audio-workshop-model-sources`；第三段（**体积 + 按机器推荐量化档**）见
> §1.3–§1.4，对应 `cc-ai-audio-workshop-quant-recommend`。
> 只写**已经实现**的行为与边界，不写愿景。

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
python3 tools/gen_model_downloads.py                 # 离线生成：体积/哈希沿用盘上已有的值
python3 tools/gen_model_downloads.py --fetch-sizes   # 联网补体积后生成
python3 tools/gen_model_downloads.py --fetch-hashes  # 联网补哈希后生成
python3 tools/gen_model_downloads.py --fetch-hashes --fetch-sizes
python3 tools/gen_model_downloads.py --check         # 与上游比对，不一致退出 1（不联网）
```

- **默认离线**：`sha256` / `bytes` 都**沿用盘上清单里已有的值**（按 URL 对齐），没有就
  `null`——不填 0、不算哈希、不拿别的档推算。跑完会打印"这次没联网，N 条沿用清单里已有的值"。
- `--fetch-sizes` 联网对每个文件发 `HTTP HEAD`，把**最终落点**的 `Content-Length` 写进
  `files[].bytes`；包级 `packages[].bytes` 是文件之和，**任一文件未知就是 `null`**。
- `--fetch-hashes` 联网对每个 `huggingface_snapshot` 文件查 HF tree API（见 §1.3），把
  LFS 真 sha256 写进 `files[].sha256`；可与 `--fetch-sizes` 组合。
- `--check` 与 `--fetch-sizes` / `--fetch-hashes` 互斥（校验不联网）。
- **逐字节稳定**：固定键序 + 固定缩进 + 行尾换行；同一份输入跑两次 `diff` 为空。
  实测：联网跑两次产物逐字节相同；离线就地重生成与联网产物**逐字节相同**（体积/哈希沿用了回来）。

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
- **覆盖校验（2026-09-24 加）**：挑中的包还得覆盖 `session_options` 里声明、且落点**在本模型
  目录内**的权重文件（按文件名比对）；覆盖不了 → `no-source`，`note` 写明缺哪些。依据是
  **应用只下载 `entry`**（`src/model_sources.rs` 不读 `aux_files`，`aux_bytes` 只进占用估算），
  所以"包凑不齐产品要加载的文件"= 用户下完也用不了。真实例子：`yue2` 的 `path` 指目录、
  上游 5 个包每个只含 sidecars + 1 个权重（main q8_0/bf16/q4_0 或 vae f16/f32），而 schema 要
  q4_0 主权重 + vae f16 —— **没有任何单包能凑齐**，于是如实标 `no-source`（跨包组条目的能力
  另开批次）。落在**别的 family** 目录里的声明（如 `qwen3_asr.forced_aligner_model_path`）不算，
  那些在界面上按独立模型看待，由 `aux_bytes` / `aux_unresolved` 如实呈现。

### 1.3 体积与哈希（`bytes` / `sha256`）是怎么取的

**只认最终 2xx 那一跳的头。** HF 的 `/resolve/` 端点先回 302，那一跳的 `content-length`
是**跳转响应体**的长度（实测 1038 B）——照着"HEAD 一下取 Content-Length"写，每个模型的
体积都会变成 1 KB 左右，看起来还挺正常。所以生成脚本**自己跟跳转**（默认的 urllib 跳转
处理器还会把 HEAD 降级成 GET，对着 2 GB 权重跑就等于为了量体积把整份下回来），只取最终
落点那一跳的 `Content-Length`；最终落点没给长度时，退回跳转链上 HF 给的 `x-linked-size`。

取不到（404 / 401 / 没有 `Content-Length` / 超时 / 跳转成环）一律写 `null` 并把原因打到
stderr，**不填 0、不拿别的档累加、不猜**。不可下载的包（gated / 上游不支持）**根本不去探**：
gated 仓库匿名 HEAD 必然 401，白跑一趟还会在日志里制造"取体积失败"的噪音。

**哈希（`sha256`）**：`--fetch-hashes` 对每个 `huggingface_snapshot` 文件查 HF tree API
（`GET https://huggingface.co/api/models/{repo}/tree/{revision}?recursive=1`），按 `path`
对齐取 `lfs.oid`——那是 LFS 真 sha256（64 位十六进制）。非 LFS 文件（存在 git 里的小文件）
只有 git blob 的 sha1（40 位），**不是权重哈希**，不许拿来冒充；取不到（HTTP 错误 /
树里没有这个路径 / 非 LFS / LFS 条目缺 oid）一律写 `null` 并把原因打到 stderr，
不拿别的档推算、不算本地文件。`modelscope_snapshot` 不取哈希（没有 LFS oid 概念），
同样写 `null` 并说明原因。同一 `(repo, revision)` 一次生成只请求一次（进程内缓存，含失败）。
默认离线时 `sha256` 与 `bytes` 一样**沿用盘上清单里已有的值**（按 URL 对齐）。

**为什么必须哈希**（`LESSON_同名同大小的模型权重可能是旧版本须比对上游哈希`）：
上游 HF repo 整体重传时新旧文件**字节数完全相同**、只有 sha256 不同——"大小对得上"完全不能
证明是同一版本，被替换/投毒也无法检出。所以哈希是校验口径，大小只是辅助。

**辅助权重**（`session_options`，如 `qwen3_asr.forced_aligner_model_path`）也会折成
`aux_bytes` + `aux_files`：它们常常是**另一个 family** 的包，按落点在全部 spec 里反查
（`qwen3-asr` 的 forced aligner 就落在 `qwen3_forced_aligner` 这个 family 下）；查不到就把
键名记进 `aux_unresolved`，不做无根据的估算。`aux_files` 单列一份是为了让**离线**重生成也
能把它的体积沿用回来（它不在 `models[].packages` 里，没地方存）。

### 1.4 档位与「本机推荐哪一档」

**档位 = 与入口同一个目录里的可下载单文件包。** 不按 `target_directory`、更不按 family：

- `target_directory` 太粗：`ace-step` 的 turbo / base / xl 是**不同变体**，都挂在
  `ACE-Step1.5-GGUF` 下，混在一起会把"另一个模型"当成"另一档"；
- family 更粗：`qwen3_asr` 同时管 0.6B 与 1.7B，`index_tts2` 同时管 2.0 与 2.5。

多文件包（safetensors 等）不进档位列表：下载器还不支持多文件包（§6），列出来等于给一个
点不了的入口。

**推荐判据是纯函数**（`model_sources::recommend_tier`，输入 = 各档体积 + 本机尺度）：

```text
占用估算(档) = (下载体积 + 辅助权重) × 1.5 + 128 MiB     ← 服务内存守卫自己的公式
需求(档)     = 占用估算(档) + 服务余量(min_free_memory_mb，缺省 1024 MiB)
预算         = 物理内存 × 50%
推荐         = 需求 ≤ 预算 里最大的那一档（体积升序里最后一个满足的）
```

- **为什么抄服务的公式**：只看下载体积会把运行时那部分全漏掉。实测 `qwen3-asr` 0.6B q8_0
  权重 1.07 GiB、服务估的是 **3.31 GiB**（差 3 倍）；`index-tts2` 3.26 GiB → **5.02 GiB**；
  `stable-audio-small-music` 目录树 1.57 GiB → **2.48 GiB**。三个数字都是真机 503 原文，
  回归用例逐位钉住（上游改了系数这里不会自动知道，所以界面上说的始终是"估算"）。
- **X = 50% 的理由**：服务的守卫比的是**当时可用**内存（macOS 上 free+inactive+purgeable），
  而应用只知道**物理**内存总量；可用永远小于物理，而且差得很远——同一台 16 GiB 的机器上，
  并行编译时服务只拿到 1.0–3.9 GiB 可用（物理的 6%–24%）。所以推荐是"这台机器适合哪一档"的
  **规划口径**（一半留给系统、桌面、引擎常驻的其它模型与文件缓存），**天花板不是承诺**：
  真正能不能加载由服务守卫决定，文案里不写"保证装得下"。
- **为什么用物理内存而不是当时可用**：可用每秒钟都在变（同一台机器实测 1.0→3.9 GiB），
  一个会跳的推荐等于没推荐；物理内存是稳定的机器属性。物理内存探测在
  `model_sources::physical_memory_bytes()`（macOS `sysctl -n hw.memsize` / Linux
  `/proc/meminfo` 的 `MemTotal` / Windows PowerShell 的 `TotalPhysicalMemory`），
  解析是纯函数、平台探测走可注入的缝，两种格式在任一平台上都有用例。

四种"没有答案"的情况**都有明确行为**（都不许瞎推荐）：

| 情况 | 行为 |
|---|---|
| 只有一档 | 不给"推荐"（没得选），但说清这一档按本机内存**装得下 / 装不下** |
| 体积未知 | 未知档不参与比较，理由里点明"另有 N 档体积未知，未参与比较"；全未知则不给结论 |
| 内存未知 | 不给推荐，理由说"拿不到本机物理内存，不猜推荐"（**不是**"装不下"） |
| 全都装不下 | 不给推荐档，理由给最小档的需求与预算两个数字 |

**下载按钮下的仍然是"与清单 `path` 对应的那一档"**（`packages[].default` /
`precision_preference` 选出来的入口）。推荐档与入口档不同时，界面上会补一句
"下载按钮取的是 X"——否则就成了"推荐 f16、按钮却在 q8_0"的回显与行为分叉。
要真按推荐档装，得同时把服务清单的 `path` 指过去，**本批不自动做**。

### 1.5 服务清单的字段

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
- **`sha256` 缺失**：回落到内置清单 per-file 的 sha256（就是这条 url 指向文件的上游 LFS
  哈希）；两边都没有 → 退化成按大小校验（见 §3），行的 `detail` 里会如实带「仅校验大小」。
- **`size` 缺失**：按响应的 `Content-Length` 校验。

## 2. 下载到哪 + 落点校验

目标一律是 **`<模型目录>/<相对落点>`**，`<模型目录>` 取全局设置的「模型目录」
（`model_dir()`）：**用户显式设置过就用它**；没设置过时默认跟着 `server.json` 里那些模型
`path` 的**公共父目录**走（本机即 `/Volumes/DataExt/models`——服务实际加载模型的那一层），
推不出来才回落到 `<应用工作目录>/models`。目录不存在会自动创建。**相对落点**：

- 内置清单知道这个模型 → 用它的 `local_paths[0]`（上游 `target_directory + strip_prefix`
  的布局，与 `server.json` 的 `path` 同源）；
- 否则用服务清单 `path` 相对模型目录的那一段；再没有就取 `path` 的文件名、
  `url` 末段（剥 query），最后 `<id>.gguf`。

**默认模型目录怎么推**（`model_root_from_paths`，纯函数）：取每条非空 `path` 的**父目录**
（文件与目录两种 `path` 都取 `parent()` —— gen 类的 `path` 指目录，如 `…/Yue2-3B-GGUF`），
再取这些父目录的最长公共祖先（按**路径组件**比：`/models-2` 不算在 `/models` 里）。
下列情况**不猜**，直接回落到 `<应用工作目录>/models`：

- 清单读不出来 / 没有非空 `path`；
- 有**相对路径**——服务按它自己的 cwd 解析，应用猜不到，让它参与会算出错误的公共根；
- **跨卷 / 跨盘**——硬算出来的是 `/Volumes`、`C:\` 这类"挂载点容器"，不是模型根；
- 公共祖先只剩文件系统根（`/`、`C:\`）。

显式设置过 `model_dir` 时它**永远**优先，上面这套推导不参与（回归用例
`explicit_model_dir_beats_the_manifest_derived_root`）。已知边界：只有**一条** path 时只能
看到一层，取的是它的父目录——若那条 path 指向根下某个模型目录里的文件，推出来的"根"会深一层
（落点校验仍会如实报冲突，不会静默下错地方）。

**落点校验（`⚠ 落点与 server.json 对不上`）**：算出来的落点与 `server.json` 声明的 `path`
按**真实路径**（解析 `..` 与软链）比，不一致时把两边都写在行上，例如：

```
来源：内置下载清单 · /Users/me/models/Qwen3-ASR-0.6B-GGUF/qwen3-asr-0.6b-q8_0.gguf
· ⚠ 落点与 server.json 对不上：清单 path 是 /Volumes/DataExt/models/…，
  本次会下到 /Users/me/models/… —— 下完服务仍可能加载不到（把「模型目录」指到清单所在的位置，
  或把清单 path 改成实际落点）
```

默认目录跟着清单走之后，真机上这 8 条冲突**应当消失**（落点 = 清单 `path` 本身）。
仍然出现冲突时只有两种原因：用户自己覆盖过「模型目录」，或者这套推导回落了（见上）。

`path` 指目录（gen 类模型，如 `Stable-Audio-3-Small-Music-GGUF`）时，文件落在这个目录里
**不算冲突**。没写 `path` 就没有可比的声明，不报。

没有 sha256 的入口（服务侧没写、内置清单也没取到）会在行的 `detail` 里如实带
「仅校验大小」——不许假装有哈希。真实清单重生成后 10 条入口都带哈希，这条基本不可达；
留着是给"服务侧给了 url、两边都没 sha"的兜底形态一个诚实的说法。

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

- 多条任务**并发**：默认同时下 2 个（可配 1–4，见 §4.1），其余是「排队」。
  一条失败 / 取消**不影响其它**。
- 单任务状态机：`排队 → 下载中 → 校验中 → 已完成 | 失败 | 已取消`。
- **同一目标文件只允许一个 writer**：入队时按 `dest` 去重，第二个请求被拒并回报
  已在跑的任务 id（`Enqueued::Duplicate`）。这不是优化而是正确性要求：`.part` + `rename`
  的提交点语义在"两个 writer"下会坏（两次 append 交错、长度还可能刚好等于声明大小，
  只给 `size` 不给 `sha256` 的条目就会被当成好文件 rename 出去）。
  实测存在这种形状——`audio8-tts` 与 `audio8-tts-stream` 是同一份权重的两个 id。
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
- 进度按 `已下载 / 总字节` 上报（总长未知时退化成"已下载 X"），**按 100 ms 节流**
  （`download::PROGRESS_INTERVAL`）：读循环每跳一条，而 `reader.read` 每次拿多少字节由
  socket 缓冲决定 —— 实测本地 4 MiB 就是 512 条。UI 每收到一条都要重建整张下载表，
  不节流时消息泵与重绘被灌满，界面看上去就是"卡死"（2026-09-19 真机反馈），
  而 1 GiB 权重比这还要多两个数量级。
  - **状态变化不吃这个时间窗**：`Downloading → Verifying → Done/Failed/Cancelled`
    是无条件推的（`ProgressGate::state`），否则用户会停在"下载中 100%"。
  - 进度条平滑度换的是 UI 可用性：10 条/秒足够顺，且每条的量级不再随权重体积增长。
- **本机内存只探一次**（`model_sources::physical_memory_bytes` 的 `OnceLock`）：
  Windows 上那次探测要起一个 `powershell`（WMI，冷启动几百毫秒到数秒）并加
  `CREATE_NO_WINDOW`（GUI 子系统下起控制台子进程会闪黑窗）。探测结果（含失败）都缓存，
  物理内存是机器属性，没有"刷新"可言。
- `effective_concurrency()` 是并发数**唯一**的归一化入口（缺省 / 0 / 越界都回落 2，
  夹到 1..=4）：队列起的线程数与界面回显都从它算。并发数改了要**下次启动**生效
  （线程池已经起了），界面回显里写明这一点，不假装立刻生效。
- 队列线程与 UI 之间走既有的 tick 消息泵（`Msg::DownloadUpdate`），
  该消息在 `message_ignores_revision` 白名单里——**下载权重与工程版本无关**，改稿不该把进度静默丢掉。

### 4.1 下载源镜像（全局设置）

设置抽屉里有「模型下载源」，填镜像前缀（如 `https://hf-mirror.com`），留空 = 用清单里的官方地址。

- **只有 HF 官方前缀**（`https://huggingface.co` / `http://huggingface.co`）会被改写成
  `<镜像前缀>/<官方前缀之后的路径与查询>`；**非 HF 链接原样不动**（服务清单里可以写内网 /
  自建仓库 / `s3://`，静默改写它们会指向不存在的地址）。
- 匹配官方前缀时**大小写不敏感且要求后面紧跟 `/` 或结束**——`https://huggingface.co.evil.example/x`
  不会被误判成官方（子串匹配的经典坑）。
- 前缀尾斜杠 / 多斜杠 / 首尾空白都归一；空前缀（含纯空白）= 官方。
- 前缀必须带 `http://` 或 `https://`：不带 scheme 会在**保存时**就被拦下并说明
  （只填 `hf-mirror.com` 会被当成相对路径，失败虽然响亮但不是可执行的线索）；
  `https://` 这种缺主机名的也单独报。
- **不静默回退**：填了镜像就只走镜像，镜像不可达如实报网络错误（用户自己能换回官方）。
  "以为在用镜像、其实偷偷走官方"正是本仓反复抓的那类失败形态。
- 界面「当前生效的源」那一行与真实改写**同源**（都从 `download_mirror::source_note` /
  `rewrite_url` 推导），不是另写一句文案。
- 改写**在入队时**应用，所以入队后改设置不会让已经在排的旧任务偷偷换源——
  用户点下载时看到的是哪条源，下的就是哪条源。

## 5. 验证

- 单测（`src/download.rs`，本地 mock HTTP 用 `std::net::TcpListener`，不引第三方 server）：
  续传（断言收到的 `Range` 头 + 最终字节）、服务器不支持 Range 时从 0 重下、
  416 丢弃坏断点重下、sha256 不匹配丢弃、大小不匹配丢弃、取消（开始前 / 下载中）、
  取消后 416 不再补发请求、服务端卡住时读超时生效、目标目录不存在时创建、
  排队中取消被跳过、`cancel` 如实区分 首次请求取消 / 已在取消中 / 其实已收尾；
  **并发是真的**（进程内屏障 mock：服务端要等**两个**请求都到齐才回，串行实现下第一个请求
  等到超时只能拿到截断响应 -> 该任务失败 -> 用例红；不靠 sleep 计时，机器负载不会把它变成
  flaky 假绿）；**同 dest 的第二个请求被拒且只产生一次请求**；**一个任务失败不影响另一个**；
  **入队时真的应用了镜像**（清单里是官方域名，mock 收到的是改写后的路径）。
  另有 `download_mirror` 模块的纯函数用例：官方改写 / 非 HF 原样 / 相似域名不误判 /
  尾斜杠归一 / 前缀带路径 / 空前缀=官方 / 文案与行为一致 / 无 scheme 或缺主机名的前缀
  被拒且给可执行说明。
- **终态快照与收尾的顺序**（复核驳回项）：`download()` 只推进度快照，终态快照**只有一处**
  产出——`worker_loop` 在 `finish_job`（摘 flag / 放开 dest 名额 / 减 in_flight）**之后**推。
  `terminal_snapshot_means_the_task_is_fully_finished` 在 `notify` **回调内部**用只读探针
  断言这两件事都已发生（在另一个线程里等快照到达再查是恒绿的——写这条时踩过）。
  两条独立变异分别转红：把 `notify` 挪到 `finish_job` 之前 / 删掉 `active_dests.remove`。
  另有 UI 侧的点击语义测试（取消未收尾期间再点不会再排一条）。
- 生成脚本：输出逐字节稳定（同输入跑两次 `diff` 为空）、`--check` 能发现被改过的产物。
  体积解析（`tools/tests/test_gen_model_downloads.py`，stdlib `unittest`，用**真的本地 HTTP
  服务器**）：取到 `Content-Length` / **跟跳转只认最终那一跳**（阳性对照：不跟跳转就会拿到
  跳转响应体的 1038 B）/ 最终没有长度时退回 `x-linked-size` / 没有 `Content-Length` 要 `null`
  不能填 0 / 404 与 401 如实报状态 / 超时要报超时；另有 `package_bytes` 任一文件未知即 `null`、
  `aux_local_path` 的三类落点、`read_existing_sizes` 覆盖 `packages[].files` 与 `aux_files`，
  以及"离线重生成与盘上逐字节一致"的端到端用例。
  哈希解析（离线纯函数，三种夹具）：LFS 条目取 `lfs.oid`（**不是**外层 git blob 的 sha1）/
  非 LFS 条目如实 None + 原因 / LFS 条目缺 oid 如实 None + 原因；tree 按 `path` 对齐、
  找不到路径如实报；`read_existing_hashes` 按 URL 对齐只收非空 sha256。
  跑法：`python3 tools/tests/test_gen_model_downloads.py`。
- 档位与推荐（`src/model_sources.rs`）：估算公式**逐位复现三个真机 503 数字**；多档取预算内
  最大档、不是入口档时点明按钮取哪档；全都装不下 / 体积未知 / 内存未知 / 只有一档各有用例，
  且 `tier` 为 `None`（界面不显示推荐档）；档位只收同目录单文件包（去掉目录过滤会把 ace-step
  的 turbo / base 混成一堆"档位"）；辅助权重计入占用、未核实的辅助权重会带"估算偏低"；物理内存
  三种平台格式 + 探测失败返回 `None`。
- 规划纯函数（`src/model_sources.rs`）四类各有能红的用例：
  **服务清单带 url 时以服务为准**、**gated / 没有源不给入口**、**内置清单读不出来 ≠ 没有源**、
  **映射歧义**（同一个 `audio8_tts` family 下 0.6B 有包、0.1B 没有；`index_tts2` 选 2.5 而非
  2.0；`qwen3_asr` 选 0.6B 而非 1.7B；`stable_audio` 选 small-music 而非 medium）。
  sha256 字段级回落三条各有用例：①服务侧有 sha 用之（内置值不覆盖）；②服务侧为空用内置
  per-file 的兜底；③两边都没有 → `None`（只按大小校验，行为不变）；打进二进制的那份真实
  清单在用例里钉住"10 条入口全部带 64 位十六进制 per-file sha256"，并钉住 sheetsage2 由
  "没有源"转为有入口、yue2 仍因"条目覆盖不了 schema 声明的权重"标 no-source（覆盖校验的正/负对照）。
  另有落点校验（清单 path 在模型目录外 → 报冲突；path 指目录 → 不报）。
  默认模型目录推导（`model_root_from_paths`）另有 5 条用例：多条 / 单条 / 空 / 相对路径 / 跨根，
  外加"显式设置优先"一条；跨卷那条注入假 volume（一台机器上造不出第二个文件系统）。
- UI 冒烟：`AW_UI_STATE=drawer cargo run`（模型区渲染）、`AW_UI_STATE=downloads cargo run`
  （灌下载中/排队/已完成/失败/落点警告/没有下载源六种状态核对进度与按钮）、
  `AW_UI_STATE=model-sources cargo run`（**真机态**：按 `server.json ∪ 内置清单` 实渲，
  状态行直接报"几个有入口、几个标了没有源"）。
- 门禁：`cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`。

## 6. 本批没做（别按已实现宣传）

- **按显存推荐**：现在只看**物理内存**，没查 GPU 显存（服务的守卫也会查后端显存，见上游
  `ensure_model_fits_memory` 的第二段）。多 GPU / 小显存大内存的机器上推荐会偏乐观。
- **不自动替用户下推荐档、也不自动改服务清单**：只"列出哪几档 + 推荐 + 为什么"。
  按推荐档装需要用户自己把清单 `path` 指过去（见 §1.4 末）。
- **只给单文件包下载入口**：多文件包（safetensors 等）不在本批范围，所以它们也不进档位列表。
- **下载完自动改服务清单 / 重启服务**：下载只是把文件放到模型目录（见 §2 的落点校验）。
- 内置清单的包级 `bytes` **不参与校验**（那是界面估算口径）；没给 sha256 的条目按服务侧
  `size` 或响应 `Content-Length` 核大小。
- **多源竞速 / 自动测速选源**：只做"用户指定一个镜像前缀"，不做自动挑源。
- 断点下载没有"定时重试 / 弱网重试"。
- **下载完自动改服务清单 / 重启服务**。
- 下载**没有**走任务中心台账（不占配音/BGM 的 worker 队列），状态只在这块列表里。
- M1「基础下载器（免费）」与 P7「增强下载器」的许可边界未变：**发行版不含任何权重**，一律由用户侧下载。
