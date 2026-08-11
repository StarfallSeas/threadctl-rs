//! EbpfSource — kernel-event event source (P7.1).
//!
//! Loads `threadctl-ebpf` (bpfel-unknown-none ELF) via aya, attaches fork/exec/exit tracepoints,
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

use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use threadctl_core::config::ConfigSnapshot;
use threadctl_core::event::{EventSource, ProcessEvent};
use threadctl_core::proc::{is_alive, read_cmdline, read_tgid};
use threadctl_core::tracker::StateTracker;
use threadctl_core::debug_log;

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
/// TRACKED_TGID_MAP 与 tracker 的同步周期（秒）——防 cleanup_dead 路径残留。
const TRACKED_SYNC_SECS: u64 = 30;

pub struct EbpfSource {
    tracker: Arc<Mutex<StateTracker>>,
    event_rx: mpsc::Receiver<EbpfProcEvent>,
    /// reader 线程句柄（join 防泄漏）。
    _reader: std::thread::JoinHandle<()>,
    /// 持有 bpf 程序存活（drop 时自动 detach/卸载）。
    _bpf: aya::Ebpf,
    cfg: Option<Arc<ConfigSnapshot>>,
    pending: Vec<PendingFork>,
    /// 启动时初始全扫（eBPF 只捕获启动后的 fork——启动前已运行的白名单
    /// 进程需一次全扫兜底，与 ProcSource 的 scan_all=true 等价）。
    initial_scan_done: bool,
    /// CLAUDE NEW-L1：initial_scan 已覆盖的进程 pid。启动窗口内 drain 到达的
    /// 同 pid FORK 事件跳过（避免重复 refresh），首次窗口结束后 clear。
    initial_scanned: HashSet<i32>,
    /// P7.2：TRACKED_TGID_MAP 上次与 tracker 同步时刻（30s 周期）。
    last_tracked_sync: Instant,
}

impl EbpfSource {
    /// 尝试加载 eBPF；任何失败返回 Err（调用方回退 ProcSource）。
    pub fn try_new(tracker: Arc<Mutex<StateTracker>>, cfg: &ConfigSnapshot) -> Result<Self, String> {
        // 1. 定位 .bpf.o（与 daemon 二进制同目录）
        let ebpf_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("threadctl-ebpf")))
            .filter(|p| p.exists())
            .ok_or("threadctl-ebpf 二进制不存在（需与 daemon 同目录）")?;
        let data = std::fs::read(&ebpf_path).map_err(|e| format!("读取 {} 失败: {e}", ebpf_path.display()))?;

        // 2. 加载。CLAUDE BUG-H1：BPF HashMap 容量加载时固定，不能运行时扩容——
        // 按配置包数计算容量（每包 2 键，next_power_of_two，下限 64），
        // 避免 32+ 包时 insert 静默失败退化为全扫兜底。
        let pkg_count = cfg.rules.pkgs().len();
        let cap = ((pkg_count * 2) as u32).next_power_of_two().max(64);
        let mut loader = aya::EbpfLoader::new();
        loader.map_max_entries("TARGET_COMM_MAP", cap);
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
            initial_scan_done: false,
            initial_scanned: HashSet::new(),
            last_tracked_sync: Instant::now(),
        })
    }

    /// 访问 TRACKED_TGID_MAP（P7.2）。
    fn tracked_map(&mut self) -> Option<aya::maps::HashMap<&mut aya::maps::MapData, i32, u32>> {
        let map = self._bpf.map_mut("TRACKED_TGID_MAP")?;
        map.try_into().ok()
    }

    /// P7.2：插入一个已确认跟踪的 tgid（fork 确认 / initial_scan 后）。
    fn tracked_insert(&mut self, tgid: i32) {
        let Some(mut map) = self.tracked_map() else {
            return;
        };
        let _ = map.insert(tgid, 1, 0);
    }

    /// P7.2：移除一个 tgid（进程退出事件到达时）。
    fn tracked_remove(&mut self, tgid: i32) {
        let Some(mut map) = self.tracked_map() else {
            return;
        };
        let _ = map.remove(&tgid);
    }

    /// P7.2：与 tracker 对齐（30s 周期）——增补漏插、清理残留
    ///（cleanup_dead / tracker.remove 路径无法逐条通知 map）。
    fn tracked_sync(&mut self) {
        let pids: HashSet<i32> = self
            .tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pids()
            .into_iter()
            .collect();
        let Some(mut map) = self.tracked_map() else {
            return;
        };
        // 增补
        for pid in &pids {
            let _ = map.insert(*pid, 1, 0);
        }
        // 清理残留（tracker 已无但 map 仍有）
        let stale: Vec<i32> = map.keys().filter_map(|r| r.ok()).filter(|k| !pids.contains(k)).collect();
        for k in stale {
            let _ = map.remove(&k);
        }
    }

    /// target_entries 的字符串版（debug 输出用）。
    fn target_entries_debug(pkgs: &[String]) -> Vec<String> {
        Self::target_entries(pkgs)
            .iter()
            .map(|k| {
                let end = k.iter().position(|&b| b == 0).unwrap_or(8);
                String::from_utf8_lossy(&k[..end]).into_owned()
            })
            .collect()
    }

    /// 白名单键：包名前 8 / 末 8 字节（8 字节滑动窗口——comm 15 字符裁剪，
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
        // 插新（CLAUDE LOW-1：target_entries 只调一次——排序去重有分配开销）
        let entries = Self::target_entries(cfg.rules.pkgs());
        let n = entries.len();
        for key in entries {
            let _ = target.insert(key, 1, 0);
        }
        println!("ebpf whitelist: {n} entries ({} pkgs)", cfg.rules.pkgs().len());
        debug_log!("ebpf", "whitelist keys: {:?}", Self::target_entries_debug(cfg.rules.pkgs()));
    }

    /// 启动时初始全扫：遍历 /proc 产 Fork 事件（eBPF 只捕获启动后的 fork，
    /// 启动前已运行的白名单进程需兜底——与 ProcSource scan_all=true 等价）。
    fn initial_scan(&mut self, events: &mut Vec<ProcessEvent>) {
        let Some(cfg) = self.cfg.clone() else {
            return;
        };
        let rules = &cfg.rules;
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return;
        };
        let mut scanned = 0usize;
        for entry in dir.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Some(pkg) = read_cmdline(pid) else {
                continue;
            };
            if pkg.is_empty() || !rules.is_interested(&pkg) {
                continue;
            }
            debug_log!("ebpf", "initial scan: pid={} pkg={}", pid, pkg);
            events.push(ProcessEvent::fork(pid, pid).with_pkg(pkg));
            self.initial_scanned.insert(pid);
            scanned += 1;
        }
        if scanned > 0 {
            println!("ebpf initial scan: {scanned} whitelisted processes");
        }
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
        debug_log!("ebpf", "raw event type={} pid={} tid={} comm={:?}",
            ev.event_type, ev.pid, ev.tid,
            core::str::from_utf8(&ev.comm).unwrap_or("<bad>"));
        match ev.event_type {
            EVENT_FORK => {
                // CLAUDE NEW-L1：启动窗口内 initial_scan 已覆盖的进程 → 跳过
                //（其 Fork 事件与 initial_scan 产出的重复，引擎会多跑一次
                // refresh_process_rules——正确但冗余 syscall）。
                if self.initial_scanned.contains(&ev.pid) {
                    debug_log!("ebpf", "fork pid={} skipped (initial scan covered)", ev.pid);
                    return;
                }
                // Zygote 空窗：fork 后 cmdline 短暂为空 → pending 退避重读。
                // CLAUDE BUG-M1：内核防抖允许同 pid 0.1s 内 2 事件 → 去重，
                // 否则同一 child_pid 进 pending 两次产生重复 Fork。
                if self.pending.len() < PENDING_MAX_PENDING
                    && !self.pending.iter().any(|p| p.child_pid == ev.pid)
                {
                    self.pending.push(PendingFork {
                        child_pid: ev.pid,
                        retry: 0,
                        next: Instant::now(),
                    });
                    debug_log!("ebpf", "fork pid={} queued (pending={})", ev.pid, self.pending.len());
                } else {
                    debug_log!("ebpf", "fork pid={} dropped (dedup/limit)", ev.pid);
                }
            }
            EVENT_EXEC => {
                // exec 事件的 pid 是 tgid（进程级）；引擎 Exec 分支处理 tracker 重置。
                events.push(ProcessEvent::exec(ev.pid, ev.pid));
            }
            EVENT_EXIT => {
                // 只关心 tracker 内的进程（内核已按 TRACKED_TGID_MAP 过滤，
                // 到达的基本是跟踪集；此检查兜底配置变更竞态）。
                let tracked = self
                    .tracker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(ev.pid);
                if tracked {
                    events.push(ProcessEvent::exit(ev.pid, ev.tid));
                }
                // P7.2：进程退出（tid==pid）→ 从内核过滤表移除；
                // 线程退出（tid!=pid）保留——进程还在跟踪集。
                if ev.tid == ev.pid {
                    self.tracked_remove(ev.pid);
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
            debug_log!("ebpf", "pending pid={} pkg={:?} tgid={} (process={})",
                p.child_pid, pkg, tgid, tgid == p.child_pid);
            if tgid == p.child_pid {
                if rules.is_interested(&pkg) {
                    debug_log!("ebpf", "FORK pid={} pkg={} interested", p.child_pid, pkg);
                    events.push(ProcessEvent::fork(p.child_pid, p.child_pid).with_pkg(pkg));
                    self.tracked_insert(tgid); // P7.2：确认跟踪 → 内核 EXIT 过滤表
                }
            } else if let Some(ppkg) = read_cmdline(tgid) {
                if rules.is_interested(&ppkg) {
                    debug_log!("ebpf", "ThreadClone pid={} tid={} pkg={} interested", tgid, p.child_pid, ppkg);
                    events.push(ProcessEvent::thread_clone(tgid, p.child_pid).with_pkg(ppkg));
                    self.tracked_insert(tgid);
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
        // 启动时初始全扫一次（eBPF 捕获启动后的 fork；启动前已运行进程兜底）
        if !self.initial_scan_done {
            self.initial_scan(&mut events);
            self.initial_scan_done = true;
        }
        self.drain(&mut events);
        // 启动窗口结束：initial_scanned 仅用于 initial_scan 同轮 drain
        self.initial_scanned.clear();
        // P7.2：TRACKED_TGID_MAP 定期与 tracker 对齐（30s）
        if self.last_tracked_sync.elapsed().as_secs() >= TRACKED_SYNC_SECS {
            self.tracked_sync();
            self.last_tracked_sync = Instant::now();
        }
        if events.is_empty() {
            // 空轮：休眠至 min(deadline, pending 最早退避时刻)。
            // CLAUDE BUG-M2：否则 100ms 的 pending 退避会被 2s 轮询周期
            // 膨胀成最差 2.1s——Zygote 场景与 ProcSource 无差别。
            let wake = self
                .pending
                .iter()
                .map(|p| p.next)
                .min()
                .map_or(deadline, |min_next| deadline.min(min_next));
            let now = Instant::now();
            if now < wake {
                std::thread::sleep(wake - now);
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


#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> [u8; 8] {
        let mut k = [0u8; 8];
        let b = s.as_bytes();
        let n = b.len().min(8);
        k[..n].copy_from_slice(&b[..n]);
        k
    }

    #[test]
    fn target_entries_short_pkg() {
        // CLAUDE LOW-4：<=8 字节包名 -> 仅前缀键
        let entries = EbpfSource::target_entries(&["sleep".into()]);
        assert_eq!(entries, vec![key("sleep")]);
    }

    #[test]
    fn target_entries_long_pkg_prefix_and_suffix() {
        // 长包名 -> 前 8 + 末 8 两个键
        let entries = EbpfSource::target_entries(&["com.tencent.mm".into()]);
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&key("com.tenc")));
        assert!(entries.contains(&key("ncent.mm"))); // 末 8 字节
    }

    #[test]
    fn target_entries_exact_eight_bytes_single_key() {
        // CLAUDE NEW-L3：恰好 8 字节包名 → 只产生前缀键（len > 8 才加后缀）
        let entries = EbpfSource::target_entries(&["com.test".into()]);
        assert_eq!(entries.len(), 1, "8 字节包名只应有前缀键");
        assert_eq!(entries[0], key("com.test"));
    }

    #[test]
    fn target_entries_dedup_and_empty() {
        // 去重 + 空包名跳过
        let entries = EbpfSource::target_entries(&["com.a".into(), "com.a".into(), String::new()]);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn target_entries_multiple_pkgs() {
        // 多包名：键排序去重
        let entries = EbpfSource::target_entries(&["bb".into(), "aa".into()]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], key("aa"), "排序后 aa 在前");
    }
}
