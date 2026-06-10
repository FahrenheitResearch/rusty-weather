//! Polite-by-default scheduling for the compute-heavy binaries.
//!
//! The ingest/render bins saturate every core through rayon (the heavy
//! ECAPE stage in particular), which makes an interactive desktop unusable
//! while a run is in flight. This module gives each binary the same two
//! knobs, applied once at the very top of `main`:
//!
//! - drop the process to BELOW_NORMAL priority (Windows; no-op elsewhere)
//! - cap the global rayon pool at `cores - 2` so the desktop keeps two
//!   cores for the foreground
//!
//! `--full-throttle` restores the old behavior (normal priority, every
//! core) for dedicated nodes; `--threads N` pins the pool size explicitly
//! and wins over both defaults.
//!
//! Shared via `#[path]` inclusion from each bin, like `ingest_compute` /
//! `store_render`.

/// Drop the CURRENT PROCESS to BELOW_NORMAL_PRIORITY_CLASS so foreground
/// apps preempt the ingest/render compute. Best-effort: a failure (which
/// SetPriorityClass essentially never reports for the own-process
/// pseudo-handle) just leaves the inherited priority in place.
#[cfg(windows)]
pub fn set_background_priority() {
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
    };
    // SAFETY: GetCurrentProcess returns the process pseudo-handle (never
    // fails, never needs closing); SetPriorityClass on it only adjusts the
    // calling process's own scheduling class.
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// No-op off Windows: there is no portable std priority API, and the dev
/// box this politeness exists for is Windows. The rayon thread cap in
/// `init_rayon_threads` is the cross-platform half of polite mode.
#[cfg(not(windows))]
pub fn set_background_priority() {}

/// Build the GLOBAL rayon pool with the policy thread count: an explicit
/// `--threads N` wins; else `--full-throttle` uses every core; else polite
/// default `max(1, cores - 2)`. Returns the count the pool runs with.
///
/// MUST be called before anything touches rayon (the global pool is built
/// lazily on first use and cannot be resized) — call it at the very top of
/// `main`. If the pool was already built (e.g. another test in the same
/// process got there first), the build error is ignored and the existing
/// pool's count is reported instead.
pub fn init_rayon_threads(requested: Option<usize>, full_throttle: bool) -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    let threads = match requested {
        Some(n) => n.max(1),
        None if full_throttle => cores,
        None => cores.saturating_sub(2).max(1),
    };
    if rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .is_err()
    {
        return rayon::current_num_threads();
    }
    threads
}

/// One-call wiring for a binary's `main`: apply the priority drop (unless
/// `--full-throttle`), build the global rayon pool, and print the one-line
/// mode banner. Call FIRST in `main`, before any rayon use.
pub fn apply(requested: Option<usize>, full_throttle: bool) {
    if !full_throttle {
        set_background_priority();
    }
    let threads = init_rayon_threads(requested, full_throttle);
    if full_throttle {
        println!("throttle: full");
    } else {
        println!(
            "throttle: below-normal priority, {threads} threads \
             (use --full-throttle on dedicated nodes)"
        );
    }
}
