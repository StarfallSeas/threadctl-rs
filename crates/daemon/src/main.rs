//! threadctl-rs — daemon entry point.
//!
//! P2: Orchestrator main loop (P1 hot-reload + ProcSource event pipeline + relock + cleanup).

mod ebpf_source;
mod ipc;
mod proc_source;

use std::env;
use std::fs;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use threadctl_core::audit;
use threadctl_core::debug;
use threadctl_core::capability::CapabilitySet;
use threadctl_core::caps::can_rt_sched;
use threadctl_core::config::ConfigSnapshot;
use threadctl_core::decision::{DecisionEngine, MigrateAction};
use threadctl_core::engine::{self, RelockContext};
use threadctl_core::event::EventSource;
use threadctl_core::foreground::refresh_foreground_uids;
use threadctl_core::store::{spawn_hot_reload, ConfigStore};
use threadctl_core::system_context::{AdaptivePoller, PressureLevel, SystemContext};
use threadctl_core::topology::init_cpu_topo;
use threadctl_core::relock::{sample_coverage, AdaptiveRelock, RelockGuard, SAMPLE_INTERVAL_SECS};
use threadctl_core::topology::BASE_CPUSET;
use threadctl_core::tracker::StateTracker;

use ebpf_source::EbpfSource;
use ipc::IpcRequest;
use proc_source::ProcSource;

/// P7.3 (NEW-L4)：SIGTERM/SIGINT → 主循环优雅退出（正常走 cleanup，不硬杀）。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_shutdown_signal(_: libc::c_int) {
    SHUTDOWN.store(true, AtomicOrdering::Relaxed);
}

fn install_signal_handlers() {
    unsafe {
        // rustc 新版：函数项先 cast 指针再转 sighandler_t
        let h = handle_shutdown_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }
}

/// 单调秒。
/// P7.3 (C1)：IPC 命令处理（主循环持有 tracker 执行，响应回写）。
fn handle_ipc(
    req: &IpcRequest,
    tracker: &mut StateTracker,
    cfg: &ConfigSnapshot,
    topo: &threadctl_core::topology::CpuTopology,
    backend: &threadctl_core::backend::LinuxV1Backend,
    store: &ConfigStore,
    source: &mut dyn EventSource,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    match req {
        IpcRequest::Status => {
            let _ = writeln!(out, "== threadctl status ==");
            let _ = writeln!(out, "version      : {}", env!("CARGO_PKG_VERSION"));
            let _ = writeln!(out, "config       : v{} ({} pkgs)", cfg.version, cfg.rules.pkgs().len());
            let tracked = engine::tracked_summary(tracker);
            let _ = writeln!(out, "tracked      : {} processes / {} threads",
                tracked.len(), tracked.iter().map(|(_, _, t)| t).sum::<usize>());
            for (pid, pkg, n) in tracked.iter().take(20) {
                let _ = writeln!(out, "  {pid:>7}  {pkg:<24} {n} threads");
            }
            let a = threadctl_core::audit::summary_windowed(60);
            let _ = writeln!(out, "audit(60s)   : total={} success={} cgroup_blocked={} downgraded={}",
                a.total_attempts, a.success, a.blocked_by_cgroup, a.downgraded);
            let r = engine::relock_stats();
            let _ = writeln!(out, "relock       : allow={} skip={} degrade={}", r.allow, r.skip, r.degrade);
        }
        IpcRequest::Dump(pid) => {
            match tracker.get(*pid) {
                Some(st) => {
                    let _ = writeln!(out, "== pid {} ({}) ==", pid, st.pkg);
                    let _ = writeln!(out, "  applied_tids : {}", st.applied_tids.len());
                    let allowed = threadctl_core::topology::CpuSet::read_allowed_mask(*pid)
                        .map(|m| m.to_range_string()).unwrap_or_else(|| "<read failed>".into());
                    let _ = writeln!(out, "  allowed_mask : {allowed}");
                    let cpuset = threadctl_core::relock::read_cpuset_owner(*pid).unwrap_or_else(|| "<none>".into());
                    let _ = writeln!(out, "  cpuset       : {cpuset}");
                    let alive = threadctl_core::proc::is_alive(*pid);
                    let _ = writeln!(out, "  alive        : {alive}");
                }
                None => {
                    let _ = writeln!(out, "pid {pid} 不在跟踪列表（未配置或已退出）");
                }
            }
        }
        IpcRequest::Reload => {
            match store.reload() {
                Ok(v) => {
                    let _ = writeln!(out, "reload: 配置已重载 (version {v})");
                    // 与热加载同路径：通知事件源 + 保留白名单 + 全量刷新
                    let cfg = store.current();
                    source.on_config_changed(&cfg);
                    tracker.retain_interested(&pkg_set(&cfg));
                    let n = engine::relock_all(tracker, &cfg, topo, now_secs(),
                        &RelockContext { pressure: PressureLevel::Normal, thermal_pressure: 0.0, audit_failure_rate: 0.0 },
                        &threadctl_core::decision::DecisionEngine::default(), backend);
                    let _ = writeln!(out, "reload: applied {n} threads");
                }
                Err(e) => {
                    let _ = writeln!(out, "reload 失败: {e}");
                }
            }
        }
        IpcRequest::Apply(pid) => {
            let n = engine::relock_all(tracker, cfg, topo, now_secs(),
                &RelockContext { pressure: PressureLevel::Normal, thermal_pressure: 0.0, audit_failure_rate: 0.0 },
                &threadctl_core::decision::DecisionEngine::default(), backend);
            let _ = writeln!(out, "apply {pid}: 全量重应用完成 (applied {n} threads)");
        }
    }
    out
}

fn now_secs() -> i64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64
}

fn lock_tracker(t: &Mutex<StateTracker>) -> std::sync::MutexGuard<'_, StateTracker> {
    t.lock().unwrap_or_else(|e| e.into_inner())
}

fn print_help(prog: &str) {
    println!("Usage: {prog} [OPTIONS]");
    println!("Options:");
    println!("  -c <file>     Config file (default: ./threadctl.toml)");
    println!("  -s <secs>     Scan interval in seconds (default: 2)");
    println!("  --debug       Verbose debug logging (or env TC_DEBUG=1)");
    println!("  -v            Print version");
    println!("  -h            Print help");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = &args[0];

    let mut config_file = String::from("./threadctl.toml");
    let mut scan_interval: u64 = 2;
    let mut dry_run = false;

    // P7.3 (C1)：CLI 子命令模式（不启动 daemon，连接运行中的 daemon）
    if args.len() >= 2 && matches!(args[1].as_str(), "status" | "dump" | "reload" | "apply") {
        let sock = env::var("TC_SOCKET").unwrap_or_else(|_| "./run/threadctl.sock".into());
        let cmd = args[1..].join(" ");
        process::exit(ipc::cli_command(&sock, &cmd));
    }

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => {
                i += 1;
                match args.get(i) {
                    Some(v) => config_file = v.clone(),
                    None => {
                        eprintln!("error: -c requires a config file path");
                        process::exit(1);
                    }
                }
            }
            "-s" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) if v >= 1 => scan_interval = v,
                    _ => {
                        eprintln!("error: -s requires an integer interval >= 1");
                        process::exit(1);
                    }
                }
            }
            "-t" | "--dry-run" => {
                dry_run = true;
            }
            "-v" => {
                println!("threadctl-rs {}", env!("CARGO_PKG_VERSION"));
                process::exit(0);
            }
            "--debug" => {
                debug::set_debug(true);
            }
            "-h" => {
                print_help(prog);
                process::exit(0);
            }
            o => {
                eprintln!("unknown option: {o}");
                print_help(prog);
                process::exit(1);
            }
        }
        i += 1;
    }

    // TC_DEBUG 环境变量兜底（--debug 优先，env 适用脚本/su 场景）
    if env::var("TC_DEBUG").map(|v| v == "1").unwrap_or(false) {
        debug::set_debug(true);
    }
    if debug::enabled() {
        println!("[debug] debug logging enabled (TC_DEBUG=1 / --debug)");
    }

    // 配置不存在时生成默认模板。
    if fs::metadata(&config_file).is_err() {
        match fs::write(&config_file, ConfigSnapshot::default_template()) {
            Ok(()) => println!("配置文件不存在，已生成默认模板: {config_file}"),
            Err(e) => eprintln!("警告: 无法生成默认配置 {config_file}: {e}"),
        }
    }

    let topo = init_cpu_topo();
    println!(
        "CPU topology: {} present, cpuset {}",
        topo.present_cpus.count(),
        if topo.cpuset_enabled { "available" } else { "unavailable" }
    );
    for cl in &topo.clusters {
        println!(
            "  {:?} cluster: {} (capacity={})",
            cl.kind, cl.range_str, cl.capacity
        );
    }
    // P6.3 M2：DVFS 域打印（policyN 分组，用户可对照官方规格/电压表）
    if !topo.dvfs_domains.is_empty() {
        let parts: Vec<String> = topo.dvfs_domains.iter().map(|d| d.to_range_string()).collect();
        println!("DVFS domains: [{}]", parts.join(", "));
    }

    let store = match ConfigStore::new(&config_file, topo.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("initial config load failed: {e}");
            process::exit(1);
        }
    };
    let cfg = store.current();

    // P7.3 (C2)：dry-run——解析 + 编译 + 打印规则（不启动 daemon）
    if dry_run {
        println!("== dry-run: {config_file} ==");
        println!("engine: mode={:?} scan={}s lock={}s", cfg.engine.mode, cfg.engine.scan_interval, cfg.engine.lock_interval);
        for line in cfg.rules.dry_run_lines() {
            println!("{line}");
        }
        println!("== dry-run: 配置有效（{} 包）==", cfg.rules.pkgs().len());
        process::exit(0);
    }

    let cfg = store.current();
    println!(
        "initial config loaded: version {}, {} rule packages",
        cfg.version,
        cfg.rules.pkg_list().len()
    );

    // Q6：RT 调度权限检查。
    let can_rt = can_rt_sched();
    println!("RT scheduling: {}", if can_rt { "full" } else { "none (fifo/rr will be skipped)" });
    if cfg.rules.has_rt_sched() && !can_rt {
        eprintln!("warning: config contains fifo/rr rules but CAP_SYS_NICE is missing; sched fields will be skipped");
    }

    // M4 接入：能力检测 + 决策引擎初始化
    let caps = CapabilitySet::detect();
    println!("{}", caps.summary());
    let decision_engine = DecisionEngine {
        force_affinity_enabled: cfg.engine.migrate_action == MigrateAction::Force,
        pressure_sensitive: cfg.engine.pressure_sensitive,
    };
    let _ = &decision_engine; // P6 深度接入策略决策，当前初始化并保留

    let tracker = Arc::new(Mutex::new(StateTracker::new()));
    // P7.1（ARCH-1）：事件源走 trait 对象注入——EbpfSource 优先（内核事件驱动，
    // near-real-time 事件发现），加载/attach 任何失败 → 回退 ProcSource（/proc 轮询）。
    // 构建产物需把 threadctl-ebpf .bpf.o 与 daemon 放同目录。
    let mut source: Box<dyn EventSource> = match EbpfSource::try_new(tracker.clone(), &cfg) {
        Ok(s) => {
            println!("event source: ebpf (kernel tracepoints)");
            Box::new(s)
        }
        Err(e) => {
            eprintln!("warning: ebpf unavailable ({e}) — falling back to /proc polling");
            Box::new(ProcSource::new(tracker.clone()))
        }
    };
    source.on_config_changed(&cfg);

    let reload_rx = spawn_hot_reload(store.clone(), scan_interval.max(cfg.engine.scan_interval));

    let mut last_lock = now_secs();
    let mut last_cleanup = now_secs();
    let mut last_fg_refresh = now_secs();
    // P7.2（B1+D3）：自适应 relock 周期 + 统一冷却闸门（ARCH-3）
    let mut adaptive = AdaptiveRelock::from_initial(cfg.engine.lock_interval.max(1));
    let mut relock_guard = RelockGuard::new();
    relock_guard.set_cooldown(1000); // D3 即时 relock 冷却 1s（防风暴）
    let mut last_coverage_sample = now_secs();

    // P7.3 (C1)：IPC 控制面（Unix socket + mpsc 回主循环）
    install_signal_handlers();
    let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<(IpcRequest, std::sync::mpsc::Sender<String>)>();
    let ipc_socket = cfg.daemon.ipc_socket.clone();
    match ipc::spawn_ipc_server(&ipc_socket, ipc_tx) {
        Ok(_) => println!("ipc socket: {ipc_socket} (root-only 0750)"),
        Err(e) => eprintln!("warning: IPC 启动失败 ({ipc_socket}): {e}——CLI status/dump/reload 不可用"),
    }
    let mut last_audit = now_secs();
    let mut sys_ctx_poller = AdaptivePoller::new();

    // P6.2-2：最近一次 SystemContext 快照（relock 决策的 fast 信号输入）。
    let mut last_sys: Option<SystemContext> = None;
    let backend = threadctl_core::backend::default_backend();

    // 组装 relock 决策上下文（fast: pressure/thermal + slow: audit failure rate）。
    let build_relock_ctx = |last_sys: &Option<SystemContext>| -> RelockContext {
        let (pressure, thermal) = match last_sys {
            Some(s) => (s.memory_pressure, s.thermal_pressure),
            None => (PressureLevel::Normal, 0.0),
        };
        RelockContext {
            pressure,
            thermal_pressure: thermal,
            audit_failure_rate: audit::summary_windowed(60).failure_rate(),
        }
    };

    println!("threadctl-rs v{} started (P2: proc event pipeline)", env!("CARGO_PKG_VERSION"));

    loop {
        if SHUTDOWN.load(AtomicOrdering::Relaxed) {
            println!("shutdown signal received, exiting");
            break;
        }
        let now = now_secs();
        let cfg = store.current();

        // ── P7.3 (C1)：IPC 请求处理（status/dump/reload/apply）──
        while let Ok((req, reply_tx)) = ipc_rx.try_recv() {
            let resp = handle_ipc(&req, &mut lock_tracker(&tracker), &cfg, &topo, &backend, &store, source.as_mut());
            let _ = reply_tx.send(resp);
        }

        // ── 配置变更：重扫白名单 + 全量刷新 ──
        while let Ok(version) = reload_rx.try_recv() {
            println!("config hot-reload: version {version}");
            source.on_config_changed(&cfg);
            {
                let mut t = lock_tracker(&tracker);
                t.retain_interested(&pkg_set(&cfg));
                let n = engine::relock_all(&mut t, &cfg, &topo, now, &build_relock_ctx(&last_sys), &decision_engine, &backend);
                println!("config change rescan done: applied {n} threads");
            }
        }

        // ── P7.2 B1/D3：覆盖采样 → 自适应周期 + 即时 relock（对抗 AMS 覆盖）──
        if cfg.engine.lock_interval > 0 && now - last_coverage_sample >= SAMPLE_INTERVAL_SECS as i64 {
            last_coverage_sample = now;
            let ratio = sample_coverage(&lock_tracker(&tracker), BASE_CPUSET);
            let before = adaptive.interval_secs();
            adaptive.observe_ratio(ratio);
            let after = adaptive.interval_secs();
            if after != before {
                println!("adaptive relock: {}s -> {}s (coverage {:.0}%)", before, after, ratio * 100.0);
            }
            // D3：检测到覆盖 → 不等周期即时 relock（guard 1s 冷却防风暴；
            // 与周期 relock 共享 guard，防 AMS↔threadctl 震荡）
            if ratio > 0.0 && relock_guard.try_lock(Instant::now()) {
                let n = engine::relock_all(&mut lock_tracker(&tracker), &cfg, &topo, now, &build_relock_ctx(&last_sys), &decision_engine, &backend);
                last_lock = now;
                if n > 0 {
                    println!("d3 immediate relock: re-applied {n} threads");
                }
            }
        }

        // ── relock 周期锁定（对抗 Android 侧覆盖；B1 自适应周期）──
        if cfg.engine.lock_interval > 0 {
            let lock_interval = adaptive.interval_secs();
            if now - last_lock >= lock_interval as i64 {
                if relock_guard.try_lock(Instant::now()) {
                    let n = engine::relock_all(&mut lock_tracker(&tracker), &cfg, &topo, now, &build_relock_ctx(&last_sys), &decision_engine, &backend);
                    last_lock = now;
                    if n > 0 {
                        println!("relock: re-applied {n} threads");
                    }
                }
            }
        }

        // ── 死进程清理 ──
        let dead_interval = cfg.engine.dead_cleanup_interval;
        if now - last_cleanup >= dead_interval.max(1) as i64 {
            let removed = engine::cleanup_dead(&mut lock_tracker(&tracker));
            last_cleanup = now;
            if removed > 0 {
                println!("dead process cleanup: {removed}");
            }
        }

        // ── 事件轮询 ──
        let poll_interval = scan_interval.max(cfg.engine.scan_interval);
        let deadline = Instant::now() + Duration::from_secs(poll_interval);
        let events = source.poll(deadline);
        if !events.is_empty() {
            let n = engine::handle_events(&mut lock_tracker(&tracker), &events, &cfg, &topo, now, &backend);
            if n > 0 {
                println!("events: {} , applied {} threads", events.len(), n);
            }
        }

        // ── M4 接入：SystemContext 自适应采样（压力异常时打印）──
        if sys_ctx_poller.should_sample() {
            let ctx = SystemContext::sample();
            if ctx.memory_pressure != threadctl_core::system_context::PressureLevel::Normal {
                println!("SystemContext: {}", ctx.summary());
            }
            sys_ctx_poller.sampled(&ctx);
            // P6.2-2：保存快照供 relock 决策（fast 信号）
            last_sys = Some(ctx);
        }

        // ── M4 接入：前台 UID 缓存刷新（30s）──
        if now - last_fg_refresh >= 30 {
            let n = refresh_foreground_uids();
            if n > 0 {
                debug_fg(&n);
            }
            last_fg_refresh = now;
        }

        // ── M4 接入：审计摘要 + relock 决策统计（60s）──
        if now - last_audit >= 60 {
            println!("{}", audit::summary_string());
            let rs = engine::relock_stats();
            println!(
                "relock decisions: allow={} skip={} degrade={}",
                rs.allow, rs.skip, rs.degrade
            );
            last_audit = now;
        }
    }
}

fn debug_fg(n: &usize) {
    // 前台 UID 数量变化仅在 debug 级别体现；INFO 级别只打印启动时
    let _ = n;
}

/// 当前规则包集合（retain_interested 用）。
fn pkg_set(cfg: &ConfigSnapshot) -> std::collections::HashSet<String> {
    cfg.rules.pkg_list().iter().cloned().collect()
}
