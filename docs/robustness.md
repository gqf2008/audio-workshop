# 落盘与工程读取的可执行错误

> 2026-09-17。对应 thread `cc-ai-audio-workshop-robustness`。
> 本文只记录**用户实际看到的行为**与复现命令，不写设计愿景。

## 1. 为什么专门做这一批

`docs/product-plan.md` 自己标了三处「待补」（v0.3 修订 ②、步骤 4 失败⑦、步骤 7 失败①④）。
对着当前 main 的 Rust 实现核了一遍，其中两条是会真丢东西的：

1. **工程损坏被静默重建**：`restore_project` 是 `let Ok(project) = Project::load(&dir) else { return }`、
   `load_resumable` 是 `Project::load(dir).ok()`——`project.json` 存在但损坏（截断 / 半截 JSON）
   时与「文件不存在」走同一条路：当作全新工程重建，用户已合成的句子全部显示为待合成，
   **而且没有一句解释**；更糟的是随后第一次落盘把损坏的 `project.json` 覆盖掉，
   唯一可人工恢复的现场就没了。
2. **落盘失败不可执行**：`write_atomic` 的错误被塞进 `ClientError::Http(e.to_string())`，
   用户看到的是 `合成中止: No space left on device (os error 28)`——不说哪个路径、
   要释放多少、下一步做什么。

## 2. 现在的行为（逐条可核）

| 场景 | 用户看到什么 | 证据 |
|---|---|---|
| 写句子 wav / srt / `project.json` / BGM manifest / 歌曲 wav / 导出拷贝时空间不足 | `磁盘空间不足（需要 0.6 MB）：请释放空间后重跑（已写好的文件不会被破坏）。路径：<完整路径>` | `aw_core::dub::write_failure_note` + 测试 `write_failure_note_is_actionable_per_error_kind` |
| 拼装 `final.wav`（hound 流式写）空间不足 | 同上，但因为拿不到确切字节数，文案**不提**"需要多少"（不会写"需要 0.0 MB"） | `hound_error_note` + 测试 `zero_byte_write_note_omits_size_and_hound_errors_are_classified` |
| 权限不足 / 路径不存在 | `没有写入权限：检查该目录权限，或把工程/导出目录换到有权限的位置。路径：<路径>` / `路径不存在（父目录可能被删除或移动）：重建目录后再重跑。路径：<路径>` | 同上 |
| 工程状态是「已合成」但句子 wav 丢了（**拼装时会先逐句预校验，所以这条在拼装里也必须命中**） | `句子音频丢失：sentences/007.wav（工程里这句状态是「已合成」）。请重录该句，或把工程目录恢复回来。完整路径：<绝对路径>` | `sentence_read_note` + 测试 `missing_sentence_wav_says_which_file_is_gone`、**走真实 `assemble` 路径**的 `assemble_reports_which_sentence_wav_is_missing` |
| `project.json` 读不了（截断 / 半截 JSON / 权限） | 启动时与开跑时都明确报 `工程文件损坏：没有自动重建，也没有覆盖它——请把 project.json 改名或移走后重开…完整路径：<路径>。解析错误：…`；**开跑会中止，不覆盖现场** | `Project::load_if_present` + 测试 `load_if_present_separates_missing_from_corrupt_project`、main 侧 `corrupt_project_aborts_the_run_and_keeps_the_file` |
| `project.json` 不存在 | 照旧按全新工程从零开始（这一条是回归守卫，不能被上面那条误伤） | 同上两条测试 |

**文案顺序是有意的**：状态栏与任务中心的行都是 `overflow: elide`，长文案会被截尾。
所以每条错误都按「发生了什么 → 该做什么 → 完整路径」排：被截断时丢掉的是路径尾巴，
而不是动作。测试里用 `find(动作) < find(完整路径)` 把这条顺序钉住了
（`write_failure_note_is_actionable_per_error_kind`、`missing_sentence_wav_says_which_file_is_gone`）。

数据安全结论（原子写 `write_atomic` 的形状没变）：失败发生在**临时文件阶段**，
目标文件要么还是旧内容、要么是新内容，不会留下写了一半的成品——
所以"原文件未被破坏"这句写在文案里是有依据的，不是安抚。

### 2.1 覆盖到的写入点（复核要求逐个可查）

句子 wav / `final.srt` / `project.json`（逐句落盘与重录前落盘）/ 拼装 `final.wav`（hound 流式写 +
fsync + rename）/ BGM manifest / BGM 分段 wav（hound 写入 + finalize + fsync + rename）/
BGM 混音 wav / 歌曲 wav / 分离两轨的写出与 rename / 句子复用（复制 + fsync + rename）/
四处导出（配音成品 wav+srt、BGM 分轨、歌曲、分离轨）/ 试听临时 wav。

**导出从 `std::fs::copy` 改成 `copy_atomic`（临时文件 + fsync + rename）**：复核指出
`fs::copy` 失败会把目标截断，而文案里写着"已写好的文件不会被破坏"——两者矛盾。
现在导出也是原子的，要么旧文件不变、要么新文件完整，这句话对所有使用者都成立。

失败路径的测试覆盖：`write_failure_note` 四类 + 零字节 + 顺序断言（helper 级）；
`write_atomic_explained`（真实原子写）；`copy_atomic`（源缺失时不动旧目标、不留 `.tmp`；**rename 失败时同样清临时文件**）；
`assemble` 缺句文件（**真实拼装路径**）；`assemble_bgm` 写不进去（**真实 BGM 写入路径**，
用只读目录触发）。

工程损坏的**读取**侧：`restore_project`（启动恢复）与 `load_resumable`（开跑前）都走
`Project::load_if_present`。

## 3. 本轮**没有**做的（别按已实现宣传）

- **写前预检剩余空间（Rust 桌面侧）**：桌面应用仍未做——理由是"失败后给出准确字节数"已经
  覆盖了可执行性。**注意口径**：产品方案步骤 7 失败①说的是 **CLI 的 `cmd_assemble`**，
  那条已在 `feat/cli-disk-boundary` 实现（`shutil.disk_usage`，标准库，**不需要新依赖**，
  原先"需要新依赖"的判断不成立）；这里留下的未做项仅指 Rust 桌面侧。
- **流式写"写到一半磁盘满"没有端到端模拟**：`assemble_bgm` / `assemble` / 混音的
  `write_sample`/`finalize` 都走同一对已单测的映射函数（`hound_error_note` /
  `write_failure_note`），但测试里触发的是**创建阶段**失败（只读目录），不是写到一半时才失败
  ——macOS 上没有便携的办法把某个文件写到一半就报 ENOSPC。这条按"映射已单测、触发时机未被
  e2e 覆盖"如实记录。
- **逐句标 `error: ENOSPC` 后继续跑**：**语义仍是整轮中止**（继续跑只会每句都失败）。但
  「标 error」这半边已在 `fix/write-fail-residue` 补上：落盘失败时该句标
  `error: ENOSPC（需要 X MB）` 并 best-effort 落盘工程，所以工程文件里**会**留下哪一句、
  为什么停的记录；释放空间后重跑依旧是「跳过已完成句、只重做没做完的」。
- **`settings.json` 的原子写**：损坏时回落默认值，损失可忽略，本批不动。
- **`tools/audio_dub.py`（M0 的 Python CLI）**：本批（Rust 侧）当时没有改它——它那份
  `write_atomic` 原本抛原始异常。**后续已在 `feat/cli-disk-boundary` 补齐**：`cmd_synth`
  捕获 ENOSPC 并把该句标 `error: ENOSPC（需要 X MB）`、`cmd_assemble` 有写前空间检查，
  文案口径与本文件的 Rust 版对齐。

## 3.5 参考音频时长护栏（2026-09-22 · thread cc-ai-audio-workshop-ref-audio-limit）

> 本节记录**引擎进程被参考音频打死**的复现与护栏口径；数字来源：随包 v0.8.2-metalbf16
> 真机实测，证据目录 `/Volumes/DataExt/tmp/aw-stream/repro-193s/`（ref20s.json/wav + engine.log）。

### 复现

- 输入：`~/Documents/音频作坊/voices/20260920陶雨欣录音-b02993a6.wav`（**192.9s**、
  44.1kHz 单声道）作 `voice_ref` 发 audio8-tts 克隆请求；
- 结果：`ggml_metal_buffer_init: error: failed to allocate buffer, size = 14539.00 MiB`
  → SIGSEGV（崩溃栈：`ggml_metal_buffer_is_shared →
  ggml_backend_metal_buffer_type_alloc_buffer → ggml_gallocr_reserve_n_impl →
  audio8_tts codec encode_reference`），**进程死、端口消失**；用户 8080 引擎因此
  崩了两次（2026-09-22 10:36、11:38）；
- 对照：同文件裁到 **20s** → HTTP 200、11.3s 出音频；分配规模约 **75 MiB/参考秒**。

### 上游问题（两条，与本批护栏无关，报给上游的最小修复清单）

1. **参考时长 → 图尺寸的膨胀没有护栏**：192.9s 就要约 14.5GB，应在上游按参考时长
   设上限（或显式报错），而不是由应用侧猜；
2. **分配失败路径本身空指针**：`ggml_metal_buffer_is_shared` 在分配失败后访问空
   buffer → SIGSEGV 打穿进程。应当返回**可捕获的分配错误**，让上层能把
   "参考音太长/显存不足"变成 HTTP 错误，而不是杀进程。

### 本批应用侧护栏（上游修好之前用户不能再踩）

第一批（30s 硬上限）在四个克隆入口拦死超长参考音；本批
（thread cc-ai-audio-workshop-ref-limit-15s）把上限收紧到 15s 并改为**自动取前
15 秒**——原文件不动，应用生成一份本地 wav 副本用于克隆，裁剪失败才退回红色拦截
文案（15s 上限 + 手动处理建议 + 自动裁剪失败原因）。

- `aw_core::ref_audio`：时长读取（wav 读 RIFF 头 / 非 wav symphonia 探测，读不出
  **fail-open**）+ 纯判据 `reference_too_long`（15s 上限，文案含实际秒数 +
  「裁到 15 秒内再合成」+ 为什么）+ `trim_reference_first_seconds`（wav 逐帧拷贝
  保留原规格 / 非 wav symphonia 解码成 s16le；原子写：临时文件 + fsync + rename，
  失败不留半截；副本名 `<slug>-<hash8(规范路径|size|mtime)>-15s.wav`，同一源稳定
  复用、源更新不误用旧副本；源 ≤15s 直接返回源路径）；
- 四个会发克隆请求的入口（开始合成 / 批量提交 / 单句重录 / 音色试听）都用
  `prepare_reference_for_clone` 返回的路径发起——**请求实际使用 ≤15s 的路径**；
  裁剪成功时状态行给信息提示「参考音频 192.9 秒 → 将使用前 15 秒（原文件未改动）」；
- 旧工程**显式迁移**：工程里存着的超长 `voice_ref`（例如 192.9s 真机打崩引擎那条）
  在加载/重录时一次性迁移为 `~/Documents/音频作坊/voice-trimmed/` 下的裁剪副本，
  `voice_ref` 与新 `voice_ref_hash` 一起更新并落盘——否则 `Cmd::Redo` 这类由 worker
  直接用工程 voice_ref 的路径仍会拿超长文件打引擎；
- UI 选中参考音频后显示时长，超 15 秒显示「→ 将使用前 15 秒（原文件未改动）」（不再
  红色警告——自动裁剪不是错误态）；
- 单测：10s/15.0s 放行 / 15.01s 拦 / 读不出 fail-open / wav 20s 裁出 15s 且规格不变
  / wav 10s 返回原路径 / 非 wav flac 真解码裁剪 / 坏文件 Err 且不留 `.tmp` /
  15.0 放行、15.01 裁剪到 15 / 同一源稳定复用、源更新产新副本
  （`aw_core` `ref_audio::tests`）+ main 的 prepare 与旧工程迁移
  （`prepare_trims_overlong_and_passes_short_and_unreadable` /
  `load_resumable_migrates_overlong_voice_ref_and_persists` /
  `redo_migrates_overlong_reference_before_any_request` /
  `redo_blocks_with_red_note_when_trim_fails`，源码守卫
  `every_clone_request_entrance_uses_prepared_reference`）；
- 配套（同批 `fix(engine)`）：托管引擎周期健康检查 + 节流自愈（崩溃后 30s 内自动重拉，
  显式地址不碰）；`server.json` 的 `min_free_memory_mb` 从 0 改为 1024
  （与 schema §1 第 19 行同口径）。

## 4. 复现命令

```console
# 错误分类与文案（不需要真的把磁盘写满）
cargo test -p aw-core --lib dub::tests::write_failure_note_is_actionable_per_error_kind
cargo test -p aw-core --lib dub::tests::zero_byte_write_note_omits_size_and_hound_errors_are_classified
cargo test -p aw-core --lib dub::tests::missing_sentence_wav_says_which_file_is_gone
cargo test -p aw-core --lib dub::tests::assemble_reports_which_sentence_wav_is_missing
cargo test -p aw-core --lib dub::tests::copy_atomic_keeps_old_target_and_leaves_no_temp
cargo test -p aw-core --lib dub::tests::copy_atomic_cleans_temp_when_rename_fails
cargo test -p aw-core --lib bgm::tests::assemble_bgm_write_failure_reports_actionable_note

# 工程损坏：不重建、不覆盖
cargo test -p audio-workshop corrupt_project_aborts_the_run_and_keeps_the_file
cargo test -p aw-core --lib dub::tests::load_if_present_separates_missing_from_corrupt_project

# 手工复现（真的造一个坏工程文件）
PROJ="$HOME/Documents/音频作坊/projects/示例工程 · 频道口播"
printf '{"sentences": [{"index": 1,' > "$PROJ/project.json"
# 重开应用：状态栏应给出「工程文件损坏：<路径>（JSON 解析失败…）」，且文件字节不变
# 复核它没被覆盖：ls -l "$PROJ/project.json"  # 大小应与刚写入的一致
```
