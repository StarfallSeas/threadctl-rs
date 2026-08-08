//! ConfigStore — config hot-reload and versioned snapshots.
//!
//! Responsibility split:
//! - `ConfigStore`: holds the current snapshot; `current()` / `reload()` (no detection logic)
//! - `spawn_hot_reload`: inotify-first, poll-fallback detection thread that
//!   broadcasts each new version over a channel (one u64 per reload)
//!
//! Degradation chain (migrated from 既有实现, proven):
//!   inotify init failure → polling; watch death (DELETE/MOVE_SELF) → reinstall → failure → polling.

use std::ffi::CString;
use std::fs;
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use crate::config::ConfigSnapshot;
use crate::topology::CpuTopology;

/// 配置存储：持有当前生效快照，提供原子替换。
pub struct ConfigStore {
    snapshot: Mutex<Arc<ConfigSnapshot>>,
    config_file: String,
    topo: CpuTopology,
}

impl ConfigStore {
    /// 初始加载；失败返回错误（调用方应退出）。
    pub fn new(config_file: &str, topo: CpuTopology) -> Result<Arc<Self>, String> {
        let snapshot = ConfigSnapshot::load(config_file, &topo, 1)?;
        Ok(Arc::new(Self {
            snapshot: Mutex::new(snapshot),
            config_file: config_file.to_string(),
            topo,
        }))
    }

    /// 当前生效快照（Arc 克隆，O(1)）。
    pub fn current(&self) -> Arc<ConfigSnapshot> {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 重新读取 + 编译配置；成功则版本 +1 并替换快照，返回新版本号。
    /// 解析/编译失败时**保留旧快照**并返回错误。
    pub fn reload(&self) -> Result<u64, String> {
        let cur = self.current();
        let new_version = cur.version + 1;
        let new_snapshot = ConfigSnapshot::load(&self.config_file, &self.topo, new_version)?;
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = new_snapshot;
        Ok(new_version)
    }

    pub fn config_file(&self) -> &str {
        &self.config_file
    }
}

/// inotify watch 状态；初始化失败返回 None（调用方降级轮询）。
struct InotifyWatch {
    fd: i32,
    wd: i32,
    config_file: String,
}

impl InotifyWatch {
    fn init(config_file: &str) -> Option<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return None;
        }
        let cstr = match CString::new(config_file) {
            Ok(c) => c,
            Err(_) => {
                unsafe { libc::close(fd) };
                return None;
            }
        };
        let wd = unsafe {
            libc::inotify_add_watch(
                fd,
                cstr.as_ptr(),
                libc::IN_CLOSE_WRITE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
            )
        };
        if wd < 0 {
            unsafe { libc::close(fd) };
            return None;
        }
        Some(Self { fd, wd, config_file: config_file.to_string() })
    }

    /// 等待事件。返回：
    /// - `Some(true)`：需要重载配置
    /// - `Some(false)`：超时或无可重载事件
    /// - `None`：watch 失效且无法恢复，调用方应降级为轮询
    fn wait(&mut self, poll_interval: u64) -> Option<bool> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, (poll_interval as libc::c_int) * 1000) };

        if ret < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                return Some(false);
            }
            return None;
        }
        if ret == 0 {
            return Some(false);
        }

        #[repr(align(8))]
        struct Buf([u8; 4096]);
        let mut buf = Buf([0u8; 4096]);
        let len = unsafe {
            libc::read(self.fd, buf.0.as_mut_ptr() as *mut libc::c_void, buf.0.len())
        };
        if len <= 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EAGAIN)
                && err.raw_os_error() != Some(libc::EWOULDBLOCK)
            {
                return None;
            }
            return Some(false);
        }

        let mut reload_needed = false;
        let mut offset = 0;
        let hdr = std::mem::size_of::<libc::inotify_event>();
        while offset + hdr <= len as usize {
            let ev = unsafe { &*(buf.0.as_ptr().add(offset) as *const libc::inotify_event) };
            if ev.mask & (libc::IN_CLOSE_WRITE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                reload_needed = true;
                if ev.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                    // 文件被替换/删除：等待重建后重装 watch。
                    // L7 修复：短等 1s（原为整个 poll_interval，60s 间隔时恢复监听延迟过大）
                    thread::sleep(Duration::from_secs(1));
                    if !self.reinstall() {
                        return None;
                    }
                }
            }
            offset += hdr + ev.len as usize;
        }
        Some(reload_needed)
    }

    /// Reinstall the watch; false → caller falls back to polling.
    fn reinstall(&mut self) -> bool {
        // NOTE: libc crate signature is inotify_rm_watch(fd: i32, wd: u32) —
        // the `as u32` cast is correct (Claude NEW-L1 flagged it, verified against
        // the actual libc version in use: it takes u32).
        unsafe { libc::inotify_rm_watch(self.fd, self.wd as u32) };
        let cstr = match CString::new(self.config_file.as_str()) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let new_wd = unsafe {
            libc::inotify_add_watch(
                self.fd,
                cstr.as_ptr(),
                libc::IN_CLOSE_WRITE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
            )
        };
        if new_wd < 0 {
            return false;
        }
        self.wd = new_wd;
        true
    }
}

impl Drop for InotifyWatch {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// 配置文件 mtime（纳秒）；读取失败返回 -1。
fn file_mtime(path: &str) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(-1)
}

/// 启动热加载线程。返回变更通知 receiver：每次成功重载发送新版本号。
///
/// `poll_interval`：inotify poll 超时 / 轮询模式间隔（秒）。
pub fn spawn_hot_reload(store: Arc<ConfigStore>, poll_interval: u64) -> mpsc::Receiver<u64> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let name = CString::new("ConfigLoader").unwrap_or_default();
        unsafe { libc::pthread_setname_np(libc::pthread_self(), name.as_ptr()) };

        let mut watch = InotifyWatch::init(&store.config_file());
        let mut inotify_active = watch.is_some();
        if inotify_active {
            println!("config hot-reload: inotify enabled");
        } else {
            println!("config hot-reload: inotify unavailable, using polling");
        }

        // 轮询模式的 mtime 基线：初始为当前值，避免启动即重复加载。
        let mut last_mtime = file_mtime(&store.config_file());

        loop {
            if inotify_active {
                match watch.as_mut().unwrap().wait(poll_interval) {
                    Some(true) => {
                        if let Err(e) = store.reload() {
                            eprintln!("config reload failed: {e} (keeping old config)");
                        } else {
                            let v = store.current().version;
                            let _ = tx.send(v);
                        }
                    }
                    Some(false) => {}
                    None => {
                        eprintln!("config hot-reload: inotify failed, degrading to polling");
                        inotify_active = false;
                        watch = None;
                        // 降级后立即用当前 mtime 重载一次
                        last_mtime = -1;
                    }
                }
            } else {
                let mt = file_mtime(&store.config_file());
                if mt != last_mtime {
                    last_mtime = mt;
                    if mt == -1 {
                        // M6 修复：文件缺失时静默等待（避免每轮"配置重载失败"刷屏）
                    } else if let Err(e) = store.reload() {
                        eprintln!("config reload failed: {e} (keeping old config)");
                    } else {
                        let v = store.current().version;
                        let _ = tx.send(v);
                    }
                }
                thread::sleep(Duration::from_secs(poll_interval));
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_config(content: &str) -> (std::path::PathBuf, String) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("threadctl-test-{}-{n}.toml", std::process::id()));
        fs::write(&path, content).unwrap();
        let path_str = path.to_string_lossy().into_owned();
        (path, path_str)
    }

    #[test]
    fn store_reload_increments_version() {
        let (path, path_str) = temp_config("[engine]\nscan_interval = 1\n");
        let topo = CpuTopology::default();
        let store = ConfigStore::new(&path_str, topo).expect("init");
        assert_eq!(store.current().version, 1);

        fs::write(&path, "[engine]\nscan_interval = 2\n").unwrap();
        let v = store.reload().expect("reload");
        assert_eq!(v, 2);
        assert_eq!(store.current().version, 2);
        assert_eq!(store.current().engine.scan_interval, 2);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn store_reload_keeps_old_on_error() {
        let (path, path_str) = temp_config("[engine]\nscan_interval = 1\n");
        let topo = CpuTopology::default();
        let store = ConfigStore::new(&path_str, topo).expect("init");

        fs::write(&path, "key = 1\nkey = 2\n").unwrap(); // 重复键 — TOML 规格禁止
        assert!(store.reload().is_err(), "坏配置应报错");
        assert_eq!(store.current().version, 1, "旧快照应保留");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn store_current_is_cheap_clone() {
        let (_, path_str) = temp_config("[engine]\nscan_interval = 1\n");
        let topo = CpuTopology::default();
        let store = ConfigStore::new(&path_str, topo).expect("init");
        let a = store.current();
        let b = store.current();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
