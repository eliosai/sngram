//! Dynamic system resource detection and thread allocation.

use sysinfo::System;

const RESERVED_THREADS: usize = 2;

/// Detected machine capabilities.
#[non_exhaustive]
#[derive(Debug, Clone)]
#[must_use]
pub struct MachineProfile {
    pub cores: usize,
    pub ram_mb: u64,
    pub os: String,
    pub arch: &'static str,
}

impl MachineProfile {
    #[must_use]
    pub fn detect() -> Self {
        let sys = System::new_all();
        Self {
            cores: num_cpus::get(),
            ram_mb: sys.total_memory() / (1024 * 1024),
            os: System::long_os_version().unwrap_or_default(),
            arch: std::env::consts::ARCH,
        }
    }
}

/// Thread counts per dataset worker group.
///
/// On machines with fewer than 5 cores, workers may exceed `usable`
/// because each dataset requires at least 1 thread (3 minimum).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct ThreadAllocation {
    pub stack: usize,
    pub fineweb: usize,
    pub redpajama: usize,
    pub reserved: usize,
}

impl ThreadAllocation {
    /// Allocate threads proportionally: 50/30/20 split.
    ///
    /// Each dataset gets at least 1 thread. On small machines
    /// (< 5 cores) this intentionally oversubscribes.
    #[must_use]
    pub fn from_cores(cores: usize) -> Self {
        let usable = cores.saturating_sub(RESERVED_THREADS).max(1);
        let stack = (usable + 1) / 2;
        let fineweb = (usable * 3 + 9) / 10;
        let redpajama = usable.saturating_sub(stack + fineweb).max(1);
        fit(stack, fineweb, redpajama, usable)
    }

    #[must_use]
    pub fn total(&self) -> usize {
        self.stack + self.fineweb + self.redpajama + self.reserved
    }
}

fn fit(s: usize, f: usize, r: usize, usable: usize) -> ThreadAllocation {
    let sum = s + f + r;
    if sum <= usable {
        return ThreadAllocation {
            stack: s, fineweb: f, redpajama: r,
            reserved: RESERVED_THREADS,
        };
    }
    let ns = (s * usable / sum).max(1);
    let nf = (f * usable / sum).max(1);
    let nr = usable.saturating_sub(ns + nf).max(1);
    ThreadAllocation {
        stack: ns, fineweb: nf, redpajama: nr,
        reserved: RESERVED_THREADS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_nonzero_resources() {
        let p = MachineProfile::detect();
        assert!(p.cores >= 1);
        assert!(p.ram_mb > 0);
    }

    #[test]
    fn workers_do_not_exceed_usable() {
        for cores in 1..=128 {
            let a = ThreadAllocation::from_cores(cores);
            let usable = cores.saturating_sub(RESERVED_THREADS).max(1);
            let workers = a.stack + a.fineweb + a.redpajama;
            assert!(
                workers <= usable + 2,
                "cores={cores} workers={workers} usable={usable}",
            );
        }
    }

    #[test]
    fn stack_gets_most_threads() {
        let a = ThreadAllocation::from_cores(20);
        assert!(a.stack >= a.fineweb);
        assert!(a.fineweb >= a.redpajama);
    }

    #[test]
    fn single_core_still_allocates() {
        let a = ThreadAllocation::from_cores(1);
        assert!(a.stack >= 1);
    }
}
