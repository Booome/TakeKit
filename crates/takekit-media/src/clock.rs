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
//!
//! # Pausing
//!
//! The clock can stop. While it is stopped every conversion here returns the
//! same instant, so a pause costs nothing to splice: no timestamp jump, no gap
//! to fill, and every source stops on the same frame because they all ask this
//! one clock. Doing it the other way round — leave the clock running and drop
//! the samples — would leave a hole in each file's timeline, and a hole is not
//! something an encoder can be handed: Media Foundation wants strictly
//! increasing timestamps per stream, so the writer would either fail or
//! silently slide the rest of the file earlier.
//!
//! Because the paused stretch is removed rather than recorded, `paused_100ns`
//! is the only proof it happened at all — it goes into the manifest, so "the
//! take is 8 minutes" and "the files add up to 8 minutes" stay the same claim.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Instant;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// The take's common origin. Cheap to clone; every capture gets one, and they
/// all share the pause state — that sharing is what makes a pause cut every
/// source on the same frame.
#[derive(Clone, Debug)]
pub struct TakeClock {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    t0: Instant,
    /// The same moment in the system clock, so a QPC-domain timestamp needs
    /// only a subtraction and never a wall-clock guess.
    t0_qpc: i64,
    paused: AtomicBool,
    /// Raw elapsed (100 ns) at which the current pause began. Only meaningful
    /// while `paused` is true.
    paused_at: AtomicI64,
    /// Paused time already banked by pauses that ended, in 100 ns units.
    paused_total: AtomicI64,
}

impl TakeClock {
    /// Start timing. Call this once per take, before any capture opens.
    pub fn start() -> Self {
        Self {
            inner: Arc::new(Inner {
                t0: Instant::now(),
                t0_qpc: qpc_100ns(),
                paused: AtomicBool::new(false),
                paused_at: AtomicI64::new(0),
                paused_total: AtomicI64::new(0),
            }),
        }
    }

    /// How long the take has been recording, in 100 ns units — the unit every
    /// timestamp in this crate is expressed in.
    ///
    /// Recorded time, not wall time: a stopwatch paused for a minute reads a
    /// minute behind the wall clock, and so does every frame.
    pub fn now_100ns(&self) -> i64 {
        visible(
            self.raw_100ns(),
            self.inner.paused_total.load(Ordering::SeqCst),
            self.inner.paused_at.load(Ordering::SeqCst),
            self.inner.paused.load(Ordering::SeqCst),
        )
    }

    /// Take time for a frame timestamp already in the system clock domain
    /// (Windows Graphics Capture). Exact: no arrival guess in between.
    pub fn from_system(&self, ts: i64) -> i64 {
        ts - self.inner.t0_qpc - self.pause_adjust()
    }

    /// The anchor for a clock of its own (Media Foundation devices). Call it
    /// the moment the first sample arrives, then subtract the result from
    /// every sample's time: the first sample lands where it really was, and
    /// the device's own spacing between samples is preserved untouched.
    pub fn device_anchor(&self, first_ts: i64) -> i64 {
        anchor_device(first_ts, self.now_100ns())
    }

    /// Stop the take's clock. `false` when it was already stopped.
    ///
    /// Call this *before* telling any writer to close its file: what is being
    /// cut is the timeline, so nothing that belongs before the cut may be
    /// stamped after it.
    pub fn pause(&self) -> bool {
        if self.inner.paused.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.inner
            .paused_at
            .store(self.raw_100ns(), Ordering::SeqCst);
        true
    }

    /// Start it again, banking how long the pause lasted. `false` when it was
    /// already running.
    pub fn resume(&self) -> bool {
        if !self.inner.paused.swap(false, Ordering::SeqCst) {
            return false;
        }
        let at = self.inner.paused_at.load(Ordering::SeqCst);
        let spent = self.raw_100ns() - at;
        self.inner.paused_total.fetch_add(spent, Ordering::SeqCst);
        true
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::SeqCst)
    }

    /// Paused time banked by pauses that already ended. What the manifest
    /// records: a pause still in progress has no length yet.
    pub fn paused_100ns(&self) -> i64 {
        self.inner.paused_total.load(Ordering::SeqCst)
    }

    /// How long the pause in progress has lasted, or 0 while running. The
    /// number behind the panel's "已暂停 0:12".
    pub fn pause_len_100ns(&self) -> i64 {
        if !self.is_paused() {
            return 0;
        }
        self.raw_100ns() - self.inner.paused_at.load(Ordering::SeqCst)
    }

    /// Wall time since the origin, pause and all.
    fn raw_100ns(&self) -> i64 {
        (self.inner.t0.elapsed().as_secs_f64() * 10_000_000.0) as i64
    }

    /// Paused time to hide from a raw interval — banked plus in progress.
    fn pause_adjust(&self) -> i64 {
        let total = self.inner.paused_total.load(Ordering::SeqCst);
        if !self.is_paused() {
            return total;
        }
        total + (self.raw_100ns() - self.inner.paused_at.load(Ordering::SeqCst))
    }
}

/// Split out so the arithmetic is testable without a running clock.
fn visible(raw: i64, paused_total: i64, paused_at: i64, paused: bool) -> i64 {
    let spent = if paused { raw - paused_at } else { 0 };
    raw - paused_total - spent
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

    /// A clock whose system-domain origin sits at `t0_qpc`, for the tests that
    /// only feed it timestamps.
    fn at(t0_qpc: i64) -> TakeClock {
        TakeClock {
            inner: Arc::new(Inner {
                t0: Instant::now(),
                t0_qpc,
                paused: AtomicBool::new(false),
                paused_at: AtomicI64::new(0),
                paused_total: AtomicI64::new(0),
            }),
        }
    }

    #[test]
    fn a_system_timestamp_is_measured_from_the_take_start() {
        let clock = at(1_000);
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

    /// The whole point of a pausable clock: recorded time stands still, so
    /// every file's timeline skips the pause instead of containing it.
    #[test]
    fn a_pause_holds_the_timeline_still() {
        let clock = TakeClock::start();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(clock.pause());
        let frozen = clock.now_100ns();
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert_eq!(clock.now_100ns(), frozen, "40ms of pause recorded as none");
        assert!(clock.pause_len_100ns() >= 400_000, "the pause is measured");

        assert!(clock.resume());
        std::thread::sleep(std::time::Duration::from_millis(10));
        let after = clock.now_100ns();
        assert!(
            after >= frozen && after < frozen + 500_000,
            "resumed at the frozen instant, and only the 10ms since counts: \
             frozen={frozen} after={after}"
        );
        assert!(
            clock.paused_100ns() >= 400_000,
            "the pause is banked at resume"
        );
    }

    /// A frame's place on the timeline is computed once, from a raw interval;
    /// two pauses must move it forward by exactly the time they hid.
    #[test]
    fn pauses_add_up() {
        assert_eq!(visible(1_000, 0, 0, false), 1_000);
        assert_eq!(visible(1_000, 300, 0, false), 700);
        // Mid-pause: the interval since the pause began is hidden too.
        assert_eq!(visible(1_000, 300, 900, true), 600);
        // A second pause, banked on top of the first.
        assert_eq!(visible(2_000, 300 + 250, 0, false), 1_450);
    }

    #[test]
    fn pausing_twice_or_resuming_twice_is_not_an_error() {
        let clock = TakeClock::start();
        assert!(clock.pause());
        assert!(clock.is_paused());
        assert!(!clock.pause(), "already paused");
        assert!(clock.resume());
        assert!(!clock.is_paused());
        assert!(!clock.resume(), "already running");
    }
}
