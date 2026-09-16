//! 排队任务的取消登记表：**还没轮到跑就被停掉**的任务，worker 取到它时必须不执行。
//!
//! 为什么单独一张表：UI 侧的"停止"按钮点的是当前正在跑的任务，但队列里的任务
//! 也需要能停——用户排了三条分离，看到第二条不想要了，不该等前一条跑完才生效。
//! 台账（`tasks.rs`）只记状态，不跨越 UI 线程与 worker 线程；这里用一个可共享的
//! 登记表把"这条已经取消了"这个事实交给执行方。
//!
//! 做法采纳自 gqf2008/Xmusic-splitter 的 per-job registry
//! （`create_job` / `cancel_job` / `take_job` + Drop guard）：取消按任务 id 定位，
//! 只作用于单条任务；执行方取走时顺手摘除（`take`），保证表不随运行时长增长。
//!
//! 它是**协作式**取消，不是抢占：正在跑的任务由各自的停止位（`stop` /
//! `sep_stop`）在能停的点上响应；本表只解决"还没开始就不该开始"。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// 线程共享的取消登记表（UI 线程与 worker 线程各持一个 clone）。
#[derive(Clone, Default)]
pub struct CancelRegistry {
    ids: Arc<Mutex<HashSet<u32>>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记"任务 id 已被取消"（幂等；worker 已经跑完再登记也不会复活任务，
    /// 因为调用方在终态时不会再登记）。
    pub fn cancel(&self, id: u32) {
        if let Ok(mut g) = self.ids.lock() {
            g.insert(id);
        }
    }

    /// 取走登记：返回"此前是否已取消"。worker 在**开始执行前**调用一次，
    /// 无论结果如何登记都被摘除，表因此有界。
    pub fn take(&self, id: u32) -> bool {
        self.ids.lock().map(|mut g| g.remove(&id)).unwrap_or(false)
    }

    /// 当前登记条数（测试用：验证取消项会被摘除、表有界）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ids.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_then_take_reports_and_clears() {
        let reg = CancelRegistry::new();
        assert!(!reg.take(7), "没登记过的任务取走时不回报已取消");

        reg.cancel(7);
        assert_eq!(reg.len(), 1);
        assert!(reg.take(7), "取走时应回报此前已取消");
        assert_eq!(reg.len(), 0, "表必须回到空：否则长跑会无限增长");

        // 再取一次：已经摘除，不该回报"已取消"
        assert!(!reg.take(7));
    }

    #[test]
    fn cancelling_one_task_does_not_affect_others() {
        let reg = CancelRegistry::new();
        reg.cancel(1);
        assert_eq!(reg.len(), 1);
        assert!(!reg.take(2), "取消只作用于单条任务");
        assert!(reg.take(1), "被取消的那条仍在登记里");
    }

    #[test]
    fn registry_is_shared_across_clones() {
        // UI 线程与 worker 线程各持一个 clone：取消必须能跨线程看见
        let ui_side = CancelRegistry::new();
        let worker_side = ui_side.clone();
        ui_side.cancel(42);
        assert!(worker_side.take(42), "clone 之间共享同一张表");
    }
}
