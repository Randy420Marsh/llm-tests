//! Keeps the app's own threads (GUI, sensor sampler, sensor helper process) off the core being measured.
//!
//! While a test runs, the app reserves one logical CPU for itself: the least busy one, preferring the
//! highest-numbered ("furthest") core. Before a test that is pinned to particular cores starts, the
//! benchmark calls [`keep_off`]: if the reserved core is one of the tested cores, or a hyper-threading
//! sibling of one (it shares the physical core), the app moves to another core, waits until the GUI
//! thread confirms the move, and gives the scheduler a moment to settle before the test starts.
//!
//! Without a GUI (CLI, tests) nothing is reserved and every call here is a no-op.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const NONE: usize = usize::MAX;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Core the app's threads should run on
static DESIRED: AtomicUsize = AtomicUsize::new(NONE);
/// Core the GUI thread has actually pinned itself to
static GUI_APPLIED: AtomicUsize = AtomicUsize::new(NONE);
/// How often the app had to move out of the way (shown in the log)
static MOVES: AtomicUsize = AtomicUsize::new(0);
/// Serialises reserve / keep_off so two suites cannot pick cores at the same time
static PICK: Mutex<()> = Mutex::new(());

/// Time the scheduler gets after a move before a measurement starts
const SETTLE: Duration = Duration::from_millis(300);
/// Longest wait for the GUI thread to confirm a move (it may be minimised and not drawing)
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// topology: which logical CPUs share a physical core
// ---------------------------------------------------------------------------------------------

/// Logical CPUs grouped by physical core (hyper-threading siblings together)
pub fn sibling_groups() -> Vec<Vec<usize>> {
    let n = num_cpus::get();
    let groups = os_sibling_groups().unwrap_or_default();
    // every logical CPU the OS did not report is its own core
    let mut seen = vec![false; n];
    let mut out: Vec<Vec<usize>> = Vec::new();
    for g in groups {
        let g: Vec<usize> = g.into_iter().filter(|&c| c < n && !seen[c]).collect();
        g.iter().for_each(|&c| seen[c] = true);
        if !g.is_empty() {
            out.push(g);
        }
    }
    for (c, s) in seen.iter().enumerate() {
        if !s {
            out.push(vec![c]);
        }
    }
    out.sort();
    out
}

/// `cpu` and every logical CPU sharing its physical core
pub fn with_siblings(cpu: usize, groups: &[Vec<usize>]) -> Vec<usize> {
    groups.iter().find(|g| g.contains(&cpu)).cloned().unwrap_or_else(|| vec![cpu])
}

#[cfg(target_os = "linux")]
fn os_sibling_groups() -> Option<Vec<Vec<usize>>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for cpu in 0..num_cpus::get() {
        let path = format!("/sys/devices/system/cpu/cpu{}/topology/thread_siblings_list", cpu);
        let list = std::fs::read_to_string(path).ok()?;
        let g = parse_cpu_list(list.trim());
        if !g.is_empty() && !groups.contains(&g) {
            groups.push(g);
        }
    }
    Some(groups)
}

#[cfg(target_os = "windows")]
fn os_sibling_groups() -> Option<Vec<Vec<usize>>> {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    unsafe {
        let mut len = 0u32;
        let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut len,
        )
        .ok()?;
        let mut groups = Vec::new();
        let mut off = 0usize;
        while off + std::mem::size_of::<u32>() * 2 <= len as usize {
            let info = &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            if info.Relationship == RelationProcessorCore {
                let p = &info.Anonymous.Processor;
                // processor group 0 only: the benchmarks pin within the first 64 logical CPUs
                if p.GroupCount >= 1 && p.GroupMask[0].Group == 0 {
                    let mask = p.GroupMask[0].Mask as u64;
                    groups.push((0..64).filter(|b| mask >> b & 1 == 1).collect());
                }
            }
            if info.Size == 0 {
                break;
            }
            off += info.Size as usize;
        }
        Some(groups)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn os_sibling_groups() -> Option<Vec<Vec<usize>>> {
    None
}

/// "0-3,8,10-11" -> [0,1,2,3,8,10,11]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    out.extend(a..=b);
                }
            }
            None => out.extend(part.parse::<usize>().ok()),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// choosing the app core
// ---------------------------------------------------------------------------------------------

/// Best core for the app: not in `blocked` (nor a sibling of one), lowest load in 5 % steps, and among
/// equally idle cores the highest-numbered one. `None` if every core is blocked.
pub fn choose(usage: &[f32], blocked: &[usize], groups: &[Vec<usize>]) -> Option<usize> {
    let mut off: Vec<usize> = Vec::new();
    for &b in blocked {
        off.extend(with_siblings(b, groups));
    }
    (0..usage.len().min(64))
        .filter(|c| !off.contains(c))
        .min_by_key(|&c| ((usage[c].max(0.0) / 5.0) as u32, std::cmp::Reverse(c)))
}

fn core_usage() -> Vec<f32> {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(150)));
    sys.refresh_cpu();
    sys.cpus().iter().map(|c| c.cpu_usage()).collect()
}

// ---------------------------------------------------------------------------------------------
// reservation
// ---------------------------------------------------------------------------------------------

/// Start keeping the app on one core (called when a test starts). Returns the chosen core.
pub fn reserve() -> Option<usize> {
    let _g = PICK.lock().unwrap_or_else(|e| e.into_inner());
    let usage = core_usage();
    let core = choose(&usage, &[], &sibling_groups())?;
    DESIRED.store(core, Ordering::SeqCst);
    MOVES.store(0, Ordering::SeqCst);
    ACTIVE.store(true, Ordering::SeqCst);
    Some(core)
}

/// Let the app use every core again (called when the test ends); the GUI unpins on its next frame
pub fn release() {
    ACTIVE.store(false, Ordering::SeqCst);
    DESIRED.store(NONE, Ordering::SeqCst);
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

pub fn current() -> Option<usize> {
    Some(DESIRED.load(Ordering::SeqCst)).filter(|&c| c != NONE && is_active())
}

/// Times the app moved out of the way since `reserve`
pub fn moves() -> usize {
    MOVES.load(Ordering::SeqCst)
}

/// Called by the GUI thread every frame: follow the reservation (or drop it)
pub fn apply_gui() {
    let want = if is_active() { DESIRED.load(Ordering::SeqCst) } else { NONE };
    let have = GUI_APPLIED.load(Ordering::SeqCst);
    if want == have {
        return;
    }
    if want == NONE {
        unpin_current_thread();
    } else if !crate::topology::pin_current_thread(want) {
        return; // the OS refused; try again next frame
    }
    GUI_APPLIED.store(want, Ordering::SeqCst);
}

thread_local! {
    static HELPER_APPLIED: std::cell::Cell<usize> = const { std::cell::Cell::new(NONE) };
}

/// Called periodically by helper threads (the sensor sampler): same as `apply_gui`, per thread
pub fn apply_helper() {
    let want = if is_active() { DESIRED.load(Ordering::SeqCst) } else { NONE };
    HELPER_APPLIED.with(|h| {
        if h.get() == want {
            return;
        }
        if want == NONE {
            unpin_current_thread();
        } else if !crate::topology::pin_current_thread(want) {
            return;
        }
        h.set(want);
    });
}

/// Make sure the app is not on any of `test_cores` (or their siblings) before a pinned test runs.
/// Moves it if needed, waits until the GUI thread has followed and lets the scheduler settle.
/// Returns the core the app moved to, if it had to move.
pub fn keep_off(test_cores: &[usize]) -> Option<usize> {
    if !is_active() || test_cores.is_empty() {
        return None;
    }
    let _g = PICK.lock().unwrap_or_else(|e| e.into_inner());
    let groups = sibling_groups();
    let here = DESIRED.load(Ordering::SeqCst);
    let clash = test_cores.iter().any(|&c| with_siblings(c, &groups).contains(&here));
    if !clash {
        return None;
    }
    let usage = core_usage();
    // also avoid the core we are leaving: the GUI is still loading it right now
    let target = choose(&usage, test_cores, &groups)?;
    DESIRED.store(target, Ordering::SeqCst);
    MOVES.fetch_add(1, Ordering::SeqCst);
    let t0 = Instant::now();
    while GUI_APPLIED.load(Ordering::SeqCst) != target && t0.elapsed() < ACK_TIMEOUT && is_active() {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(SETTLE);
    Some(target)
}

/// Undo a pin: allow every CPU the process may use
pub fn unpin_current_thread() {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessAffinityMask, SetThreadAffinityMask};
        let (mut proc_mask, mut sys_mask) = (0usize, 0usize);
        if GetProcessAffinityMask(GetCurrentProcess(), &mut proc_mask, &mut sys_mask).is_ok() && proc_mask != 0 {
            SetThreadAffinityMask(GetCurrentThread(), proc_mask);
        }
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for c in 0..num_cpus::get().min(libc::CPU_SETSIZE as usize) {
            libc::CPU_SET(c, &mut set);
        }
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_parse() {
        assert_eq!(parse_cpu_list("0-3,8,10-11"), vec![0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(parse_cpu_list("5"), vec![5]);
        assert_eq!(parse_cpu_list(""), Vec::<usize>::new());
    }

    #[test]
    fn app_core_is_idle_far_and_never_a_sibling_of_the_tested_core() {
        // 8 logical CPUs, 0/1 and 2/3 are hyper-threading pairs
        let groups = vec![vec![0, 1], vec![2, 3], vec![4], vec![5], vec![6], vec![7]];
        let usage = [3.0, 1.0, 2.0, 0.0, 1.0, 4.0, 2.0, 1.5];
        // all within the same 5 % step: the furthest core wins
        assert_eq!(choose(&usage, &[], &groups), Some(7));
        // testing core 7: the next furthest idle one
        assert_eq!(choose(&usage, &[7], &groups), Some(6));
        // a busy core is avoided even if it is far away
        let busy = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 90.0, 95.0];
        assert_eq!(choose(&busy, &[], &groups), Some(5));
        // testing core 2 blocks its sibling 3 as well
        let only_pairs = vec![vec![0, 1], vec![2, 3]];
        assert_eq!(choose(&[0.0; 4], &[2], &only_pairs), Some(1));
        assert_eq!(choose(&[0.0; 4], &[0, 2], &only_pairs), None, "nothing left outside the tested cores");
    }

    #[test]
    fn every_logical_cpu_belongs_to_one_group() {
        let g = sibling_groups();
        let mut all: Vec<usize> = g.iter().flatten().copied().collect();
        all.sort();
        assert_eq!(all, (0..num_cpus::get()).collect::<Vec<_>>());
        assert_eq!(with_siblings(0, &g).contains(&0), true);
    }

    #[test]
    fn keep_off_is_a_no_op_without_a_reservation() {
        // nothing reserved (CLI / tests): never blocks, never moves
        if !is_active() {
            assert_eq!(keep_off(&[0]), None);
        }
    }
}
