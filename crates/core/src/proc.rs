//! /proc read utilities — stack-allocated path buffers.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::FileExt;

use crate::topology::{MAX_PKG_LEN, MAX_THREAD_LEN};

/// Build a path on the stack and read the file head; the caller truncates (\0/\n).
fn read_proc_file<'a>(pid: i32, suffix: &str, buf: &'a mut [u8]) -> Option<&'a [u8]> {
    let mut path = [0u8; 32];
    let mut cur = std::io::Cursor::new(&mut path[..]);
    write!(cur, "/proc/{}/{}", pid, suffix).ok()?;
    let len = cur.position() as usize;
    let path_str = std::str::from_utf8(&path[..len]).ok()?;
    let file = fs::File::open(path_str).ok()?;
    let n = file.read_at(buf, 0).ok()?;
    (n > 0).then_some(&buf[..n])
}

/// Process executable name (last path segment of cmdline).
pub fn read_cmdline(pid: i32) -> Option<String> {
    let mut buf = [0u8; MAX_PKG_LEN];
    let bytes = read_proc_file(pid, "cmdline", &mut buf)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let cmdline = std::str::from_utf8(&bytes[..end]).ok()?;
    let name = cmdline.rsplit('/').next().unwrap_or(cmdline);
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Thread name (/proc/<tid>/comm).
pub fn read_thread_name(tid: i32) -> Option<String> {
    let mut buf = [0u8; MAX_THREAD_LEN];
    let bytes = read_proc_file(tid, "comm", &mut buf)?;
    let end = bytes
        .iter()
        .position(|&b| b == 0 || b == b'\n')
        .unwrap_or(bytes.len());
    let name = std::str::from_utf8(&bytes[..end]).ok()?;
    Some(name.trim().to_string())
}

/// Process start time (/proc/<pid>/stat field 22, jiffies), used for PID-reuse detection.
pub fn read_start_time(pid: i32) -> Option<u64> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // /proc/pid/stat: "pid (comm) state ... 22nd field = starttime"
    // comm in parens may contain spaces/parens, so parse from the right paren onward
    let after_comm = content.rsplit_once(')')?.1;
    after_comm
        .split_whitespace()
        // 20th whitespace-separated field after ')' (0-indexed 19),
        // i.e. /proc/pid/stat field 22 = starttime
        .nth(19)
        .and_then(|v| v.parse::<u64>().ok())
}

/// Process oom_score_adj (foreground-aware relock).
/// Returns 0 (foreground default, safe side: prefer relocking over dropping rules) on failure.
pub fn read_oom_adj(pid: i32) -> i32 {
    fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

/// 读 `/proc/<pid>/status` 的 Tgid（P7.1 fork 分流：Tgid == Pid → 进程 fork；
/// Tgid != Pid → 线程 clone——eBPF fork 事件无法从 tracepoint 参数拿 child tgid，
/// 用户态 pending 后读这里，与 Zygote pending 天然合并）。
pub fn read_tgid(pid: i32) -> Option<i32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|l| {
        let l = l.trim();
        if let Some(v) = l.strip_prefix("Tgid:") {
            v.trim().parse::<i32>().ok()
        } else {
            None
        }
    })
}

/// Process liveness (Claude DESIGN-3: kill(pid,0) may return EPERM under
/// SELinux restrictions for an alive process — EPERM means "exists but no
/// signal permission", must be treated as alive, otherwise running processes
/// would be wrongly removed from the tracker).
pub fn is_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Enumerate all tids under /proc/<pid>/task.
pub fn list_tids(pid: i32) -> Vec<i32> {
    let mut path = [0u8; 32];
    let mut cur = std::io::Cursor::new(&mut path[..]);
    write!(cur, "/proc/{}/task", pid).ok();
    let len = cur.position() as usize;
    let path_str = match std::str::from_utf8(&path[..len]) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let Ok(dir) = fs::read_dir(path_str) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_self_cmdline_and_comm() {
        let pid = std::process::id() as i32;
        let name = read_cmdline(pid);
        assert!(name.is_some(), "当前进程 cmdline 可读");

        let tid = pid; // 主线程 tid == pid
        let comm = read_thread_name(tid);
        assert!(comm.is_some(), "当前线程 comm 可读");
    }

    #[test]
    fn list_self_tids_nonempty() {
        let pid = std::process::id() as i32;
        let tids = list_tids(pid);
        assert!(!tids.is_empty(), "当前进程至少一个线程");
        assert!(tids.contains(&pid));
    }
}
