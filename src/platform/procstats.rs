//! Memory and CPU for a set of processes.
//!
//! The browser is not one process. WebView2 runs a browser process, a GPU
//! process, a network service and one renderer per site, and our own host
//! process sits alongside them. A useful "how much is this browser costing me"
//! figure has to sum all of them, which is why this takes a PID list rather
//! than measuring itself.
//!
//! # Which memory number
//!
//! We sum `PrivateUsage` (private commit) rather than working set. Summing
//! working sets across Chromium processes double-counts heavily — they share a
//! great deal of mapped memory — so the total would read far higher than the
//! browser actually costs. Private commit is close to what Task Manager shows
//! in its "Commit size" column and does not double-count.

use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ,
};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcessStats {
    pub memory_bytes: u64,
    /// Share of a single core, averaged over the interval since the previous
    /// sample. Can exceed 100 on a multi-core machine only if normalisation is
    /// removed; as written it is already divided by the core count.
    pub cpu_percent: f32,
    pub process_count: u32,
}

/// Holds the previous CPU reading so a rate can be derived.
///
/// CPU time is a counter, not a gauge: a single reading says nothing. The first
/// `sample` therefore reports 0% and only establishes the baseline.
pub struct ProcessSampler {
    previous: Option<(Instant, u64)>,
    cores: u32,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self {
            previous: None,
            cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1).max(1),
        }
    }

    /// Sample the given processes plus our own.
    pub fn sample(&mut self, pids: &[u32]) -> ProcessStats {
        let mut memory_bytes: u64 = 0;
        let mut cpu_100ns: u64 = 0;
        let mut counted: u32 = 0;

        let own = unsafe { GetCurrentProcessId() };
        for pid in pids.iter().copied().chain(std::iter::once(own)) {
            // A renderer can exit between us listing it and opening it; that is
            // ordinary, not an error worth surfacing.
            let Some(handle) = open(pid) else { continue };

            if let Some(bytes) = private_bytes(handle) {
                memory_bytes = memory_bytes.saturating_add(bytes);
                counted += 1;
            }
            if let Some(ticks) = cpu_ticks(handle) {
                cpu_100ns = cpu_100ns.saturating_add(ticks);
            }

            unsafe {
                let _ = CloseHandle(handle);
            }
        }

        let now = Instant::now();
        let cpu_percent = match self.previous {
            Some((then, previous_cpu)) => {
                // FILETIME ticks are 100 ns.
                let elapsed_100ns = now.duration_since(then).as_nanos() / 100;
                let busy_100ns = cpu_100ns.saturating_sub(previous_cpu);

                if elapsed_100ns == 0 {
                    0.0
                } else {
                    let share =
                        busy_100ns as f64 / (elapsed_100ns as f64 * self.cores as f64) * 100.0;
                    // A process exiting between samples makes the delta look
                    // negative-ish; clamping keeps the UI from flickering.
                    share.clamp(0.0, 100.0) as f32
                }
            }
            // First sample only establishes the baseline.
            None => 0.0,
        };
        self.previous = Some((now, cpu_100ns));

        ProcessStats { memory_bytes, cpu_percent, process_count: counted }
    }
}

fn open(pid: u32) -> Option<HANDLE> {
    unsafe {
        // The narrowest rights that still allow both queries. Notably not
        // PROCESS_QUERY_INFORMATION, which is broader than we need.
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, false, pid).ok()
    }
}

fn private_bytes(handle: HANDLE) -> Option<u64> {
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        // GetProcessMemoryInfo takes the base struct; the EX layout is a
        // superset and is selected by the size we pass.
        GetProcessMemoryInfo(handle, &mut counters as *mut _ as *mut _, size).ok()?;
        Some(counters.PrivateUsage as u64)
    }
}

fn cpu_ticks(handle: HANDLE) -> Option<u64> {
    unsafe {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).ok()?;
        Some(filetime_to_u64(kernel) + filetime_to_u64(user))
    }
}

fn filetime_to_u64(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_halves_are_combined_in_the_right_order() {
        let value = FILETIME { dwLowDateTime: 0x89AB_CDEF, dwHighDateTime: 0x0123_4567 };
        assert_eq!(filetime_to_u64(value), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn the_first_sample_reports_no_cpu_but_does_report_memory() {
        // Sampling our own process is enough to exercise both paths.
        let mut sampler = ProcessSampler::new();
        let first = sampler.sample(&[]);
        assert_eq!(first.cpu_percent, 0.0, "a single reading cannot yield a rate");
        assert!(first.memory_bytes > 0);
        assert_eq!(first.process_count, 1);
    }

    #[test]
    fn a_second_sample_produces_a_bounded_rate() {
        let mut sampler = ProcessSampler::new();
        sampler.sample(&[]);
        // Burn a little CPU so the delta is not trivially zero.
        let mut sink: u64 = 0;
        for i in 0..2_000_000u64 {
            sink = sink.wrapping_add(i);
        }
        std::hint::black_box(sink);

        let second = sampler.sample(&[]);
        assert!((0.0..=100.0).contains(&second.cpu_percent), "got {}", second.cpu_percent);
    }

    #[test]
    fn a_dead_pid_is_skipped_rather_than_failing() {
        let mut sampler = ProcessSampler::new();
        // PID 0 is the system idle process and cannot be opened this way.
        let stats = sampler.sample(&[0, u32::MAX]);
        assert_eq!(stats.process_count, 1, "only our own process should count");
    }
}
