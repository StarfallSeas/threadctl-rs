//! EbpfSource — kernel-event event source (P7.1).
//!
//! Loads `threadctl-ebpf` .bpf.o via aya, attaches fork/exec/exit tracepoints,
//! consumes the ringbuf in a reader thread → mpsc channel → `poll()`.
//!
//! Event semantics:
//! - FORK  → Zygote pending（cmdline 空窗）→ 读 Tgid 分流：
//!   Tgid==Pid → ProcessEvent::fork；Tgid!=Pid → thread_clone(tgid, tid)
//! - EXEC  → ProcessEvent::exec（引擎处理 tracker 重置）
//! - EXIT  → tgid/pid 由内核 helper 提供（线程退出时 /proc/<tid> 已消失，
//!   无法事后读 tgid）→ 引擎按 tid!=pid 做线程级 applied_tids 清理
//!
//! 降级链：任何加载失败 → `try_new` 返回 Err → main.rs 回退 ProcSource。

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use threadctl_core::config::ConfigSnapshot;
use threadctl_core::event::{EventSource, ProcessEvent};
use threadctl_core::proc::{is_alive, read_cmdline, read_tgid};
use threadctl_core::tracker::StateTracker;

/// eBPF 事件类型（与内核态 threadctl-ebpf 一致）。
const EVENT_FORK: u32 = 1;
const EVENT_EXEC: u32 = 2;
const EVENT_EXIT: u32 = 3;

/// eBPF 事件结构（#[repr(C)] 布局与内核态 threadctl-ebpf::ProcEvent 一致——
/// 内核/用户态结构体必须保持字段顺序与类型同步）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EbpfProcEvent {
    /// FORK: child_pid；EXEC: tgid；EXIT: tgid（进程 pid）
    pub pid: i32,
    /// FORK: child_pid；EXEC: pid；EXIT: 退出任务 pid（线程=线程 tid）
    pub tid: i32,
    /// FORK: child_pid；EXEC/EXIT: 0
    pub child_pid: i32,
    pub comm: [u8; 16],
    pub event_type: u32,
}

/// Zygote fork 空窗 pending（与 ProcSource 同款指数退避）。
struct PendingFork {
    child_pid: i32,
    retry: u8,
    next: Instant,
}

const PENDING_BACKOFF_MS: [u64; 3] = [100, 300, 1000];
const PENDING_MAX_RETRIES: u8 = 3;
const PENDING_MAX_PENDING: usize = 128;

pub struct EbpfSource {
    tracker: Arc<Mutex<StateTracker>>,
    event_rx: mpsc::Receiver<EbpfProcEvent>,
    /// reader 线程句柄（join 防泄漏）。
    _reader: std::thread::JoinHandle<()>,
    /// 持有 bpf 程序存活（drop 时自动 detach/卸载）。
    _bpf: aya::Ebpf,
    cfg: Option<Arc<ConfigSnapshot>>,
    pending: Vec<PendingFork>,
}

impl EbpfSource {
    /// 尝试加载 eBPF；任何失败返回 Err（调用方回退 ProcSource）。
    pub fn try_new(tracker: Arc<Mutex<StateTracker>>) -> Result<Self, String> {
        // 1. 定位 .bpf.o（与 daemon 二进制同目录）
        let ebpf_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("threadctl-ebpf")))
            .filter(|p| p.exists())
            .ok_or("threadctl-ebpf 二进制不存在（需与 daemon 同目录）")?;
        let data = std::fs::read(&ebpf_path).map_err(|e| format!("读取 {} 失败: {e}", ebpf_path.display()))?;

        // 2. 加载（TARGET_COMM_MAP 容量初始 64；on_config_changed 时按包名数重建）
        let mut loader = aya::EbpfLoader::new();
        loader.map_max_entries("TARGET_COMM_MAP", 64);
        let mut bpf = loader.load(&data).map_err(|e| format!("eBPF 加载失败: {e}"))?;

        // 3. attach 三个 tracepoint（任一失败 → 回退）
        for (category, name) in [
            ("sched", "sched_process_fork"),
            ("sched", "sched_process_exec"),
            ("sched", "sched_process_exit"),
        ] {
            let prog = bpf
                .program_mut(name)
                .ok_or_else(|| format!("eBPF 无程序 {name}"))?;
            let tp: &mut aya::programs::TracePoint = prog
                .try_into()
                .map_err(|_| format!("eBPF {name} 类型转换失败"))?;
            tp.load().map_err(|e| format!("eBPF {name} load 失败: {e}"))?;
            tp.attach(category, name)
                .map_err(|e| format!("eBPF {name} attach 失败: {e}"))?;
        }

        // 4. ringbuf → reader 线程 → mpsc
        let ring = bpf
            .take_map("EVENTS")
            .ok_or("eBPF 无 EVENTS map")?;
        let mut ring_buf = aya::maps::RingBuf::try_from(ring)
            .map_err(|e| format!("eBPF EVENTS 转换失败: {e}"))?;

        let (tx, rx) = mpsc::channel::<EbpfProcEvent>();
        let reader = std::thread::spawn(move || {
            loop {
                match ring_buf.next() {
                    Some(item) => {
                        let bytes: &[u8] = &item;
                        if bytes.len() >= std::mem::size_of::<EbpfProcEvent>() {
                            // 未对齐读取：ringbuf 条目起始对齐不保证。
                            let ev: EbpfProcEvent = unsafe {
                                std::ptr::read_unaligned(bytes.as_ptr() as *const EbpfProcEvent)
                            };
                            if tx.send(ev).is_err() {
                                break;
                            }
                        }
                    }
                    None => {
                        // ringbuf 空：休眠保持 50ms 节奏（事件发现延迟 ~50ms 级）。
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        });

        Ok(Self {
            tracker,
            event_rx: rx,
            _reader: reader,
            _bpf: bpf,
            cfg: None,
            pending: Vec::new(),
        })
    }

    /// 白名单键：包名前 8 / 末 8 字节（既有实现 同款——comm 15 字符裁剪，
    /// 滑动窗口匹配需要 8 字节子串；精确匹配在用户态 read_cmdline）。
    fn target_entries(pkgs: &[String]) -> Vec<[u8; 8]> {
        let mut entries: Vec<[u8; 8]> = Vec::new();
        for pkg in pkgs {
            let bytes = pkg.as_bytes();
            if bytes.is_empty() {
                continue;
            }
            let mut prefix = [0u8; 8];
            let plen = bytes.len().min(8);
            prefix[..plen].copy_from_slice(&bytes[..plen]);
            entries.push(prefix);

            if bytes.len() > 8 {
                let mut suffix = [0u8; 8];
                suffix.copy_from_slice(&bytes[bytes.len() - 8..]);
                entries.push(suffix);
            }
        }
        entries.sort();
        entries.dedup();
        entries
    }

    /// 重建白名单（配置变更时：清旧 + 插新）。
    fn update_whitelist(&mut self, cfg: &ConfigSnapshot) {
        let Some(map) = self._bpf.map_mut("TARGET_COMM_MAP") else {
            eprintln!("warning: ebpf TARGET_COMM_MAP 不可访问");
            return;
        };
        let mut target: aya::maps::HashMap<_, [u8; 8], u32> = match map.try_into() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("warning: ebpf TARGET_COMM_MAP 类型转换失败 ({e})");
                return;
            }
        };
        // 清旧
        let old: Vec<[u8; 8]> = target.keys().filter_map(|r| r.ok()).collect();
        for k in &old {
            let _ = target.remove(k);
        }
        // 插新
        for key in Self::target_entries(cfg.rules.pkgs()) {
            let _ = target.insert(key, 1, 0);
        }
        println!("ebpf whitelist: {} entries ({} pkgs)", Self::target_entries(cfg.rules.pkgs()).len(), cfg.rules.pkgs().len());
    }

    /// 非阻塞排空 mpsc → 处理。
    fn drain(&mut self, events: &mut Vec<ProcessEvent>) {
        let mut batch: Vec<EbpfProcEvent> = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            batch.push(ev);
            if batch.len() >= 256 {
                break; // 单轮上限，防 reader 积压无限增长
            }
        }
        for ev in batch {
            self.handle_raw(ev, events);
        }
        self.flush_pending(events);
    }

    fn handle_raw(&mut self, ev: EbpfProcEvent, events: &mut Vec<ProcessEvent>) {
        match ev.event_type {
            EVENT_FORK => {
                // Zygote 空窗：fork 后 cmdline 短暂为空 → pending 退避重读。
                if self.pending.len() < PENDING_MAX_PENDING {
                    self.pending.push(PendingFork {
                        child_pid: ev.pid,
                        retry: 0,
                        next: Instant::now(),
                    });
                }
            }
            EVENT_EXEC => {
                // exec 事件的 pid 是 tgid（进程级）；引擎 Exec 分支处理 tracker 重置。
                events.push(ProcessEvent::exec(ev.pid, ev.pid));
            }
            EVENT_EXIT => {
                // 只关心 tracker 内的进程（EXIT 全量上报，非跟踪的直接忽略）。
                let tracked = self
                    .tracker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(ev.pid);
                if tracked {
                    events.push(ProcessEvent::exit(ev.pid, ev.tid));
                }
            }
            _ => {}
        }
    }

    /// Zygote pending：退避重读 cmdline → 就绪后 Tgid 分流。
    fn flush_pending(&mut self, events: &mut Vec<ProcessEvent>) {
        if self.pending.is_empty() {
            return;
        }
        let Some(cfg) = self.cfg.clone() else {
            return;
        };
        let rules = &cfg.rules;
        let now = Instant::now();
        let mut kept: Vec<PendingFork> = Vec::with_capacity(self.pending.len());
        for p in std::mem::take(&mut self.pending) {
            if now < p.next {
                kept.push(p);
                continue;
            }
            if !is_alive(p.child_pid) {
                continue; // 进程已死，丢弃
            }
            let Some(pkg) = read_cmdline(p.child_pid) else {
                self.retry_or_drop(p, &mut kept);
                continue;
            };
            if pkg.is_empty() {
                self.retry_or_drop(p, &mut kept);
                continue;
            }
            // Tgid 分流（P7 调研修订：fork 参数无 child tgid，用户态读 status）：
            // Tgid == Pid → 进程 fork；Tgid != Pid → 线程 clone（tgid 是进程）。
            let tgid = read_tgid(p.child_pid).unwrap_or(p.child_pid);
            if tgid == p.child_pid {
                if rules.is_interested(&pkg) {
                    events.push(ProcessEvent::fork(p.child_pid, p.child_pid).with_pkg(pkg));
                }
            } else if let Some(ppkg) = read_cmdline(tgid) {
                if rules.is_interested(&ppkg) {
                    events.push(ProcessEvent::thread_clone(tgid, p.child_pid).with_pkg(ppkg));
                }
            }
        }
        self.pending = kept;
    }

    fn retry_or_drop(&self, p: PendingFork, kept: &mut Vec<PendingFork>) {
        if p.retry < PENDING_MAX_RETRIES {
            kept.push(PendingFork {
                child_pid: p.child_pid,
                retry: p.retry + 1,
                next: Instant::now() + Duration::from_millis(PENDING_BACKOFF_MS[p.retry as usize]),
            });
        }
        // 重试耗尽 → 丢弃（ProcSource 全扫兜底）
    }
}

impl EventSource for EbpfSource {
    fn poll(&mut self, deadline: Instant) -> Vec<ProcessEvent> {
        let mut events = Vec::new();
        self.drain(&mut events);
        if events.is_empty() {
            // 空轮：休眠至 deadline（与 ProcSource 节奏一致）。
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
        }
        events
    }

    fn on_config_changed(&mut self, cfg: &Arc<ConfigSnapshot>) {
        self.cfg = Some(cfg.clone());
        self.update_whitelist(cfg);
        self.pending.clear(); // 旧配置的 pending 无意义
    }

    fn shutdown(&mut self) {}
}
