//! threadctl-rs — daemon entry point.
//!
//! P2: Orchestrator main loop (P1 hot-reload + ProcSource event pipeline + relock + cleanup).

mod ebpf_source;
mod ipc;
mod proc_source;

use std::env;
use std::fs;
use std::process;
use std::thread;
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
use threadctl_core::observe::{Sampler, SnapshotWindow};
use threadctl_core::i18n;
use threadctl_core::debug_log;
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
/// P9：debug 日志辅助——事件批里的不同包数（调试流信息）。
fn count_pkgs(events: &[threadctl_core::event::ProcessEvent]) -> usize {
    let mut seen = std::collections::HashSet::new();
    for ev in events {
        if let Some(pkg) = ev.pkg.as_deref() {
            seen.insert(pkg);
        }
    }
    seen.len()
}

/// P7.3 (CLAUDE BUG-M1)：配置重载公共路径——热加载线程与 IPC reload 命令
/// 共用。消除重复逻辑 + 统一使用真实 decision_engine/压力上下文
///（此前 IPC 路径用 DecisionEngine::default()，决策与主循环不一致）。
fn do_reload(
    store: &ConfigStore,
    source: &mut dyn EventSource,
    tracker: &mut StateTracker,
    topo: &threadctl_core::topology::CpuTopology,
    backend: &threadctl_core::backend::LinuxV1Backend,
    decision: &threadctl_core::decision::DecisionEngine,
    rctx: &RelockContext,
) -> usize {
    let cfg = store.current();
    source.on_config_changed(&cfg);
    tracker.retain_interested(&pkg_set(&cfg));
    engine::relock_all(tracker, &cfg, topo, now_secs(), rctx, decision, backend)
}

/// P7.3 (C1)：IPC 命令处理（主循环持有 tracker 执行，响应回写）。
fn handle_ipc(
    req: &IpcRequest,
    tracker: &mut StateTracker,
    cfg: &ConfigSnapshot,
    topo: &threadctl_core::topology::CpuTopology,
    backend: &threadctl_core::backend::LinuxV1Backend,
    store: &ConfigStore,
    source: &mut dyn EventSource,
    decision: &threadctl_core::decision::DecisionEngine,
    rctx: &RelockContext,
    window: &SnapshotWindow,
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
                    // CLAUDE BUG-M1：与热加载共用 do_reload（真实 decision/rctx）
                    let n = do_reload(store, source, tracker, topo, backend, decision, rctx);
                    let _ = writeln!(out, "reload: applied {n} threads");
                }
                Err(e) => {
                    let _ = writeln!(out, "reload 失败: {e}");
                }
            }
        }
        IpcRequest::Apply(pid) => {
            let n = engine::relock_all(tracker, cfg, topo, now_secs(), rctx, decision, backend);
            // CLAUDE BUG-M3：当前版本全量重应用（pid 参数保留用于 P8 单进程精确）
            let _ = writeln!(out, "apply {pid}: 全量重应用完成 (applied {n} threads)（当前为全量，P8 支持单 pid）");
        }
        IpcRequest::ApplyScene(name) => {
            // P12：场景一键套用——写回配置（标记段）→ 触发 reload
            let content = match fs::read_to_string(&cfg.config_file) {
                Ok(c) => c,
                Err(e) => {
                    let _ = writeln!(out, "{}", i18n::t(format!("场景失败: 读取配置失败 {e}").as_str(),
                        format!("apply-scene failed: cannot read config {e}").as_str()));
                    return out;
                }
            };
            match threadctl_core::scene::apply_scene_to_config(&content, name) {
                Ok(new_content) => {
                    if let Err(e) = fs::write(&cfg.config_file, new_content) {
                        let _ = writeln!(out, "{}", i18n::t(format!("场景失败: 写回配置失败 {e}").as_str(),
                            format!("apply-scene failed: cannot write config {e}").as_str()));
                        return out;
                    }
                    // 触发热重载（与 reload 命令同路径）
                    match store.reload() {
                        Ok(v) => {
                            let n = do_reload(store, source, tracker, topo, backend, decision, rctx);
                            let _ = writeln!(out, "{}", i18n::t(
                                format!("场景已应用: {name}（配置版本 {v}，重应用 {n} 线程）").as_str(),
                                format!("scene applied: {name} (config v{v}, re-applied {n} threads)").as_str()));
                        }
                        Err(e) => {
                            let _ = writeln!(out, "{}", i18n::t(format!("场景写回成功但重载失败: {e}").as_str(),
                                format!("scene written but reload failed: {e}").as_str()));
                        }
                    }
                }
                Err(e) => {
                    let _ = writeln!(out, "{}", i18n::t(format!("场景失败: {e}").as_str(),
                        format!("apply-scene failed: {e}").as_str()));
                }
            }
        }
        IpcRequest::Tune(kind, value) => {
            let result = match (kind.as_str(), value.as_str()) {
                ("governor", v) if !v.is_empty() => {
                    threadctl_core::tune::apply_governor(v).map(|n| format!("{n} CPUs"))
                }
                ("iosched", v) if !v.is_empty() => {
                    threadctl_core::tune::apply_io_scheduler(v).map(|n| format!("{n} devices"))
                }
                _ => Err(format!("tune 用法: tune governor <name> | tune iosched <name>")),
            };
            match result {
                Ok(detail) => {
                    let _ = writeln!(out, "{}", i18n::t(format!("tune {kind} {value}: 已应用（{detail}）").as_str(),
                        format!("tune {kind} {value}: applied ({detail})").as_str()));
                }
                Err(e) => {
                    let _ = writeln!(out, "{}", i18n::t(format!("tune 失败: {e}").as_str(),
                        format!("tune failed: {e}").as_str()));
                }
            }
        }
        IpcRequest::Snapshot(filter) => {
            // P8：线程观测——"0-7[4]" 展示：affinity + [当前运行核]
            let _ = writeln!(out, "== threadctl snapshot (窗口统计) ==");
            let _ = writeln!(out, "{:>8}  {:<20} {:>8} {:>4} {:>5} {:>4} {:>6} {:>6}  {}", "tid", "name", "affinity", "cur", "avg%", "max%", "migr", "affChg", "primary");
            for st in window.stats() {
                if let Some(pid) = filter {
                    // 过滤：pid 匹配该线程所属进程（window 只存 tid——查 tracker 归属）
                    let owned = tracker.get(*pid).map(|p| p.applied_tids.contains(&st.tid)).unwrap_or(false);
                    if !owned {
                        continue;
                    }
                }
                let (cur_cpu, cur_aff) = window
                    .recent_sample(st.tid)
                    .map(|(cpu, aff)| (cpu.map(|c| c.to_string()).unwrap_or_else(|| "-".into()), aff))
                    .unwrap_or(("-".into(), "-".into()));
                let _ = writeln!(
                    out,
                    "{:>8}  {:<20} {:>8} {:>4} {:>5} {:>4} {:>6} {:>6}  {}",
                    st.tid,
                    st.name.chars().take(20).collect::<String>(),
                    cur_aff,
                    cur_cpu,
                    st.avg_load_pct,
                    st.max_load_pct,
                    st.migrations,
                    st.affinity_changes,
                    st.primary_cpu.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
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
    println!("  -c <file>     Config file (default: ./threadctl.kdl)");
    println!("  -s <secs>     Scan interval in seconds (default: 2)");
    println!("  --debug       Verbose debug logging (or env TC_DEBUG=1)");
    println!("  -v            Print version");
    println!("  -h            Print help");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = &args[0];

    let mut config_file = String::from("./threadctl.kdl");
    let mut scan_interval: u64 = 2;
    let mut dry_run = false;

    // P7.3 (C1)：CLI 子命令模式（不启动 daemon，连接运行中的 daemon）
    if args.len() >= 2 && matches!(args[1].as_str(), "status" | "dump" | "reload" | "apply" | "snapshot" | "apply-scene" | "tune") {
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
            eprintln!("{}", i18n::t(format!("初始配置加载失败: {e}").as_str(), format!("initial config load failed: {e}").as_str()));
            process::exit(1);
        }
    };
    let cfg = store.current();

    // P7.3 (C2)：dry-run——解析 + 编译 + 打印规则（不启动 daemon）
    if dry_run {
        println!("{}", i18n::t(format!("== dry-run: {config_file} ==").as_str(), format!("== dry-run: {config_file} ==").as_str()));
        println!("engine: mode={:?} scan={}s lock={}s", cfg.engine.mode, cfg.engine.scan_interval, cfg.engine.lock_interval);
        for line in cfg.rules.dry_run_lines() {
            println!("{line}");
        }
        println!("{}", i18n::t(format!("== dry-run: 配置有效（{} 包）==", cfg.rules.pkgs().len()).as_str(),
            format!("== dry-run: config valid ({} packages) ==", cfg.rules.pkgs().len()).as_str()));
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
    println!("{}", i18n::t(if can_rt { "RT 调度: 完整" } else { "RT 调度: 无（fifo/rr 将跳过）" },
            if can_rt { "RT scheduling: full" } else { "RT scheduling: none (fifo/rr will be skipped)" }));
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
            println!("{}", i18n::t("eBPF 事件源: 可用（fork/exec/exit tracepoints）",
                        "eBPF event source: available (fork/exec/exit tracepoints)"));
            Box::new(s)
        }
        Err(e) => {
            eprintln!("{}", i18n::t(format!("警告: eBPF 不可用（{e}）——降级为 /proc 轮询").as_str(),
                        format!("warning: eBPF unavailable ({e}) — falling back to /proc polling").as_str()));
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

    // P8：观测数据层——周期采样（2s）→ 窗口统计（迁移/分布）
    let mut sampler = Sampler::new(2);
    let mut snap_window = SnapshotWindow::new();
    let mut last_observe = now_secs();

    // P14：启动时应用 system 配置（CPU governor / IO 调度器）
    if let Ok(content) = fs::read_to_string(&config_file) {
        if let Ok(tuning) = threadctl_core::kdl_parser::parse_system(&content) {
            if let Some(gov) = &tuning.governor {
                match threadctl_core::tune::apply_governor(gov) {
                    Ok(n) => println!("{}", i18n::t(format!("CPU governor: {gov}（已应用 {n} 个 CPU）").as_str(),
                        format!("CPU governor: {gov} (applied {n} CPUs)").as_str())),
                    Err(e) => eprintln!("{}", i18n::t(format!("警告: governor 应用失败: {e}").as_str(),
                        format!("warning: governor apply failed: {e}").as_str())),
                }
            }
            if let Some(io) = &tuning.io_scheduler {
                match threadctl_core::tune::apply_io_scheduler(io) {
                    Ok(n) => println!("{}", i18n::t(format!("IO 调度器: {io}（已应用 {n} 个设备）").as_str(),
                        format!("IO scheduler: {io} (applied {n} devices)").as_str())),
                    Err(e) => eprintln!("{}", i18n::t(format!("警告: IO 调度器应用失败: {e}").as_str(),
                        format!("warning: IO scheduler apply failed: {e}").as_str())),
                }
            }
        }
    }

    // P7.3 (C1)：IPC 控制面（Unix socket + mpsc 回主循环）
    install_signal_handlers();
    // 部署修复：run/ 等父目录可能不存在（新目录部署）——自动创建，
    // 否则 IPC socket 与 pid-file 都绑定失败
    if let Some(parent) = std::path::Path::new(&cfg.daemon.ipc_socket).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Some(parent) = std::path::Path::new(&cfg.daemon.pid_file).parent() {
        let _ = fs::create_dir_all(parent);
    }
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
        let (pressure, thermal, freq) = match last_sys {
            Some(s) => (s.memory_pressure, s.thermal_pressure, s.freq_throttle),
            None => (PressureLevel::Normal, 0.0, 1.0),
        };
        RelockContext {
            pressure,
            thermal_pressure: thermal,
            freq_throttle: freq,
            audit_failure_rate: audit::summary_windowed(60).failure_rate(),
        }
    };

    println!("{}", i18n::t(format!("threadctl-rs v{} 已启动（事件管道就绪）", env!("CARGO_PKG_VERSION")).as_str(),
            format!("threadctl-rs v{} started (event pipeline ready)", env!("CARGO_PKG_VERSION")).as_str()));

    loop {
        if SHUTDOWN.load(AtomicOrdering::Relaxed) {
            println!("{}", i18n::t("收到停止信号，正常退出", "shutdown signal received, exiting"));
            break;
        }
        let now = now_secs();
        let cfg = store.current();

        // ── P7.3 (C1)：IPC 请求处理（status/dump/reload/apply）──
        while let Ok((req, reply_tx)) = ipc_rx.try_recv() {
            let resp = handle_ipc(&req, &mut lock_tracker(&tracker), &cfg, &topo, &backend, &store, source.as_mut(), &decision_engine, &build_relock_ctx(&last_sys), &snap_window);
            let _ = reply_tx.send(resp);
        }

        // ── 配置变更：重扫白名单 + 全量刷新（CLAUDE BUG-M1：与 IPC reload 共用 do_reload）──
        while let Ok(version) = reload_rx.try_recv() {
            debug_log!("store", "hot-reload: version {version}");
            let n = do_reload(&store, source.as_mut(), &mut lock_tracker(&tracker), &topo, &backend, &decision_engine, &build_relock_ctx(&last_sys));
            debug_log!("engine", "config change rescan done: applied {n} threads");
        }

        // ── P7.2 B1/D3：覆盖采样 → 自适应周期 + 即时 relock（对抗 AMS 覆盖）──
        if cfg.engine.lock_interval > 0 && now - last_coverage_sample >= SAMPLE_INTERVAL_SECS as i64 {
            last_coverage_sample = now;
            let ratio = sample_coverage(&lock_tracker(&tracker), BASE_CPUSET);
            let before = adaptive.interval_secs();
            adaptive.observe_ratio(ratio);
            let after = adaptive.interval_secs();
            if after != before {
                debug_log!("relock", "adaptive: {}s -> {}s (coverage {:.0}%)", before, after, ratio * 100.0);
            }
            // D3：检测到覆盖 → 不等周期即时 relock（guard 1s 冷却防风暴；
            // 与周期 relock 共享 guard，防 AMS↔threadctl 震荡）
            if ratio > 0.0 && relock_guard.try_lock(Instant::now()) {
                let n = engine::relock_all(&mut lock_tracker(&tracker), &cfg, &topo, now, &build_relock_ctx(&last_sys), &decision_engine, &backend);
                last_lock = now;
                if n > 0 {
                    debug_log!("relock", "d3 immediate: re-applied {n} threads");
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
                        debug_log!("relock", "periodic: re-applied {n} threads");
                    }
                }
            }
        }

        // ── P8：线程观测采样（5s，性能审查：2s 全量读 600+ /proc 文件是
        // CPU 2.8% 均值的持续大头；5s 后窗口 150×5=12.5 分钟仍足够）──
        if now - last_observe >= 5 {
            last_observe = now;
            let snaps = sampler.sample(&lock_tracker(&tracker));
            snap_window.push_batch(snaps);
        }

        // ── 死进程清理 ──
        let dead_interval = cfg.engine.dead_cleanup_interval;
        if now - last_cleanup >= dead_interval.max(1) as i64 {
            let removed = engine::cleanup_dead(&mut lock_tracker(&tracker));
            last_cleanup = now;
            if removed > 0 {
                debug_log!("engine", "dead process cleanup: {removed}");
            }
            // 审查修复：快照窗口同步清理已退出线程（stats 只增不减会泄漏；
            // 线程退出但进程存活时不触发 removed>0，故每次 cleanup 都同步）
            let mut alive = std::collections::HashSet::new();
            {
                let t = lock_tracker(&tracker);
                for pid in t.pids() {
                    if let Some(st) = t.get(pid) {
                        alive.extend(st.applied_tids.iter().copied());
                    }
                }
            }
            snap_window.retain(&alive);
        }

        // ── 事件轮询 ──
        let poll_interval = scan_interval.max(cfg.engine.scan_interval);
        let deadline = Instant::now() + Duration::from_secs(poll_interval);
        let events = source.poll(deadline);
        // ── CPU 保护（性能审查发现）：事件源 poll 是立即返回的（eBPF drain
        // 不阻塞，deadline 被忽略）→ 主循环无等待会高速空转（实测 38% CPU）。
        // 睡到 deadline 剩余（上限 100ms——fork 应用延迟 ≤100ms 仍 near-real-time，
        // 空转频率 10Hz，CPU <1%）。
        let remain = deadline.saturating_duration_since(Instant::now());
        if !remain.is_zero() {
            thread::sleep(remain.min(Duration::from_millis(100)));
        }
        if !events.is_empty() {
            let n = engine::handle_events(&mut lock_tracker(&tracker), &events, &cfg, &topo, now, &backend);
            if n > 0 {
                debug_log!("engine", "events: {} , applied {} threads (pkg-count={})", events.len(), n, count_pkgs(&events));
            }
        }

        // ── M4 接入：SystemContext 自适应采样（压力异常时打印）──
        if sys_ctx_poller.should_sample() {
            let ctx = SystemContext::sample();
            if ctx.memory_pressure != threadctl_core::system_context::PressureLevel::Normal {
                debug_log!("context", "SystemContext: {}", ctx.summary());
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
            debug_log!("audit", "{}", audit::summary_string());
            let rs = engine::relock_stats();
            debug_log!(
                "relock",
                "decisions: allow={} skip={} degrade={}",
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
