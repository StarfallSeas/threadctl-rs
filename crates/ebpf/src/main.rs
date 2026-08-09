#![no_std]
#![no_main]
#![allow(linker_messages)]

//! threadctl-ebpf — 内核态事件源（P7.1）
//!
//! tracepoint：sched_process_fork / sched_process_exec / sched_process_exit
//!   - FORK/EXEC：白名单粗过滤（TARGET_COMM_MAP 8 字节滑动窗口，参考 既有实现）
//!     + 防抖（DEDUP_MAP，0.1s 窗口每 pid 最多 2 事件）
//!   - EXIT：全量上报（线程退出 comm 不匹配包名键——过滤会漏线程退出，
//!     IMPL-4 需要所有退出事件供用户态清理 applied_tids）
//!
//! 事件格式对齐 SM8550 kernel 5.15 tracepoint format（已实测确认）：
//!   fork:  parent_comm[16]@8, parent_pid@24, child_comm[16]@28, child_pid@44
//!   exit:  comm[16]@8, pid@24, prio@28
//!   exec:  bpf_get_current_pid_tgid/comm（当前任务即 exec 者）
//!
//! 线程 clone 分流（tgid==pid → Fork；tgid!=pid → ThreadClone）在**用户态**
//! pending 后读 /proc/<pid>/status Tgid 完成（P7 调研修订——fork 参数无 child tgid，
//! 内核态读 task_struct 复杂且 /proc 未就绪，用户态与 Zygote pending 天然合并）。

use aya_ebpf::{
    helpers::{bpf_get_current_comm, bpf_get_current_pid_tgid, bpf_ktime_get_ns},
    macros::{map, tracepoint},
    maps::{HashMap, LruHashMap, RingBuf},
    programs::TracePointContext,
};

const EVENT_FORK: u32 = 1;
const EVENT_EXEC: u32 = 2;
const EVENT_EXIT: u32 = 3;

/// 进程/线程事件（#[repr(C)] 布局与用户态 threadctl_core::ebpf::EbpfProcEvent 一致）
#[repr(C)]
pub struct ProcEvent {
    /// FORK: child_pid；EXEC: tgid；EXIT: 退出任务 pid（线程退出=pid==tid）
    pub pid: i32,
    /// FORK: child_pid；EXEC: pid；EXIT: 同 pid
    pub tid: i32,
    /// FORK: 子进程 PID；EXEC/EXIT: 0
    pub child_pid: i32,
    pub comm: [u8; 16],
    pub event_type: u32,
}

#[repr(C)]
struct DedupEntry {
    last_ns: u64,
    count: u32,
}

/// 白名单键容量（用户态 EbpfLoader::set_max_entries 按包名数动态覆盖）。
const MAP_CAPACITY: u32 = 512;
/// 防抖表容量（LruHashMap，高频进程 evict）。
const DEDUP_CAPACITY: u32 = 4096;
/// 防抖窗口：0.1 秒（纳秒）。
const DEDUP_WINDOW_NS: u64 = 100_000_000;
/// 窗口内最大事件数，超过则丢弃。
const DEDUP_MAX_COUNT: u32 = 2;

/// FORK/EXEC 白名单：键为 8 字节匹配串（包名前 8 / 末 8 字节，comm 滑动窗口命中）。
/// EXIT 不走白名单（全量上报，见模块注释）。
#[map]
static TARGET_COMM_MAP: HashMap<[u8; 8], u32> = HashMap::with_max_entries(MAP_CAPACITY, 0);

/// FORK/EXEC 防抖表（高频 fork 风暴抑制，参考 既有实现）。
#[map]
static DEDUP_MAP: LruHashMap<u32, DedupEntry> = LruHashMap::with_max_entries(DEDUP_CAPACITY, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// 防抖：窗口内每 pid 最多 DEDUP_MAX_COUNT 事件。
#[inline(always)]
fn should_dedup(pid: u32, now_ns: u64) -> bool {
    let prev = unsafe { DEDUP_MAP.get(&pid) };
    let (last_ns, count) = match prev {
        Some(e) => (e.last_ns, e.count),
        None => (0, 0),
    };

    if now_ns - last_ns < DEDUP_WINDOW_NS {
        let new_count = count + 1;
        if new_count > DEDUP_MAX_COUNT {
            return true;
        }
        let _ = DEDUP_MAP.insert(
            &pid,
            &DedupEntry {
                last_ns,
                count: new_count,
            },
            0,
        );
    } else {
        let _ = DEDUP_MAP.insert(
            &pid,
            &DedupEntry {
                last_ns: now_ns,
                count: 1,
            },
            0,
        );
    }
    false
}

/// 白名单粗过滤：comm 任意 8 字节子串命中即通过（既有实现 同模式）。
/// comm 是 15 字符裁剪近似（如抖音主进程 "droid.ugc.aweme"），
/// 精确匹配永远在用户态 read_cmdline——粗过滤只为减少无关事件。
#[inline(always)]
fn comm_matches(comm: &[u8; 16]) -> bool {
    let mut pos: usize = 0;
    while pos <= 8 {
        let key: [u8; 8] = [
            comm[pos],
            comm[pos + 1],
            comm[pos + 2],
            comm[pos + 3],
            comm[pos + 4],
            comm[pos + 5],
            comm[pos + 6],
            comm[pos + 7],
        ];
        if unsafe { TARGET_COMM_MAP.get(&key) }.is_some() {
            return true;
        }
        pos += 1;
    }
    false
}

/// 统一上报：过滤（FORK/EXEC）→ 防抖（FORK/EXEC）→ ringbuf。
#[inline(always)]
fn submit_event(pid: i32, tid: i32, child_pid: i32, comm: [u8; 16], event_type: u32) {
    // EXIT 全量上报：线程退出 comm 不匹配包名键，过滤会漏线程退出
    // （IMPL-4 需要所有退出事件供用户态清 applied_tids）。
    if event_type != EVENT_EXIT && !comm_matches(&comm) {
        return;
    }
    if event_type != EVENT_EXIT {
        let now_ns = bpf_ktime_get_ns();
        let dedup_key = if event_type == EVENT_FORK {
            child_pid as u32
        } else {
            pid as u32
        };
        if should_dedup(dedup_key, now_ns) {
            return;
        }
    }

    let event = ProcEvent {
        pid,
        tid,
        child_pid,
        comm,
        event_type,
    };
    if let Some(mut entry) = EVENTS.reserve(0) {
        entry.write(event);
        entry.submit(0);
    }
}

/// 进程 fork / 线程 clone（分流在用户态，见模块注释）。
#[tracepoint(name = "sched_process_fork", category = "sched")]
fn sched_process_fork(ctx: TracePointContext) -> u32 {
    // format（5.15）：parent_comm[16]@8, parent_pid@24, child_comm[16]@28, child_pid@44
    let child_comm = unsafe { ctx.read_at::<[u8; 16]>(28).unwrap_or([0u8; 16]) };
    let child_pid = unsafe { ctx.read_at::<i32>(44).unwrap_or(0) };
    submit_event(child_pid, child_pid, child_pid, child_comm, EVENT_FORK);
    0
}

/// 进程 exec（新映像）。
#[tracepoint(name = "sched_process_exec", category = "sched")]
fn sched_process_exec(_ctx: TracePointContext) -> u32 {
    // 当前任务即 exec 者；comm 取 current（exec 早期，可能旧 comm——
    // 用户态 read_cmdline 才是权威，此处仅白名单粗过滤）。
    let pid_tgid = bpf_get_current_pid_tgid();
    let comm = bpf_get_current_comm().unwrap_or_default();
    submit_event((pid_tgid >> 32) as i32, pid_tgid as i32, 0, comm, EVENT_EXEC);
    0
}

/// 进程/线程退出（IMPL-4：用户态即时清 applied_tids，修 TID 复用窗口）。
#[tracepoint(name = "sched_process_exit", category = "sched")]
fn sched_process_exit(ctx: TracePointContext) -> u32 {
    // format（5.15）：comm[16]@8, pid@24, prio@28
    let comm = unsafe { ctx.read_at::<[u8; 16]>(8).unwrap_or([0u8; 16]) };
    let pid = unsafe { ctx.read_at::<i32>(24).unwrap_or(0) };
    submit_event(pid, pid, 0, comm, EVENT_EXIT);
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
