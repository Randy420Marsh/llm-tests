//! Hybrid-CPU topology: which logical CPUs are performance (P) or efficiency (E) cores.
//!
//! Works on Windows and Linux by pinning a short-lived thread to each logical CPU and
//! reading CPUID leaf 0x1A (core type), which Intel hybrid parts report per core.

/// Core type of one logical CPU
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreKind {
    Performance,
    Efficiency,
    Other,
}

/// Pin the calling thread to one logical CPU. Returns false if the OS refused.
pub fn pin_current_thread(cpu: usize) -> bool {
    if cpu >= 64 {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
        unsafe { SetThreadAffinityMask(GetCurrentThread(), 1usize << cpu) != 0 }
    }
    #[cfg(target_os = "linux")]
    {
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(cpu, &mut set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

/// Map a CPUID.1A EAX value to a core kind (bits 31:24: 0x40 = Core/P, 0x20 = Atom/E)
pub fn kind_from_leaf_1a(eax: u32) -> CoreKind {
    match eax >> 24 {
        0x40 => CoreKind::Performance,
        0x20 => CoreKind::Efficiency,
        _ => CoreKind::Other,
    }
}

/// Core kinds for logical CPUs 0..n, or `None` when the CPU is not a hybrid part.
///
/// What the OS reports comes first, it needs no tricks: Windows gives every physical core an
/// "efficiency class" (on hybrid parts the P cores have the higher one), Linux lists the P and E
/// CPUs under /sys/devices/cpu_core and cpu_atom. Only when neither is available is CPUID leaf 0x1A
/// read on each CPU, and then only after the thread has really moved there: reading it before the
/// move reports the CPU the thread started on, and a new thread usually starts on a P core, which
/// made every core look like a P core.
pub fn detect_core_kinds() -> Option<Vec<CoreKind>> {
    let n = num_cpus::get().min(64);
    os_core_kinds(n).or_else(|| cpuid_core_kinds(n)).filter(|k| is_hybrid(k))
}

fn is_hybrid(k: &[CoreKind]) -> bool {
    k.contains(&CoreKind::Efficiency) && k.contains(&CoreKind::Performance)
}

/// Kinds from per-core efficiency classes: the highest class is P, every lower one E
pub fn kinds_from_classes(n: usize, cores: &[(u8, Vec<usize>)]) -> Option<Vec<CoreKind>> {
    let top = cores.iter().map(|c| c.0).max()?;
    if cores.iter().all(|c| c.0 == top) {
        return None;
    }
    let mut kinds = vec![CoreKind::Other; n];
    for (class, cpus) in cores {
        for &cpu in cpus.iter().filter(|&&c| c < n) {
            kinds[cpu] = if *class == top { CoreKind::Performance } else { CoreKind::Efficiency };
        }
    }
    kinds.iter().all(|&k| k != CoreKind::Other).then_some(kinds)
}

#[cfg(target_os = "windows")]
fn os_core_kinds(n: usize) -> Option<Vec<CoreKind>> {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    let mut cores: Vec<(u8, Vec<usize>)> = Vec::new();
    unsafe {
        let mut len = 0u32;
        let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        GetLogicalProcessorInformationEx(RelationProcessorCore, Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX), &mut len).ok()?;
        let mut off = 0usize;
        while off + 8 <= len as usize {
            let info = &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            if info.Relationship == RelationProcessorCore {
                let p = &info.Anonymous.Processor;
                if p.GroupCount >= 1 && p.GroupMask[0].Group == 0 {
                    let mask = p.GroupMask[0].Mask as u64;
                    cores.push((p.EfficiencyClass, (0..64).filter(|b| mask >> b & 1 == 1).collect()));
                }
            }
            if info.Size == 0 {
                break;
            }
            off += info.Size as usize;
        }
    }
    kinds_from_classes(n, &cores)
}

#[cfg(target_os = "linux")]
fn os_core_kinds(n: usize) -> Option<Vec<CoreKind>> {
    let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| crate::app_core::parse_cpu_list(s.trim()));
    let (p, e) = (read("/sys/devices/cpu_core/cpus")?, read("/sys/devices/cpu_atom/cpus")?);
    kinds_from_classes(n, &[(1, p), (0, e)])
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn os_core_kinds(_n: usize) -> Option<Vec<CoreKind>> {
    None
}

/// The logical CPU the calling thread runs on right now
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
fn current_cpu() -> Option<usize> {
    #[cfg(target_os = "windows")]
    unsafe {
        Some(windows::Win32::System::Threading::GetCurrentProcessorNumber() as usize)
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let c = libc::sched_getcpu();
        (c >= 0).then_some(c as usize)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

#[cfg(target_arch = "x86_64")]
fn cpuid_core_kinds(n: usize) -> Option<Vec<CoreKind>> {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    let max_leaf = __cpuid(0).eax;
    // Leaf 7 EDX bit 15 = hybrid part
    if max_leaf < 0x1A || __cpuid_count(7, 0).edx & (1 << 15) == 0 {
        return None;
    }
    let handles: Vec<_> = (0..n)
        .map(|cpu| {
            std::thread::spawn(move || {
                if !pin_current_thread(cpu) {
                    return None;
                }
                // wait until the scheduler has actually moved us (when the OS can tell us where we are)
                for _ in 0..1000 {
                    match current_cpu() {
                        Some(c) if c == cpu => break,
                        Some(_) => std::thread::yield_now(),
                        None => {
                            std::thread::yield_now();
                            break;
                        }
                    }
                }
                if current_cpu().is_some_and(|c| c != cpu) {
                    return None;
                }
                Some(kind_from_leaf_1a(__cpuid_count(0x1A, 0).eax))
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().ok().flatten()).collect()
}

#[cfg(not(target_arch = "x86_64"))]
fn cpuid_core_kinds(_n: usize) -> Option<Vec<CoreKind>> {
    None
}

/// Physical cores in logical-CPU order, each with its kind and logical CPUs. Places sensors that are
/// named per core ("CPU Core #3", "P-Core #2", "E-Core #7" in LibreHardwareMonitor) on logical CPU
/// numbers: on hybrid Intel parts the P and E cores are interleaved (e.g. P = 0, 1, 10-13, 22, 23).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoreMap {
    pub cores: Vec<(CoreKind, Vec<usize>)>,
}

impl CoreMap {
    pub fn detect() -> Self {
        Self::from_parts(crate::app_core::sibling_groups(), detect_core_kinds().as_deref())
    }

    /// `groups`: logical CPUs per physical core; `kinds`: kind per logical CPU (None = not hybrid)
    pub fn from_parts(mut groups: Vec<Vec<usize>>, kinds: Option<&[CoreKind]>) -> Self {
        groups.retain(|g| !g.is_empty());
        groups.iter_mut().for_each(|g| g.sort_unstable());
        groups.sort_by_key(|g| g[0]);
        let cores = groups
            .into_iter()
            .map(|g| (kinds.and_then(|k| k.get(g[0]).copied()).unwrap_or(CoreKind::Other), g))
            .collect();
        Self { cores }
    }

    /// Logical CPUs of the core a per-core sensor name refers to (None for any other name)
    pub fn logical_for(&self, name: &str) -> Option<Vec<usize>> {
        let lower = name.trim().to_lowercase();
        let (kind, rest) = if let Some(r) = lower.strip_prefix("p-core #") {
            (Some(CoreKind::Performance), r)
        } else if let Some(r) = lower.strip_prefix("e-core #") {
            (Some(CoreKind::Efficiency), r)
        } else if let Some(r) = lower.strip_prefix("cpu core #") {
            (None, r)
        } else {
            return None;
        };
        // "CPU Core #1 Distance to TjMax" and the like are not the core's temperature
        let idx = rest.trim().parse::<usize>().ok()?.checked_sub(1)?;
        let core = match kind {
            None => self.cores.get(idx),
            Some(k) => self.cores.iter().filter(|c| c.0 == k).nth(idx),
        };
        match core {
            Some(c) => Some(c.1.clone()),
            // unknown topology: "CPU Core #n" is core n-1, one thread per core
            None if kind.is_none() && self.cores.is_empty() => Some(vec![idx]),
            None => None,
        }
    }
}

/// [`detect_core_kinds`], detected once per process
pub fn cached_core_kinds() -> Option<&'static [CoreKind]> {
    static KINDS: std::sync::OnceLock<Option<Vec<CoreKind>>> = std::sync::OnceLock::new();
    KINDS.get_or_init(detect_core_kinds).as_deref()
}

/// "P" / "E" for a logical CPU of a hybrid part, "" otherwise
pub fn class_label(kinds: Option<&[CoreKind]>, cpu: usize) -> &'static str {
    match kinds.and_then(|k| k.get(cpu)) {
        Some(CoreKind::Performance) => "P",
        Some(CoreKind::Efficiency) => "E",
        _ => "",
    }
}

/// `(p_thread_count, e_thread_count)`; non-hybrid CPUs report `(logical, 0)`
pub fn hybrid_counts() -> (usize, usize) {
    match cached_core_kinds() {
        Some(k) => (
            k.iter().filter(|&&c| c == CoreKind::Performance).count(),
            k.iter().filter(|&&c| c == CoreKind::Efficiency).count(),
        ),
        None => (num_cpus::get(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_core_types() {
        assert_eq!(kind_from_leaf_1a(0x4000_0001), CoreKind::Performance);
        assert_eq!(kind_from_leaf_1a(0x2000_0001), CoreKind::Efficiency);
        assert_eq!(kind_from_leaf_1a(0), CoreKind::Other);
    }

    #[test]
    fn per_core_sensor_names_land_on_logical_cpus() {
        use CoreKind::{Efficiency as E, Performance as P};
        // Core Ultra 7 270K: P = 0, 1, 10-13, 22, 23; E = 2-9, 14-21; no Hyper-Threading
        let mut kinds = vec![E; 24];
        for p in [0, 1, 10, 11, 12, 13, 22, 23] {
            kinds[p] = P;
        }
        let map = CoreMap::from_parts((0..24).map(|c| vec![c]).collect(), Some(&kinds));
        assert_eq!(map.logical_for("P-Core #1"), Some(vec![0]));
        assert_eq!(map.logical_for("P-Core #3"), Some(vec![10]));
        assert_eq!(map.logical_for("P-Core #8"), Some(vec![23]));
        assert_eq!(map.logical_for("E-Core #1"), Some(vec![2]));
        assert_eq!(map.logical_for("E-Core #9"), Some(vec![14]));
        assert_eq!(map.logical_for("E-Core #17"), None);
        assert_eq!(map.logical_for("P-Core #1 Distance to TjMax"), None);
        assert_eq!(map.logical_for("CPU Package"), None);
        // Hyper-Threading: one physical core = two logical CPUs
        let ht = CoreMap::from_parts(vec![vec![0, 1], vec![2, 3]], None);
        assert_eq!(ht.logical_for("CPU Core #2"), Some(vec![2, 3]));
        assert_eq!(CoreMap::default().logical_for("CPU Core #4"), Some(vec![3]));
    }

    #[test]
    fn efficiency_classes_map_to_kinds() {
        use CoreKind::{Efficiency as E, Performance as P};
        // Arrow Lake: class 1 = P (0, 1, 10-13, 22, 23), class 0 = E
        let cores: Vec<(u8, Vec<usize>)> = (0..24).map(|c| (u8::from([0, 1, 10, 11, 12, 13, 22, 23].contains(&c)), vec![c])).collect();
        let k = kinds_from_classes(24, &cores).unwrap();
        assert_eq!((k[0], k[2], k[10], k[21], k[23]), (P, E, P, E, P));
        // every core in the same class: not a hybrid CPU
        assert_eq!(kinds_from_classes(4, &[(0, vec![0, 1]), (0, vec![2, 3])]), None);
        // Hyper-Threading: both threads of a P core are P
        let k = kinds_from_classes(6, &[(1, vec![0, 1]), (1, vec![2, 3]), (0, vec![4]), (0, vec![5])]).unwrap();
        assert_eq!(k, vec![P, P, P, P, E, E]);
        // a CPU the OS did not describe makes the answer unusable
        assert_eq!(kinds_from_classes(3, &[(1, vec![0]), (0, vec![1])]), None);
    }

    #[test]
    fn detection_is_consistent() {
        // Must not panic or hang on any machine; counts must add up when hybrid
        let (p, e) = hybrid_counts();
        assert!(p > 0);
        if e > 0 {
            assert!(p + e <= num_cpus::get());
        }
    }

    #[test]
    fn pinning_cpu_zero_works_or_is_refused() {
        // Containers may forbid it; either answer is fine, but it must not crash
        let _ = std::thread::spawn(|| pin_current_thread(0)).join().unwrap();
    }
}
