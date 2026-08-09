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
/// 兼容两种分隔：配置用逗号（"0-3,6-7"），sysfs related_cpus/affected_cpus
/// 用空格（"0 1 2"）——P6.3 M2 DVFS 域探测依赖此兼容。
pub fn parse_cpu_ranges(spec: &str, present: Option<&CpuSet>) -> CpuSet {
    let mut set = CpuSet::new();
    if spec.is_empty() {
        return set;
    }
    for part in spec.split(|c| c == ',' || c == ' ' || c == '\t') {
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
#[derive(Clone, Debug, PartialEq)]
pub enum CpuClusterKind {
    Prime,   // 最高容量单核
    Big,     // 性能核
    /// P6.3 M1：中核档（4 组容量 SoC，如 SM8650 的 A720@3.0GHz）
    Mid,
    Little,  // 能效核
    Unknown, // 同构/无法识别
}

impl CpuClusterKind {
    /// 配置名（CLAUDE NEW-L2）：显式名称方法，避免 config.rs 依赖
    /// Debug 派生的格式（`{:?}` 实现若变体改名/手写会静默失效）。
    pub fn config_name(&self) -> &'static str {
        match self {
            Self::Prime => "prime",
            Self::Big => "big",
            Self::Mid => "mid",
            Self::Little => "little",
            Self::Unknown => "unknown",
        }
    }
}

/// 全大核判定阈值（CLAUDE P6.3-规划-1：双条件"且"，防新一代高效小核误判）：
/// 2 组容量时，容量比 < 2.0 **且** 最低组容量 > 300 → Big+Prime（全大核）；
/// 否则 Little+Big（传统 big.LITTLE）。
const ALL_BIG_MAX_RATIO: f64 = 2.0;
const ALL_BIG_MIN_CAPACITY: u32 = 300;

/// 按容量组分类（纯函数，单测友好）——detect_clusters 读 sysfs 后调用。
///
/// 输入：容量组升序（每组 (capacity, cpu 列表)）
/// 输出：每组对应的 cluster kind
///
/// ```text
/// N=1 → Unknown
/// N=2 → 全大核判定（容量比 < 2.0 且 最低组 > 300）→ Big+Prime；否则 Little+Big
/// N=3 → Little / Big / Prime（SM8475/SM8550 等）
/// N=4 → Little / Mid / Big / Prime（SM8650）
/// N≥5 → Little / Mid / Big×k / Prime（中间组：第一中间=Mid，其余=Big）
/// ```
pub(crate) fn classify_clusters(groups: &[(u32, Vec<usize>)]) -> Vec<CpuClusterKind> {
    let n = groups.len();
    match n {
        0 => Vec::new(),
        1 => vec![CpuClusterKind::Unknown],
        2 => {
            let min_cap = groups[0].0;
            let max_cap = groups[1].0;
            let ratio = if min_cap > 0 {
                max_cap as f64 / min_cap as f64
            } else {
                f64::MAX
            };
            if ratio < ALL_BIG_MAX_RATIO && min_cap > ALL_BIG_MIN_CAPACITY {
                // 全大核（SM8750/SM8850：Oryon 2+6，无小核）
                vec![CpuClusterKind::Big, CpuClusterKind::Prime]
            } else {
                // 传统 big.LITTLE
                vec![CpuClusterKind::Little, CpuClusterKind::Big]
            }
        }
        _ => {
            let mut kinds = Vec::with_capacity(n);
            for i in 0..n {
                let kind = if i == 0 {
                    CpuClusterKind::Little
                } else if i == n - 1 {
                    CpuClusterKind::Prime
                } else if n >= 4 && i == 1 {
                    CpuClusterKind::Mid
                } else {
                    CpuClusterKind::Big
                };
                kinds.push(kind);
            }
            kinds
        }
    }
}

/// 自动检测到的 CPU 集群。
#[derive(Clone, Debug)]
pub struct CpuCluster {
    pub kind: CpuClusterKind,
    pub cpus: CpuSet,
    pub range_str: String,
    pub capacity: u32,
}

/// DVFS 域探测（P6.3 M2）：枚举 `/sys/devices/system/cpu/cpufreq/policyN/`，
/// 读 `related_cpus`（完整频率域，含离线 CPU，优先），缺失时 fallback
/// `affected_cpus`（在线子集）——CLAUDE P6.3-规划-3。
///
/// 返回按 policy 序号升序的域成员集合（如 SM8550: [{0-2},{3-6},{7}]）。
/// 无 cpufreq 接口时返回空（cpuset 通道仍可用）。
pub fn detect_dvfs_domains() -> Vec<CpuSet> {
    let mut domains: Vec<(usize, CpuSet)> = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/cpufreq") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(policy) = name.strip_prefix("policy") else {
            continue;
        };
        let Ok(pol_idx) = policy.parse::<usize>() else {
            continue;
        };
        let base = format!("/sys/devices/system/cpu/cpufreq/{name}");
        // related_cpus 优先（完整域），affected_cpus fallback（在线子集）
        let raw = fs::read_to_string(format!("{base}/related_cpus"))
            .or_else(|_| fs::read_to_string(format!("{base}/affected_cpus")))
            .ok();
        let Some(raw) = raw else { continue };
        let set = parse_cpu_ranges(raw.trim(), None);
        if set.count() > 0 {
            domains.push((pol_idx, set));
        }
    }
    domains.sort_by_key(|(idx, _)| *idx);
    domains.into_iter().map(|(_, set)| set).collect()
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

    // P6.3 M1：分组逻辑提取为纯函数 classify_clusters（可单测），
    // 此处只负责把 sysfs 容量组转成 (capacity, cpus) 切片再调用。
    let groups: Vec<(u32, Vec<usize>)> = capacity_groups
        .iter()
        .map(|(&cap, cpus)| (cap, cpus.clone()))
        .collect();
    let kinds = classify_clusters(&groups);

    // BTreeMap 按容量升序，最后一项是最大容量
    let mut clusters: Vec<CpuCluster> = Vec::with_capacity(groups.len());
    for (idx, (&capacity, cpu_list)) in capacity_groups.iter().enumerate() {
        let kind = kinds[idx].clone();

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
    /// DVFS 域（cpufreq policy 分组；related_cpus 优先/affected_cpus fallback）。
    /// P6.3 M2：只读探测，供日志核对与未来绑核优化（同域同频）。
    pub dvfs_domains: Vec<CpuSet>,
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
            dvfs_domains: Vec::new(),
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

    // P6.3 M2：DVFS 域探测（cpufreq policyN/related_cpus，fallback affected_cpus）
    topo.dvfs_domains = detect_dvfs_domains();

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

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_name(k: &CpuClusterKind) -> &'static str {
        match k {
            CpuClusterKind::Prime => "prime",
            CpuClusterKind::Big => "big",
            CpuClusterKind::Mid => "mid",
            CpuClusterKind::Little => "little",
            CpuClusterKind::Unknown => "unknown",
        }
    }

    fn names(kinds: &[CpuClusterKind]) -> Vec<&'static str> {
        kinds.iter().map(kind_name).collect()
    }

    #[test]
    fn classify_three_groups_unchanged() {
        // SM8550/SM8475 回归：3 组 → Little/Big/Prime（现有语义不变）
        let groups = vec![
            (280, vec![0, 1, 2]),   // A510/A520 little
            (855, vec![3, 4, 5, 6]), // A715/A710 big
            (1024, vec![7]),        // X3/X2 prime
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "big", "prime"]);
    }

    #[test]
    fn classify_four_groups_sm8650() {
        // SM8650：4 组 → Little/Mid/Big/Prime（P6.3 M1 核心）
        let groups = vec![
            (240, vec![0, 1]),    // A520 little
            (560, vec![2, 3]),    // A720@3.0 mid
            (720, vec![4, 5, 6]), // A720@3.2 big
            (1024, vec![7]),      // X4 prime
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "mid", "big", "prime"]);
    }

    #[test]
    fn classify_two_groups_all_big_sm8750() {
        // SM8750/SM8850：2 组全大核（容量比 < 2.0 且 最低组 > 300）→ Big/Prime
        let groups = vec![
            (700, vec![0, 1, 2, 3, 4, 5]), // Oryon@3.53GHz
            (900, vec![6, 7]),             // Oryon@4.32GHz
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["big", "prime"]);
    }

    #[test]
    fn classify_two_groups_big_little() {
        // 传统 big.LITTLE：容量比 > 2.0 → Little/Big
        let groups = vec![
            (280, vec![0, 1, 2]),
            (855, vec![3, 4, 5, 6]),
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "big"]);
    }

    #[test]
    fn classify_two_groups_ratio_boundary() {
        // 边界：容量比恰好 2.0 → 不算全大核（< 是严格小于）
        let groups = vec![
            (400, vec![0, 1, 2, 3]),
            (800, vec![4, 5, 6, 7]),
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "big"]);

        // 边界：最低组恰好 300 → 不算全大核（> 是严格大于）
        let groups = vec![
            (300, vec![0, 1, 2, 3]),
            (500, vec![4, 5, 6, 7]),
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "big"]);

        // CLAUDE NEW-L1：AND 半条件测试——ratio ✓ 但 min_cap ✗ → 不算全大核
        let groups = vec![
            (250, vec![0, 1, 2, 3]),
            (400, vec![4, 5, 6, 7]),
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "big"]);
        // AND 半条件：min_cap ✓ 但 ratio ✗ → 不算全大核
        let groups = vec![
            (400, vec![0, 1, 2, 3]),
            (1000, vec![4, 5, 6, 7]),
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "big"]);
    }

    #[test]
    fn classify_five_groups_generic() {
        // N≥5：Little/Mid/Big×k/Prime（第一中间=Mid，其余=Big）
        let groups = vec![
            (200, vec![0, 1]),
            (400, vec![2]),
            (600, vec![3, 4]),
            (800, vec![5, 6]),
            (1024, vec![7]),
        ];
        assert_eq!(names(&classify_clusters(&groups)), vec!["little", "mid", "big", "big", "prime"]);
    }

    #[test]
    fn classify_one_group_unknown() {
        // 同构 SoC → Unknown
        let groups = vec![(800, vec![0, 1, 2, 3, 4, 5, 6, 7])];
        assert_eq!(names(&classify_clusters(&groups)), vec!["unknown"]);
    }

    #[test]
    fn cpu_set_parse_roundtrip() {
        // 范围字符串解析 → to_range_string 往返
        let s = "0-3,6-7";
        let set = parse_cpu_ranges(s, None);
        assert_eq!(set.to_range_string(), s);
        assert_eq!(set.count(), 6);
    }

    #[test]
    fn parse_sysfs_space_separated() {
        // P6.3 M2：sysfs related_cpus/affected_cpus 是空格分隔（"0 1 2"），
        // 与配置的逗号格式兼容解析
        let set = parse_cpu_ranges("0 1 2", None);
        assert_eq!(set.to_range_string(), "0-2");
        assert_eq!(set.count(), 3);

        let set = parse_cpu_ranges("3 4 5 6", None);
        assert_eq!(set.to_range_string(), "3-6");

        let set = parse_cpu_ranges("0 1,4-5", None);
        assert_eq!(set.to_range_string(), "0-1,4-5");
    }
}
