//! SystemContext — system-state awareness layer (ChatGPT review item #5).
//!
//! Adaptive polling frequency:
//!   Normal    → 10s (idle, low overhead)
//!   Moderate  → 3s (memory/thermal pressure, denser sampling)
//!   Critical  → 1s (emergency, intensive sampling)

use std::fs;
use std::time::{Duration, Instant};

/// 内存压力等级（源自 /proc/pressure/memory some avg10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    Normal,    // avg10 < 10
    Moderate,  // avg10 10-60
    Critical,  // avg10 > 60
}

/// 单路温度传感器快照。
#[derive(Debug, Clone)]
pub struct ThermalZone {
    /// 类型标识（如 "cpu-thermal", "gpu-thermal", "skin-thermal"）
    pub zone_type: String,
    pub temperature_mc: i64,  // millicelsius
}

/// 系统状态快照。
#[derive(Debug, Clone)]
pub struct SystemContext {
    pub memory_pressure: PressureLevel,
    pub memory_avg10: f64,
    pub thermal_zones: Vec<ThermalZone>,
    /// 冷却设备使用率 0.0~1.0（Claude 审查 Bug 4：由 sample() 计算并缓存，
    /// 与 thermal_zones 时间同步；决策用快照而非独立实时读取）。
    pub thermal_pressure: f64,
    pub battery_pct: Option<u8>,
    pub is_charging: bool,
    /// 建议的下次采集间隔。
    pub next_poll_interval: Duration,
}

impl SystemContext {
    /// 采集当前状态。
    pub fn sample() -> Self {
        let mem = Self::read_memory_pressure();
        let thermal = Self::read_thermal_zones();
        let (battery, charging) = Self::read_battery();
        // Claude 审查 Bug 4：sample() 内一次计算并缓存，决策与日志用同一快照
        let thermal_pressure = Self::thermal_pressure();

        let next = match mem.0 {
            PressureLevel::Normal => Duration::from_secs(10),
            PressureLevel::Moderate => Duration::from_secs(3),
            PressureLevel::Critical => Duration::from_secs(1),
        };

        Self {
            memory_pressure: mem.0,
            memory_avg10: mem.1,
            thermal_zones: thermal,
            thermal_pressure,
            battery_pct: battery,
            is_charging: charging,
            next_poll_interval: next,
        }
    }

    /// M7 修复：冷却设备使用率（0.0~1.0），替代硬编码温度阈值。
    /// 读 /sys/class/thermal/cooling_device*/cur_state / max_state 取平均。
    pub fn thermal_pressure() -> f64 {
        let mut total = 0.0;
        let mut count = 0u32;
        if let Ok(dir) = fs::read_dir("/sys/class/thermal") {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("cooling_device") {
                    continue;
                }
                let base = format!("/sys/class/thermal/{name}");
                let cur = fs::read_to_string(format!("{base}/cur_state"))
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok());
                let max = fs::read_to_string(format!("{base}/max_state"))
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok());
                if let (Some(c), Some(m)) = (cur, max) {
                    if m > 0.0 {
                        total += c / m;
                        count += 1;
                    }
                }
            }
        }
        if count == 0 {
            0.0
        } else {
            total / count as f64
        }
    }

    fn read_memory_pressure() -> (PressureLevel, f64) {
        let content = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
        // Android 专项审查 🟡J：PSI 不可用时提示一次（静默降级 Normal 会误导诊断）
        use std::sync::Once;
        static WARNED: Once = Once::new();
        if content.is_empty() {
            WARNED.call_once(|| {
                eprintln!("system context: /proc/pressure/memory unavailable (kernel without PSI), memory-pressure sensing disabled");
            });
            return (PressureLevel::Normal, 0.0);
        }
        // 格式: "some avg10=5.23 avg60=3.10 avg300=1.50 total=123456"
        for field in content.split_whitespace() {
            if let Some(v) = field.strip_prefix("avg10=") {
                if let Ok(val) = v.parse::<f64>() {
                    let level = if val > 60.0 {
                        PressureLevel::Critical
                    } else if val > 10.0 {
                        PressureLevel::Moderate
                    } else {
                        PressureLevel::Normal
                    };
                    return (level, val);
                }
            }
        }
        (PressureLevel::Normal, 0.0)
    }

    fn read_thermal_zones() -> Vec<ThermalZone> {
        let mut zones = Vec::new();
        let Ok(dir) = fs::read_dir("/sys/class/thermal") else {
            return zones;
        };
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("thermal_zone") {
                continue;
            }
            let base = format!("/sys/class/thermal/{name}");
            let zone_type = fs::read_to_string(format!("{base}/type"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if zone_type.is_empty() {
                continue;
            }
            let temp = fs::read_to_string(format!("{base}/temp"))
                .unwrap_or_default()
                .trim()
                .parse::<i64>()
                .unwrap_or(0);
            zones.push(ThermalZone { zone_type, temperature_mc: temp });
        }
        // 按名称排序：cpu → gpu → skin → battery
        zones.sort_by(|a, b| a.zone_type.cmp(&b.zone_type));
        zones
    }

    fn read_battery() -> (Option<u8>, bool) {
        // 尝试多个常见路径
        for base in &["/sys/class/power_supply/battery", "/sys/class/power_supply/BAT0"] {
            if let Ok(pct) = fs::read_to_string(format!("{base}/capacity")) {
                let pct = pct.trim().parse::<u8>().ok();
                let status = fs::read_to_string(format!("{base}/status"))
                    .unwrap_or_default();
                let charging = status.trim() == "Charging";
                return (pct, charging);
            }
        }
        (None, false)
    }

    /// 给调试/日志用的简短摘要（设备日志降噪：只保留关键传感器，
    /// 过滤 -273°C 等未连接传感器的无效值）。
    pub fn summary(&self) -> String {
        // 有效传感器（> -100°C，滤除未连接/无效节点）
        let valid: Vec<&ThermalZone> = self
            .thermal_zones
            .iter()
            .filter(|z| z.temperature_mc > -100_000)
            .collect();

        let max_of = |prefix: &str| -> String {
            valid
                .iter()
                .filter(|z| z.zone_type.starts_with(prefix))
                .map(|z| z.temperature_mc)
                .max()
                .map(|v| format!("{:.1}", v as f64 / 1000.0))
                .unwrap_or_else(|| "N/A".into())
        };

        format!(
            "mem_pressure={:?}(avg10={:.1}) cpu_max={}°C gpu_max={}°C ddr_max={}°C battery={}%{} next_poll={}s zones={}",
            self.memory_pressure,
            self.memory_avg10,
            max_of("cpu"),
            max_of("gpu"),
            max_of("ddr"),
            self.battery_pct.unwrap_or(0),
            if self.is_charging { "⚡" } else { "" },
            self.next_poll_interval.as_secs(),
            valid.len(),
        )
    }
}

/// 自适应轮询器：根据上次 context 决定下次采样间隔。
pub struct AdaptivePoller {
    last_sample: Instant,
    next_interval: Duration,
}

impl AdaptivePoller {
    pub fn new() -> Self {
        Self {
            last_sample: Instant::now() - Duration::from_secs(3600),
            next_interval: Duration::from_secs(10),
        }
    }

    /// 是否到了下次采样时间。
    pub fn should_sample(&self) -> bool {
        self.last_sample.elapsed() >= self.next_interval
    }

    /// 采样后更新间隔。
    pub fn sampled(&mut self, ctx: &SystemContext) {
        self.last_sample = Instant::now();
        self.next_interval = ctx.next_poll_interval;
    }

    /// 阻塞直到下次采样时间或 deadline。
    pub fn wait_until_next(&self, deadline: Instant) {
        let next = self.last_sample + self.next_interval;
        let now = Instant::now();
        if next > now {
            let sleep = (next - now).min(
                if deadline > now { deadline - now } else { Duration::ZERO }
            );
            if sleep > Duration::ZERO {
                std::thread::sleep(sleep);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_poll_intervals() {
        let mut poller = AdaptivePoller::new();
        assert!(poller.should_sample()); // 首次总是采样
        let ctx = SystemContext {
            memory_pressure: PressureLevel::Normal,
            memory_avg10: 5.0,
            thermal_zones: vec![],
            thermal_pressure: 0.0,
            battery_pct: Some(80),
            is_charging: false,
            next_poll_interval: Duration::from_secs(10),
        };
        poller.sampled(&ctx);
        assert!(!poller.should_sample()); // 刚刚采过
    }

    #[test]
    fn critical_pressure_fast_poll() {
        let ctx = SystemContext {
            memory_pressure: PressureLevel::Critical,
            memory_avg10: 80.0,
            thermal_zones: vec![],
            thermal_pressure: 0.0,
            battery_pct: None,
            is_charging: false,
            next_poll_interval: Duration::from_secs(1), // Critical → 1s
        };
        assert_eq!(ctx.next_poll_interval, Duration::from_secs(1));
    }
}
