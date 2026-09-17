//! CPU pinning for the separate-process mode: find taskset, split the
//! cores between server and clients, and read each child's CPU time.

use super::*;

mod procs;
pub(crate) use procs::*;

/// `taskset` on the PATH (item A's pinning primitive; `std` has no
/// affinity API and this crate forbids `unsafe`, so the syscall goes
/// through util-linux instead).
pub(crate) fn which_taskset() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("taskset");
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    })
}

/// Expand one `taskset` range element ("5" or "4-7").
pub(crate) fn range_cpus(r: &str) -> Option<Vec<u32>> {
    let mut it = r.split('-');
    let a: u32 = it.next()?.parse().ok()?;
    match it.next() {
        Some(b) => {
            let b: u32 = b.parse().ok()?;
            Some((a..=b).collect())
        }
        None => Some(vec![a]),
    }
}

/// Disjoint core sets from the CPU topology (`/sys/.../topology`): the
/// server gets the first `server_cores` *physical* cores (all their SMT
/// siblings), the client processes round-robin share the rest. Returns
/// `(server set, one set per client process)` as logical CPU numbers, or
/// `None` when the topology is not readable (non-Linux, or no SMT
/// information at all — in which case pinning is skipped rather than
/// guessing).
pub(crate) fn pin_masks(server_cores: u32, procs: u32) -> Option<(Vec<u32>, Vec<Vec<u32>>)> {
    let ncpu = std::thread::available_parallelism().ok()?.get().max(1) as u32;
    let mut groups: Vec<Vec<u32>> = Vec::new();
    for cpu in 0..ncpu {
        let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
        let list = std::fs::read_to_string(&path).ok()?;
        let set: Vec<u32> = list
            .trim()
            .split(',')
            .filter_map(range_cpus)
            .flatten()
            .collect();
        if set.is_empty() || !set.contains(&cpu) {
            return None;
        }
        if !groups.iter().any(|g| g.as_slice() == set.as_slice()) {
            groups.push(set);
        }
    }
    if groups.is_empty() {
        return None;
    }
    let nserver = server_cores.min(groups.len() as u32) as usize;
    let mut server: Vec<u32> = groups[..nserver].iter().flatten().copied().collect();
    server.sort_unstable();
    let mut clients = vec![Vec::new(); procs as usize];
    for (i, g) in groups.iter().skip(nserver).enumerate() {
        clients[i % procs as usize].extend_from_slice(g);
    }
    for c in &mut clients {
        c.sort_unstable();
    }
    Some((server, clients))
}
