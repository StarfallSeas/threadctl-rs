//! threadctl-rs — daemon entry point.
//!
//! P2: Orchestrator main loop (P1 hot-reload + ProcSource event pipeline + relock + cleanup).

mod proc_source;

use std::env;
use std::fs;
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use threadctl_core::audit;
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
use threadctl_core::tracker::StateTracker;

use proc_source::ProcSource;

/// 单调秒。
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
    println!("  -v            Print version");
    println!("  -h            Print help");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = &args[0];

    let mut config_file = String::from("./threadctl.toml");
    let mut scan_interval: u64 = 2;

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
            "-v" => {
                println!("threadctl-rs {}", env!("CARGO_PKG_VERSION"));
                process::exit(0);
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

    let store = match ConfigStore::new(&config_file, topo.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("initial config load failed: {e}");
            process::exit(1);
        }
    };

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
    let mut source = ProcSource::new(tracker.clone());
    source.on_config_changed(&cfg);

    let reload_rx = spawn_hot_reload(store.clone(), scan_interval.max(cfg.engine.scan_interval));

    let mut last_lock = now_secs();
    let mut last_cleanup = now_secs();
    let mut last_fg_refresh = now_secs();
    let mut last_audit = now_secs();
    let mut sys_ctx_poller = AdaptivePoller::new();

    // P6.2-2：最近一次 SystemContext 快照（relock 决策的 fast 信号输入）。
    let mut last_sys: Option<SystemContext> = None;

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
        let now = now_secs();
        let cfg = store.current();

        // ── 配置变更：重扫白名单 + 全量刷新 ──
        while let Ok(version) = reload_rx.try_recv() {
            println!("config hot-reload: version {version}");
            source.on_config_changed(&cfg);
            {
                let mut t = lock_tracker(&tracker);
                t.retain_interested(&pkg_set(&cfg));
                let n = engine::relock_all(&mut t, &cfg, &topo, now, &build_relock_ctx(&last_sys), &decision_engine);
                println!("config change rescan done: applied {n} threads");
            }
        }

        // ── relock 周期锁定（对抗 Android 侧覆盖）──
        let lock_interval = cfg.engine.lock_interval;
        if lock_interval > 0 && now - last_lock >= lock_interval as i64 {
            let n = engine::relock_all(&mut lock_tracker(&tracker), &cfg, &topo, now, &build_relock_ctx(&last_sys), &decision_engine);
            last_lock = now;
            if n > 0 {
                println!("relock: re-applied {n} threads");
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
            let n = engine::handle_events(&mut lock_tracker(&tracker), &events, &cfg, &topo, now);
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

        // ── M4 接入：审计摘要（60s）──
        if now - last_audit >= 60 {
            println!("{}", audit::summary_string());
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
