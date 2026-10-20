//! One origin for every source's timestamps.
//!
//! Without this each capture zeroes its own clock at its own first frame
//! (`time - first`), so two files both start at 0.000 whatever actually
//! happened: a camera whose first frame arrived half a second late would be
//! cut half a second late against the screen, and nothing in the take would
//! say so. A shared origin makes the offset visible — and correct.
//!
//! Two clock domains meet here:
//!
//! - Windows Graphics Capture stamps frames in the system clock (QPC, 100 ns
//!   units), so its times convert to the take's timeline by one subtraction.
//! - Media Foundation hands out device-clock times: a microphone counts from
//!   wherever its stream started and a camera from its driver's own origin.
//!   Those have to be bridged through the wall clock when the first sample
//!   arrives, after which their internal spacing (the part that matters for
//!   sync) comes straight from the device.

use std::time::Instant;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// The take's common origin. Cheap to copy; every capture gets one.
#[derive(Clone, Copy, Debug)]
pub struct TakeClock {
    t0: Instant,
    /// The same moment in the system clock, so a QPC-domain timestamp needs
    /// only a subtraction and never a wall-clock guess.
    t0_qpc: i64,
}

impl TakeClock {
    /// Start timing. Call this once per take, before any capture opens.
    pub fn start() -> Self {
        Self {
            t0: Instant::now(),
            t0_qpc: qpc_100ns(),
        }
    }

    /// How long the take has been running, in 100 ns units — the unit every
    /// timestamp in this crate is expressed in.
    pub fn now_100ns(&self) -> i64 {
        (self.t0.elapsed().as_secs_f64() * 10_000_000.0) as i64
    }

    /// Take time for a frame timestamp already in the system clock domain
    /// (Windows Graphics Capture). Exact: no arrival guess in between.
    pub fn from_system(&self, ts: i64) -> i64 {
        ts - self.t0_qpc
    }

    /// The anchor for a clock of its own (Media Foundation devices). Call it
    /// the moment the first sample arrives, then subtract the result from
    /// every sample's time: the first sample lands where it really was, and
    /// the device's own spacing between samples is preserved untouched.
    pub fn device_anchor(&self, first_ts: i64) -> i64 {
        anchor_device(first_ts, self.now_100ns())
    }
}

/// Split out so the arithmetic is testable without a running clock.
fn anchor_device(first_ts: i64, now_100ns: i64) -> i64 {
    first_ts - now_100ns
}

/// The system clock in 100 ns units, the domain WGC stamps its frames in.
fn qpc_100ns() -> i64 {
    let mut frequency = 0i64;
    let mut counter = 0i64;
    // SAFETY: both calls fill a plain integer and have no preconditions; a
    // frequency is fixed for the life of the machine.
    unsafe {
        let _ = QueryPerformanceFrequency(&raw mut frequency);
        let _ = QueryPerformanceCounter(&raw mut counter);
    }
    if frequency <= 0 {
        return 0;
    }
    // i128 first: a 10 MHz counter multiplied by ten million overflows i64
    // after ~29 hours of uptime.
    (counter as i128 * 10_000_000 / frequency as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_system_timestamp_is_measured_from_the_take_start() {
        let clock = TakeClock {
            t0: Instant::now(),
            t0_qpc: 1_000,
        };
        assert_eq!(clock.from_system(1_000), 0);
        assert_eq!(clock.from_system(4_100), 3_100);
    }

    #[test]
    fn a_device_clock_is_bridged_so_its_first_sample_lands_where_it_arrived() {
        // The device's own origin is nowhere near the take's: this mic counts
        // from 5 seconds in, and its first sample was heard 0.2s after GO.
        let anchor = anchor_device(50_000_000, 2_000_000);
        assert_eq!(50_000_000 - anchor, 2_000_000, "first sample lands at 0.2s");
        // And the device's own spacing survives the bridge untouched.
        assert_eq!(60_000_000 - anchor, 12_000_000);
    }

    #[test]
    fn the_clock_runs_forward() {
        let clock = TakeClock::start();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            clock.now_100ns() >= 100_000,
            "20ms is 200,000 units of 100ns"
        );
    }
}
