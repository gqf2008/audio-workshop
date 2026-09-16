//! 跨 Tab 任务台账：配音 / BGM / 音乐制作（以及将来的人声分离）共用一份登记。
//!
//! 为什么需要它：worker 本来就是一个顺序队列，但**任务状态只散落在各 Tab 的
//! status-text / 进度条里**——切到别的 Tab 就看不见当前在跑什么，也不知道有几个
//! 失败过。这里把"有哪些任务、什么状态、属于哪个 Tab、进度多少"集中起来，
//! 供状态栏计数与任务中心使用。
//!
//! 边界：本模块只管**登记**，不负责调度（worker 仍是唯一的执行者，任务串行）。
//! 因此这里没有"排队中"状态——UI 在任务进行中会拒绝开启新任务。

/// 任务种类。tab 决定任务中心里「跳转」会切到哪个主 Tab。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum TaskKind {
    Dub,
    Bgm,
    Song,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Dub => "配音",
            TaskKind::Bgm => "BGM",
            TaskKind::Song => "音乐制作",
        }
    }

    /// 与 `ui/app.slint` 的 scene 下标一致（0 配音 / 1 BGM / 2 人声分离 / 3 音乐制作 / 4 音色设计）
    pub fn tab(self) -> i32 {
        match self {
            TaskKind::Dub => 0,
            TaskKind::Bgm => 1,
            TaskKind::Song => 3,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Running,
    Done,
    Failed,
    Stopped,
}

impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Running => "运行中",
            TaskState::Done => "已完成",
            TaskState::Failed => "失败",
            TaskState::Stopped => "已停止",
        }
    }

    /// 终态：不再变化，可以「清除已完成」清掉
    pub fn is_final(self) -> bool {
        !matches!(self, TaskState::Running)
    }
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: u32,
    pub kind: TaskKind,
    pub title: String,
    pub state: TaskState,
    pub detail: String,
    pub progress: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Counts {
    pub running: usize,
    pub failed: usize,
    /// 终态且非失败：**完成 + 已停止**都算这里。界面文案必须写「结束」而不是「完成」，
    /// 否则用户主动停止的任务会被显示成"完成"。
    pub finished: usize,
}

/// 台账保留的**终态**条目上限（防长时间运行后无限增长）。
///
/// 不变量：**运行中的条目永不裁剪**——所以总长度是"运行中条数 + ≤MAX_TASKS"。
/// 今天 UI 在运行中会拒绝开启新任务（worker 也是串行），运行中最多 1 条，
/// 因此总长度实际有界；这里不假设这一点，只保证不会为了砍长度丢掉在跑的任务。
const MAX_TASKS: usize = 50;

#[derive(Default)]
pub struct TaskQueue {
    next_id: u32,
    tasks: Vec<Task>,
}

impl TaskQueue {
    /// 登记一个新任务（调用即视为开始运行），返回任务 id。
    pub fn start(&mut self, kind: TaskKind, title: impl Into<String>) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.tasks.push(Task {
            id,
            kind,
            title: title.into(),
            state: TaskState::Running,
            detail: String::new(),
            progress: 0.0,
        });
        self.trim_finished();
        id
    }

    /// 把终态条目裁到上限之内（运行中的一条都不动）。
    fn trim_finished(&mut self) {
        while self.tasks.iter().filter(|t| t.state.is_final()).count() > MAX_TASKS {
            let Some(pos) = self.tasks.iter().position(|t| t.state.is_final()) else {
                break;
            };
            self.tasks.remove(pos);
        }
    }

    /// 更新进度与说明（只对运行中的任务生效：晚到的进度不该把已完成的任务改回运行中）。
    pub fn progress(&mut self, id: u32, progress: f32, detail: impl Into<String>) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            if t.state == TaskState::Running {
                t.progress = progress.clamp(0.0, 1.0);
                let detail = detail.into();
                if !detail.is_empty() {
                    t.detail = detail;
                }
            }
        }
    }

    /// 收尾。已处于终态的任务不会被二次改写（保证"失败后显示失败"，不被收尾清扫覆盖）。
    pub fn finish(&mut self, id: u32, state: TaskState, detail: impl Into<String>) {
        debug_assert!(state.is_final(), "收尾必须是终态");
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            if t.state == TaskState::Running {
                t.state = state;
                t.progress = if state == TaskState::Done {
                    1.0
                } else {
                    t.progress
                };
                t.detail = detail.into();
            }
        }
        self.trim_finished();
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for t in &self.tasks {
            match t.state {
                TaskState::Running => c.running += 1,
                TaskState::Failed => c.failed += 1,
                TaskState::Done | TaskState::Stopped => c.finished += 1,
            }
        }
        c
    }

    /// 最近一个仍在跑的任务（状态栏要显示"当前在跑什么"）。
    pub fn running(&self) -> Option<&Task> {
        self.tasks
            .iter()
            .rev()
            .find(|t| t.state == TaskState::Running)
    }

    /// 最近的失败任务（状态栏要提示"有失败"）。
    pub fn last_failed(&self) -> Option<&Task> {
        self.tasks
            .iter()
            .rev()
            .find(|t| t.state == TaskState::Failed)
    }

    /// 任务中心的列表：新的在前。
    pub fn tasks_newest_first(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().rev()
    }

    /// 清掉终态条目（运行中的保留）。返回清掉的条数。
    pub fn clear_finished(&mut self) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(|t| !t.state.is_final());
        before - self.tasks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_then_finish_moves_task_through_counts() {
        let mut q = TaskQueue::default();
        let id = q.start(TaskKind::Dub, "开始配音 34 句");
        assert_eq!(
            q.counts(),
            Counts {
                running: 1,
                failed: 0,
                finished: 0
            }
        );

        q.progress(id, 0.4, "第 12/34 句");
        assert_eq!(q.running().unwrap().progress, 0.4);
        assert_eq!(q.running().unwrap().detail, "第 12/34 句");

        q.finish(id, TaskState::Done, "34/34 完成");
        let t = &q.tasks_newest_first().next().unwrap();
        assert_eq!(t.state, TaskState::Done);
        assert_eq!(t.progress, 1.0, "完成时进度补到 1");
        assert_eq!(
            q.counts(),
            Counts {
                running: 0,
                failed: 0,
                finished: 1
            }
        );
    }

    #[test]
    fn progress_and_finish_do_not_resurrect_finished_tasks() {
        let mut q = TaskQueue::default();
        let id = q.start(TaskKind::Bgm, "生成 BGM");
        q.finish(id, TaskState::Failed, "磁盘满");
        // 晚到的进度与收尾都不该覆盖失败态（真实场景：消息泵里排队的老消息）
        q.progress(id, 0.9, "生成中");
        q.finish(id, TaskState::Done, "完成");
        let t = q.tasks_newest_first().next().unwrap();
        assert_eq!(t.state, TaskState::Failed);
        assert_eq!(t.detail, "磁盘满");
        assert_eq!(q.counts().failed, 1);
        assert_eq!(q.counts().finished, 0);
    }

    /// 停止属于"结束"而不是"失败"、也不是"完成"这类成功语义——这档分类此前没人守，
    /// 改坏成 failed 时全绿（审查指出）。
    #[test]
    fn stopped_counts_as_finished_not_failed() {
        let mut q = TaskQueue::default();
        let id = q.start(TaskKind::Dub, "被停掉的配音");
        q.finish(id, TaskState::Stopped, "用户停止");
        let c = q.counts();
        assert_eq!(c.running, 0);
        assert_eq!(c.failed, 0, "主动停止不是失败");
        assert_eq!(c.finished, 1, "停止与完成同属「结束」档");
        assert!(
            q.last_failed().is_none(),
            "停止的任务不该被当成'最近失败'提示"
        );
    }

    #[test]
    fn clear_finished_keeps_running_task() {
        let mut q = TaskQueue::default();
        let a = q.start(TaskKind::Dub, "a");
        let b = q.start(TaskKind::Song, "b");
        q.finish(a, TaskState::Stopped, "用户停止");
        assert_eq!(q.clear_finished(), 1);
        assert_eq!(q.counts().running, 1, "运行中的 b 不能被清掉");
        assert_eq!(q.running().unwrap().id, b);
    }

    #[test]
    fn task_kind_tabs_match_the_five_tab_shell() {
        // 与 ui/app.slint 的 scenes 顺序对齐：0 配音 / 1 BGM / 2 人声分离 / 3 音乐制作 / 4 音色设计
        assert_eq!(TaskKind::Dub.tab(), 0);
        assert_eq!(TaskKind::Bgm.tab(), 1);
        assert_eq!(TaskKind::Song.tab(), 3);
    }

    #[test]
    fn history_trims_finished_but_never_drops_a_running_task() {
        let mut q = TaskQueue::default();
        let running = q.start(TaskKind::Dub, "正在跑的任务");

        // 制造大量终态条目：终态应有上限
        for i in 0..(MAX_TASKS + 5) {
            let id = q.start(TaskKind::Bgm, format!("t{i}"));
            q.finish(id, TaskState::Done, "ok");
        }
        assert!(
            q.tasks.len() <= MAX_TASKS + 1,
            "终态条目应被裁到上限附近（运行中的额外保留），实得 {}",
            q.tasks.len()
        );
        assert!(
            q.tasks_newest_first()
                .any(|t| t.id == running && t.state == TaskState::Running),
            "运行中的任务绝不能被裁掉"
        );

        // 极端情形（全部运行中、无终态可裁）也不 panic、不丢任务
        let mut all_running = TaskQueue::default();
        let ids: Vec<u32> = (0..(MAX_TASKS + 5))
            .map(|i| all_running.start(TaskKind::Dub, format!("r{i}")))
            .collect();
        assert_eq!(all_running.counts().running, ids.len());
    }
}
