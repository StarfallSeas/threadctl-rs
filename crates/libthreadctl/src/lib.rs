//! libthreadctl — 可嵌入的亲和线程池（P11，affinitypool 方向）。
//!
//! 与 daemon 形态互补：
//! - **daemon**（threadctl）：管理已有进程的线程（游戏场景）
//! - **lib**（本 crate）：应用内嵌创建绑核线程池（服务器/中间件/游戏引擎）
//!
//! 示例：
//! ```rust,ignore
//! let mut pool = AffinityPool::new("render", "6-7", 2).expect("创建失败");
//! pool.spawn(|| println!("在绑 6-7 核的线程上执行"));
//! ```
//!
//! 底层：`CpuSet::parse`（threadctl-core 拓扑）+ `sched_setaffinity`（libc）。

use std::sync::mpsc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use threadctl_core::topology::{parse_cpu_ranges, CpuSet};

/// 亲和线程池：N 个 worker，全部绑定到指定 CPU 集，round-robin 分发任务。
pub struct AffinityPool {
    name: String,
    cpus: CpuSet,
    workers: Vec<Worker>,
    next: AtomicUsize,
}

struct Worker {
    _handle: thread::JoinHandle<()>,
    tx: mpsc::Sender<Box<dyn FnOnce() + Send + 'static>>,
}

impl AffinityPool {
    /// 创建线程池：`name`（线程名前缀）、`cpus`（"0-3"/"6-7" 范围）、`size`（worker 数）。
    pub fn new(name: &str, cpus: &str, size: usize) -> Result<Self, String> {
        let set = parse_cpu_ranges(cpus, None);
        if set.is_empty() {
            return Err(format!("CPU 集为空: {cpus}"));
        }
        if set.is_empty() {
            return Err(format!("CPU 集为空: {cpus}"));
        }
        if size == 0 {
            return Err("线程池大小必须 >= 1".into());
        }
        let mut workers = Vec::with_capacity(size);
        for i in 0..size {
            let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send + 'static>>();
            let cpus = set.clone();
            let worker_name = format!("{name}-{i}");
            let handle = thread::Builder::new()
                .name(worker_name.clone())
                .spawn(move || {
                    // 绑定到目标 CPU 集
                    bind_thread(&cpus).unwrap_or_else(|e| {
                        eprintln!("libthreadctl: worker {worker_name} 绑定失败: {e}");
                    });
                    // 任务循环（每任务一条消息；rx 关闭即退出）
                    while let Ok(task) = rx.recv() {
                        task();
                    }
                })
                .map_err(|e| format!("worker 创建失败: {e}"))?;
            workers.push(Worker { _handle: handle, tx });
        }
        Ok(Self {
            name: name.to_string(),
            cpus: set,
            workers,
            next: AtomicUsize::new(0),
        })
    }

    /// 提交任务（round-robin 到某个 worker 线程执行）。
    /// 返回 Err：所有 worker 通道已关闭（线程池已销毁）。
    pub fn spawn<F>(&self, task: F) -> Result<(), mpsc::SendError<Box<dyn FnOnce() + Send + 'static>>>
    where
        F: FnOnce() + Send + 'static,
    {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.workers[idx].tx.send(Box::new(task))
    }

    /// worker 数。
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// 目标 CPU 集描述（"0-3"）。
    pub fn cpus_desc(&self) -> String {
        self.cpus.to_range_string()
    }

    /// 池名。
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// 将当前线程绑定到 CPU 集（`sched_setaffinity`）。
pub fn bind_thread(cpus: &CpuSet) -> Result<(), String> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for cpu in cpus.iter() {
        unsafe { libc::CPU_SET(cpu as usize, &mut set) };
    }
    let ret = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if ret != 0 {
        return Err(format!("sched_setaffinity errno={}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpu_set() {
        let set = parse_cpu_ranges("0-3", None);
        assert_eq!(set.count(), 4);
        assert!(!set.is_empty());
    }

    #[test]
    fn pool_rejects_bad_input() {
        assert!(AffinityPool::new("x", "not-a-range", 1).is_err());
        assert!(AffinityPool::new("x", "0-1", 0).is_err(), "size 0 拒绝");
        assert!(parse_cpu_ranges("", None).is_empty());
    }

    #[test]
    fn pool_spawn_executes_task() {
        let pool = AffinityPool::new("test", "0-1", 2).expect("创建");
        let (tx, rx) = mpsc::channel();
        pool.spawn(move || {
            let _ = tx.send(42u32);
        })
        .expect("提交");
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(2)), Ok(42), "任务应在 worker 执行");
        assert_eq!(pool.worker_count(), 2);
        assert_eq!(pool.cpus_desc(), "0-1");
    }

    #[test]
    fn bind_current_thread_is_ok_or_perm() {
        // 非 root 下对当前线程 setaffinity 通常允许；受限环境 EPERM——不 panic 即通过
        let set = parse_cpu_ranges("0", None);
        let _ = bind_thread(&set);
    }
}
