# 统一任务队列

> 2026-09-17。对应 thread `cc-ai-audio-workshop-queue-v2`（前身 `cc-ai-audio-workshop-task-queue`）。
> 参考对象：`gqf2008/Xmusic-splitter`（Tauri v2 + Demucs 的人声分离桌面应用，本地只读观察）。

## 1. 为什么需要"排队"而不是"忙时拒绝"

v1 已经有了跨 Tab 的任务台账与任务中心，但提交侧是**硬拒**：

```rust
if ui.get_running() || ui.get_busy() || ui.get_sep_busy() {
    ui.set_status_text("任务进行中：等当前任务结束再生成".into());
    return;
}
```

worker 本来就是单线程 FIFO（`Cmd` 只有一个通道，一条一条消费），所以"拒绝"不是技术限制，
而是把选择权推给了用户：想跑第二条，就得盯着第一条跑完再回来点一次。台账也因此没有
「排队中」这个状态，队列深度恒为 0。

本批把它翻过来：**提交即入队，执行事实决定状态**。

## 2. 状态机

```
提交 ──enqueue──► Pending ──worker 回报 TaskStarted──► Running ──┬──► Done
                    │                                            ├──► Failed
                    │                                            └──► Stopped
                    └──排队中点停止：登记取消 + 直接 Stopped
                        （worker 轮到时 take 到取消标记，不执行）
```

- `Pending`（排队中）与 `Running`（运行中）**都不是终态**，台账裁剪只裁终态那一档；
  运行中的条目不会被"清除已结束"清掉，排队中的同样不会。
- 位次只数**排队项**（1 基）：正在跑的那条不算位次，所以"下一个就是我"永远显示 `#1`。
- 排队中点停止 = 立刻终态：UI 侧把 id 登记进取消表，worker 取到它时不会执行，
  也不消耗算力（不是"等前一条跑完再忽略"）。
- 状态由 worker 的 `TaskStarted` 决定，不由提交动作决定：提交后界面先说"排队中 #N"，
  真正开始时才切成"运行中"。这样任务中心显示的永远是执行事实。

## 3. 哪些任务能排队

| 任务 | 能否排队 | 原因 |
|---|---|---|
| 人声分离 | ✅ | 读自己的输入文件、写 `stems/`，不碰配音工程状态 |
| 音乐制作 | ✅ | 只复用工程目录写 `song/`，不碰配音工程状态 |
| 配音合成 | ❌ | 会改写 worker 持有的 `current` 工程；两个 Run 同时飞会互相覆盖 |
| BGM（含混音） | ❌ | 依赖"配音是否有成品"来决定混音还是独立生成，中途状态会变 |
| 重录 / 拼装 / 导出 | ⚠️ | 自己会置 `busy`，挡得住随后的配音/BGM/分离；但它们不看 `song-busy`，所以**可以排在歌曲后面** |

每个 Tab 同一时刻只保留一条在飞（进度/结果只有一个槽位）：分离还在队列里时再点分离会被
拒绝，歌曲同理。跨 Tab 则允许——正在配音时提交分离，分离就排在它后面。

两类守卫要分清（混起来会误伤）：

| 守卫 | 判据 | 管什么 |
|---|---|---|
| 提交守卫 | 配音 / BGM 用「台账里还有未跑完的任务」（`tasks_in_flight`）；分离 / 歌曲看自己那个 Tab 的槽位 | 谁能现在入队 |
| 编辑守卫 | 配音 / BGM / 试听在飞（`running` / `busy`），**不含**分离与歌曲 | 稿件、工程名、音色、服务设置能不能改 |

编辑守卫**故意不包含**分离与歌曲：这两类任务不碰配音工程，跑着的时候改稿不会互相影响
（分离写自己的 `stems/`，歌曲写自己的 `song/`，两者的终态消息都带 `task_id`、
不靠 `revision` 过滤，所以改稿不会把它们的终态丢掉）。反过来说，配音与 BGM 在飞时
所有编辑都被挡住（它们真的会改写同一份工程状态）。

配音 / BGM 之所以"提交即运行中"（用 `TaskQueue::start`），是因为提交守卫保证入队时
worker 队列是空的；分离 / 歌曲用 `TaskQueue::enqueue`，状态一直等到 worker 的
`TaskStarted` 才变。

## 4. 采纳与不采纳（Xmusic-splitter 参考研究）

采纳：

1. **per-job 取消登记表**。Xmusic 用 `JOB_REGISTRY: Mutex<HashMap<u64, Arc<AtomicBool>>>`
   + `create_job` / `cancel_job` / `take_job` + Drop guard，把取消按 job id 定位，
   并在任务结束时摘除以保证表有界。本项目照抄结论、简化实现为
   `src/cancel.rs` 的 `CancelRegistry`（`cancel` / `take`），由 UI 线程登记、worker 线程取走。
   *这是本批最有价值的一条*：没有它，"排队中点停止"就只能靠 UI 侧标终态，
   worker 轮到时照样会把任务跑完。
2. **位次显示**。Xmusic 在历史列表里给 `pending` 项显示 `#N`；
   本项目的任务中心显示 `排队中 #N`，状态栏 chip 显示 `排队 k（下一个：人声分离）`。
3. **取消是独立状态，不是失败**。Xmusic 的 `cancelled` 与 `failed` 分开；
   本项目沿用 `TaskState::Stopped`（与 `Failed` 分开，状态栏不把主动停止算成"有失败"）。

不采纳：

1. **前端持有队列**。Xmusic 的 FIFO 在 React 侧（`queue` state + `processNextInQueue`），
   后端只提供 `cancel_processing`。本项目的执行者本来就是 Rust 单线程 worker，
   把队列放在 UI 旁边只会让"谁的队列算数"变成两个真相源；这里保持队列在 worker 侧。
2. **status 文件式恢复**。Xmusic 把处理状态写进 task 目录（`get_splitter_status` 读回
   `progress_stage` / `progress_percent`），用于重启后恢复显示。跨进程持久化队列本批不做
   （应用重启后队列清空，与 v1 行为一致），需要时另立。
3. **Demucs 权重与 Tauri/React 架构**。分离后端仍是上游 `stem-splitter-core`（htdemucs），
   理由与实测见 `docs/vocal-separation-backend.md`；Xmusic 的 Demucs 权重与 UI 架构不迁移。

## 5. 已知边界

- 取消是**协作式**的，且三种任务粒度不同（别按同一个想象理解）：
  · 配音：句间检查 `synthesize_stoppable`，已完成的句子保留；
  · BGM：每段开始前检查 `generate_segments_stoppable`，已生成的分段留在目录里；
  · 人声分离：上游**没有**取消 API，`should_stop` 只在整轮 `Separator::separate`
    返回之后被查一次（见 `crates/aw-core/src/separate.rs` 的 `separate_tracks`），
    所以"停止"的真实语义是**跑完整轮再丢弃结果、不落盘**，耗时照算。
  排队中的任务则是硬取消——`take` 到取消标记就不执行。
- **歌曲没有运行中停止**：`generate_song` 是一次阻塞调用，服务端没有取消接口，
  请求发出去就中断不了。所以歌曲 Tab 的主按钮只在**排队中**显示"取消排队"，
  一旦开始跑就显示"生成中…"（不给假停止）。
  人声分离**同样没有**运行中停止（见上一条：整轮返回后才检查一次），但它有确认式的
  "停止"按钮——点下去的效果是"整轮跑完再丢弃结果"，界面文案如实这么写。
  真正能"点一下就停"的只有排队中的任务（硬取消，不执行）。
- 编辑守卫仍是按"是否碰配音工程"分的粗粒度档位（见上表），不按具体字段细分。
- 队列不跨进程：应用退出即清空（与 v1 一致）。
- **产物落点按"提交时"的工程名确定**：分离的 `out_dir` 与歌曲的 `project_name` 在提交
  那一刻就写进命令里，之后改工程名不会改这两条任务的落点（产物会写进旧工程目录）。
  预览/导出用的是绝对路径，不受影响；如果不希望这样，应该在 `sep-busy` / `song-busy`
  时挡住工程名修改——本批选择如实记下而不是拿走改名的能力。
