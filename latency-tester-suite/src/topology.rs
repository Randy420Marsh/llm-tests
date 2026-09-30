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

/// Core kinds for logical CPUs 0..n, or `None` when the CPU is not a hybrid part
/// (or the OS would not let us pin threads).
#[cfg(target_arch = "x86_64")]
pub fn detect_core_kinds() -> Option<Vec<CoreKind>> {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    let max_leaf = __cpuid(0).eax;
    // Leaf 7 EDX bit 15 = hybrid part
    if max_leaf < 0x1A || __cpuid_count(7, 0).edx & (1 << 15) == 0 {
        return None;
    }
    let logical = num_cpus::get().min(64);
    let handles: Vec<_> = (0..logical)
        .map(|cpu| {
            std::thread::spawn(move || {
                if !pin_current_thread(cpu) {
                    return None;
                }
                // Give the scheduler a chance to actually migrate us before reading CPUID
                std::thread::yield_now();
                Some(kind_from_leaf_1a(__cpuid_count(0x1A, 0).eax))
            })
        })
        .collect();
    let kinds: Option<Vec<CoreKind>> = handles.into_iter().map(|h| h.join().ok().flatten()).collect();
    kinds.filter(|k| k.iter().any(|&c| c == CoreKind::Efficiency) && k.iter().any(|&c| c == CoreKind::Performance))
}

#[cfg(not(target_arch = "x86_64"))]
pub fn detect_core_kinds() -> Option<Vec<CoreKind>> {
    None
}

/// `(p_thread_count, e_thread_count)`; non-hybrid CPUs report `(logical, 0)`
pub fn hybrid_counts() -> (usize, usize) {
    match detect_core_kinds() {
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
