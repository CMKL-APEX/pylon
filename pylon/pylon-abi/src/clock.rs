// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

//! The counter behind every `*_tsc` field of the shared page, and its rate.
//!
//! All Pylon components count "host counter ticks" (`spec/07-aarch64-support.md` §3): the TSC on
//! x86_64 and the virtual counter `CNTVCT_EL0` on aarch64. Run in a guest, the same read gives the
//! guest's counter; `PylonSharedPage::guest_to_host_tsc` converts between the two.

#[cfg(target_arch = "aarch64")]
use core::arch::asm;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::_rdtsc;

/// Reads the counter, which is `rdtsc` on x86_64 and `CNTVCT_EL0` on aarch64. Returns 0 on any
/// other architecture, where Pylon has no clock yet.
#[inline]
pub fn read_counter() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `rdtsc` has no preconditions and no side effects on x86_64.
        #[allow(unused_unsafe)]
        unsafe {
            _rdtsc()
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        let ticks: u64;
        // SAFETY: `CNTVCT_EL0` is readable from EL0 on Linux and the read has no side effects. The
        // `isb` keeps the read from executing before earlier instructions. Memory isn't declared
        // untouched (no `nomem`), so the compiler doesn't move accesses across the read either.
        unsafe {
            asm!(
                "isb",
                "mrs {ticks}, cntvct_el0",
                ticks = out(reg) ticks,
                options(nostack, preserves_flags),
            );
        }
        ticks
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        0
    }
}

/// The counter's rate in kHz where the architecture defines it, or `None` where the caller has
/// to measure it (x86_64, where the TSC rate is model specific).
///
/// On aarch64 this is `CNTFRQ_EL0`. It returns `None` there too if firmware left the register 0.
#[inline]
pub fn architectural_counter_khz() -> Option<u64> {
    #[cfg(target_arch = "aarch64")]
    {
        let hz: u64;
        // SAFETY: `CNTFRQ_EL0` is readable from EL0 on Linux and the read has no side effects.
        unsafe {
            asm!(
                "mrs {hz}, cntfrq_el0",
                hz = out(reg) hz,
                options(nomem, nostack, preserves_flags),
            );
        }
        (hz != 0).then_some(hz / 1000)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn test_counter_advances() {
        let first = read_counter();
        thread::sleep(Duration::from_millis(1));
        let second = read_counter();
        assert!(first > 0);
        assert!(second > first, "{first} then {second}");
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn test_x86_rate_is_measured_not_architectural() {
        assert_eq!(architectural_counter_khz(), None);
    }

    /// The architectural rate must match what the counter does against the monotonic clock.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_aarch64_rate_matches_the_counter() {
        use std::time::Instant;

        let khz = architectural_counter_khz().expect("CNTFRQ_EL0 is set");
        let start = Instant::now();
        let first = read_counter();
        thread::sleep(Duration::from_millis(50));
        let ticks = read_counter() - first;
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap();
        let measured_khz =
            u64::try_from(u128::from(ticks) * 1_000_000 / u128::from(nanos)).unwrap();
        // A sleep overshoots, but the counter and the timestamp cover the same span, so the two
        // rates agree closely.
        assert!(
            measured_khz.abs_diff(khz) * 100 < khz,
            "architectural {khz} kHz, measured {measured_khz} kHz"
        );
    }
}
