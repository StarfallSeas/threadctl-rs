//! CPU bitmap & topology — migrated from 既有实现's cpuset.rs with full semantics preserved.

use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::io::RawFd;

pub const BASE_CPUSET: &str = "/dev/cpuset/threadctl";
pub const MAX_PKG_LEN: usize = 128;
pub const MAX_THREAD_LEN: usize = 32;
pub const CPU_SETSIZE: usize = 1024;
pub const CPU_WORD_BITS: usize = 64;
pub const CPU_WORDS: usize = CPU_SETSIZE / CPU_WORD_BITS;

/// 与 Linux `cpu_set_t` 二进制布局一致的位图。
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CpuSet {
    pub bits: [u64; CPU_WORDS],
}

impl CpuSet {
    pub const fn new() -> Self {
        Self { bits: [0u64; CPU_WORDS] }
    }

    #[inline]
    pub fn set(&mut self, cpu: usize) {
        if cpu < CPU_SETSIZE {
            self.bits[cpu / CPU_WORD_BITS] |= 1u64 << (cpu % CPU_WORD_BITS);
        }
    }

    #[inline]
    pub fn is_set(&self, cpu: usize) -> bool {
        cpu < CPU_SETSIZE
            && (self.bits[cpu / CPU_WORD_BITS] & (1u64 << (cpu % CPU_WORD_BITS))) != 0
    }

    pub fn count(&self) -> usize {
        self.bits.iter().map(|&w| w.count_ones() as usize).sum()
    }

    pub fn or(&mut self, other: &CpuSet) {
        for (d, &s) in self.bits.iter_mut().zip(other.bits.iter()) {
            *d |= s;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&w| w == 0)
    }

    /// 位与（交集）：`self = self & other`。
    pub fn and(&mut self, other: &CpuSet) {
        for (d, &s) in self.bits.iter_mut().zip(other.bits.iter()) {
            *d &= s;
        }
    }

    /// 读取 /proc/<tid>/status 的 `Cpus_allowed_list`——内核 cgroup 实际允许的 CPU 集合。
    /// 返回 None 表示文件不可读。
    pub fn read_allowed_mask(tid: i32) -> Option<CpuSet> {
        let content = fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
        // Cpus_allowed_list 是范围字符串（如 "0-6"、"0-3,6-7"），比 hex 掩码可读且无位宽限制
        for line in content.lines() {
            if let Some(v) = line.strip_prefix("Cpus_allowed_list:") {
                let list = v.trim();
                if list.is_empty() {
                    return None;
                }
                return Some(parse_cpu_ranges(list, None));
            }
        }
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.bits.iter().enumerate().flat_map(|(wi, &w)| {
            let base = wi * CPU_WORD_BITS;
            (0..CPU_WORD_BITS)
                .filter(move |&b| (w >> b) & 1 == 1)
                .map(move |b| base + b)
        })
    }

    /// 紧凑范围字符串："0-3,6-7"。
    pub fn to_range_string(&self) -> String {
        let mut out = String::new();
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        let mut first = true;

        for (wi, &word) in self.bits.iter().enumerate() {
            if word == 0 {
                if start.is_some() {
                    push_range(&mut out, start, end, &mut first);
                    start = None;
                    end = None;
                }
                continue;
            }
            let base = wi * CPU_WORD_BITS;
            for bit in 0..CPU_WORD_BITS {
                if word & (1u64 << bit) != 0 {
                    let cpu = base + bit;
                    match start {
                        None => {
                            start = Some(cpu);
                            end = Some(cpu);
                        }
                        Some(_) if cpu == end.unwrap() + 1 => end = Some(cpu),
                        _ => {
                            push_range(&mut out, start, end, &mut first);
                            start = Some(cpu);
                            end = Some(cpu);
                        }
                    }
                }
            }
        }
        push_range(&mut out, start, end, &mut first);
        out
    }

    pub fn get_affinity(tid: i32) -> Option<CpuSet> {
        let mut curr = CpuSet::new();
        let ret = unsafe {
            libc::sched_getaffinity(
                tid,
                std::mem::size_of::<CpuSet>(),
                &mut curr as *mut CpuSet as *mut libc::cpu_set_t,
            )
        };
        if ret == -1 {
            None
        } else {
            Some(curr)
        }
    }

    pub fn set_affinity(&self, tid: i32) -> io::Result<()> {
        let ret = unsafe {
            libc::sched_setaffinity(
                tid,
                std::mem::size_of::<CpuSet>(),
                self as *const CpuSet as *const libc::cpu_set_t,
            )
        };
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn push_range(s: &mut String, start: Option<usize>, end: Option<usize>, first: &mut bool) {
    if let (Some(lo), Some(hi)) = (start, end) {
        if !*first {
            s.push(',');
        }
        if lo == hi {
            let _ = write!(s, "{lo}");
        } else {
            let _ = write!(s, "{lo}-{hi}");
        }
        *first = false;
    }
}

/// 解析 CPU 范围字符串，可选 present 过滤。
pub fn parse_cpu_ranges(spec: &str, present: Option<&CpuSet>) -> CpuSet {
    let mut set = CpuSet::new();
    if spec.is_empty() {
        return set;
    }
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (lo, hi) = if let Some(pos) = part.find('-') {
            let a: usize = part[..pos].parse().unwrap_or(usize::MAX);
            let b: usize = part[pos + 1..].parse().unwrap_or(a);
            if a == usize::MAX {
                continue;
            }
            if a <= b {
                (a, b)
            } else {
                (b, a)
            }
        } else {
            let v: usize = part.parse().unwrap_or(usize::MAX);
            if v == usize::MAX {
                continue;
            }
            (v, v)
        };
        for i in lo..=hi.min(CPU_SETSIZE - 1) {
            if let Some(p) = present {
                if !p.is_set(i) {
                    continue;
                }
            }
            set.set(i);
        }
    }
    set
}

/// 创建 cpuset 子目录并写 cpus/mems；EEXIST 视为成功。
pub fn create_cpuset_dir(path: &str, cpus: &str, mems: &str) -> bool {
    let c_path = CString::new(path).expect("受控输入无 NUL");
    let ret = unsafe { libc::mkdir(c_path.as_ptr(), 0o755) };
    if ret != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
        return false;
    }
    unsafe {
        libc::chmod(c_path.as_ptr(), 0o755);
        libc::chown(c_path.as_ptr(), 0, 0);
    }
    fs::write(format!("{path}/cpus"), cpus).is_ok() && fs::write(format!("{path}/mems"), mems).is_ok()
}

/// 删除 cpuset 子目录（仅当目录为空时成功）。
pub fn remove_cpuset_dir(path: &str) -> bool {
    // L3 修复：路径来自受控派生（BASE_CPUSET + 数字范围），无 NUL
    let c_path = CString::new(path).expect("受控输入无 NUL");
    unsafe { libc::rmdir(c_path.as_ptr()) == 0 }
}

// ── CPU 集群检测 ──────────────────────────────────────────────

/// 集群类别。
#[derive(Clone, Debug)]
pub enum CpuClusterKind {
    Prime,   // 最高容量单核
    Big,     // 性能核
    Little,  // 能效核
    Unknown, // 同构/无法识别
}

/// 自动检测到的 CPU 集群。
#[derive(Clone, Debug)]
pub struct CpuCluster {
    pub kind: CpuClusterKind,
    pub cpus: CpuSet,
    pub range_str: String,
    pub capacity: u32,
}

/// 读取 `/sys/devices/system/cpu/cpuN/cpu_capacity`，按容量分组自动识别集群。
pub fn detect_clusters() -> Vec<CpuCluster> {
    use std::collections::BTreeMap;

    let mut capacity_groups: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for entry in fs::read_dir("/sys/devices/system/cpu").into_iter().flatten().flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") {
            continue;
        }
        let cpu_str = &name[3..];
        let Ok(cpu) = cpu_str.parse::<usize>() else { continue };
        let capacity_path = format!("/sys/devices/system/cpu/{name}/cpu_capacity");
        if let Ok(raw) = fs::read_to_string(&capacity_path) {
            if let Ok(cap) = raw.trim().parse::<u32>() {
                capacity_groups.entry(cap).or_default().push(cpu);
            }
        }
    }

    if capacity_groups.is_empty() {
        // M1 修复：回退用实际在线 CPU，而非 0..CPU_SETSIZE 全部置位
        let mut cpus = CpuSet::new();
        if let Ok(content) = fs::read_to_string("/sys/devices/system/cpu/online") {
            cpus = parse_cpu_ranges(content.trim(), None);
        }
        return vec![CpuCluster {
            kind: CpuClusterKind::Unknown,
            range_str: cpus.to_range_string(),
            cpus,
            capacity: 0,
        }];
    }

    let num_groups = capacity_groups.len();
    let mut clusters: Vec<CpuCluster> = Vec::with_capacity(num_groups);

    // BTreeMap 按容量升序，最后一项是最大容量
    for (idx, (&capacity, cpu_list)) in capacity_groups.iter().enumerate() {
        let kind = match (num_groups, idx) {
            (1, _) => CpuClusterKind::Unknown,
            (2, 0) => CpuClusterKind::Little,
            (2, 1) => CpuClusterKind::Big,
            (_n, i) if i == _n - 1 => CpuClusterKind::Prime,  // 最高容量 → prime
            (_n, i) if i == 0 => CpuClusterKind::Little,
            _ => CpuClusterKind::Big, // 中间组
        };

        let mut cpus = CpuSet::new();
        for &cpu in cpu_list {
            cpus.set(cpu);
        }
        clusters.push(CpuCluster {
            kind,
            range_str: cpus.to_range_string(),
            cpus,
            capacity,
        });
    }

    clusters
}

// ── CpuTopology ────────────────────────────────────────────────

#[derive(Clone)]
pub struct CpuTopology {
    pub present_cpus: CpuSet,
    pub present_str: String,
    pub online_cpus: CpuSet,
    pub online_str: String,
    pub mems_str: String,
    pub cpuset_enabled: bool,
    /// 进程生命周期 fd，不主动关闭。
    pub base_cpuset_fd: RawFd,
    /// CPU 集群（基于 cpu_capacity 自动检测），同类容量归一组，按容量升序排列。
    pub clusters: Vec<CpuCluster>,
}

impl Default for CpuTopology {
    fn default() -> Self {
        Self {
            present_cpus: CpuSet::new(),
            present_str: String::new(),
            online_cpus: CpuSet::new(),
            online_str: String::new(),
            mems_str: String::new(),
            cpuset_enabled: false,
            base_cpuset_fd: -1,
            clusters: Vec::new(),
        }
    }
}

pub fn init_cpu_topo() -> CpuTopology {
    let mut topo = CpuTopology::default();
    if let Ok(c) = fs::read_to_string("/sys/devices/system/cpu/present") {
        topo.present_str = c.trim().to_string();
    }
    topo.present_cpus = parse_cpu_ranges(&topo.present_str, None);

    // 在线 CPU（用于 setaffinity 前过滤，避免 EINVAL）
    if let Ok(c) = fs::read_to_string("/sys/devices/system/cpu/online") {
        topo.online_str = c.trim().to_string();
    }
    topo.online_cpus = parse_cpu_ranges(&topo.online_str, None);

    // CPU 集群检测（big.LITTLE 自动识别）
    topo.clusters = detect_clusters();

    let root = CString::new("/dev/cpuset").expect("常量字符串无 NUL");
    if unsafe { libc::access(root.as_ptr(), libc::F_OK) } != 0 {
        return topo;
    }

    if create_cpuset_dir(BASE_CPUSET, &topo.present_str, "0") {
        let bp = CString::new(BASE_CPUSET).expect("常量字符串无 NUL");
        topo.base_cpuset_fd =
            unsafe { libc::open(bp.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
        topo.cpuset_enabled = topo.base_cpuset_fd != -1;
    }

    if let Ok(m) = fs::read_to_string(format!("{BASE_CPUSET}/mems")) {
        topo.mems_str = m.trim().to_string();
    } else {
        topo.mems_str = "0".into();
    }
    topo
}
