//! The thread lifecycle every device capture shares.
//!
//! A Media Foundation reader and a WASAPI client have one thing in common that
//! matters more than their differences: neither can be created or read on any
//! thread but the one that made it (the objects are not `Send`, and COM has to
//! be initialised there anyway). So each capture is a thread plus three pieces
//! of shared state — a stop flag, a failure slot, and the channel into the
//! writer — and the same four methods on top.
//!
//! Camera, microphone and loopback each used to carry their own copy of that.
//! Three copies of ~90 lines that were identical except for the format struct
//! had already drifted: the loopback joined its thread when the device failed
//! to open, the other two dropped the handle and detached it. `DeviceCapture`
//! is that skeleton, once; each module supplies `open` and its read loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::clock::TakeClock;
use crate::error::{MediaError, Result};
use crate::frame::MediaFrame;

/// How long a device is given to open before the take is called off.
///
/// Every device this crate opens does so in well under a second; the wait is
/// for the one that hangs, which would otherwise leave the panel saying
/// "starting" for the length of the take.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// What the read loop is handed: the three things it shares with the take.
pub struct CaptureCtx {
    /// Set when the take wants the capture to stop. Checked once per iteration.
    pub stop: Arc<AtomicBool>,
    /// The queue into the writer. Bounded, and fed through `frame::offer`:
    /// a full queue drops the frame rather than blocking the device.
    pub out: SyncSender<MediaFrame>,
    /// The take's common origin, so this device's timestamps land on the same
    /// timeline as every other source's.
    pub clock: TakeClock,
}

/// A device's read loop, built **on the capture thread** (its device object is
/// not `Send`) and run there. Returns when the device ends, when the writer
/// goes away, or when `stop` is set.
pub type Pump = Box<dyn FnOnce(CaptureCtx) -> Result<()>>;

/// A running device capture. Dropping it stops it.
pub struct DeviceCapture<F> {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    format: F,
}

impl<F: Copy + Send + 'static> DeviceCapture<F> {
    /// Run `open` on a fresh thread, report the format it found, then run the
    /// read loop until it returns.
    ///
    /// `open` must do everything that touches the device — create it, read its
    /// native format, set the type we want — and must finish by returning the
    /// loop that reads it. Both happen on the capture thread for the reason in
    /// the module comment; `open` itself only has to be `Send` because it
    /// travels to that thread.
    ///
    /// `timeout_msg` is this device's own words for "it never opened", so the
    /// panel and the log can say *which* device hung.
    pub fn spawn(
        timeout_msg: &'static str,
        out: SyncSender<MediaFrame>,
        clock: TakeClock,
        open: impl FnOnce() -> Result<(F, Pump)> + Send + 'static,
    ) -> Result<Self> {
        let (ready, opened) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let (thread_stop, thread_failure) = (stop.clone(), failure.clone());

        let thread = std::thread::spawn(move || match open() {
            Ok((format, pump)) => {
                // The caller is waiting for this: it is what says the device
                // opened, and what the writer needs before it can size a frame.
                let _ = ready.send(Ok(format));
                if let Err(e) = pump(CaptureCtx {
                    stop: thread_stop,
                    out,
                    clock,
                }) && let Ok(mut slot) = thread_failure.lock()
                {
                    // A device that dies mid-take has no other way to say so:
                    // the read loop's error is the only witness, and it has to
                    // reach `stop()` or a short file passes as a good one.
                    *slot = Some(e.to_string());
                }
            }
            Err(e) => {
                let _ = ready.send(Err(e.to_string()));
            }
        });

        match wait_for_open(opened, &stop, thread, timeout_msg) {
            Ok((format, thread)) => Ok(Self {
                stop,
                failure,
                thread: Some(thread),
                format,
            }),
            // `wait_for_open` stopped and joined the thread on every failure.
            Err(e) => Err(e),
        }
    }

    /// What the device turned out to produce, known before the first frame.
    pub fn format(&self) -> F {
        self.format
    }

    /// Stop the capture and wait for the read loop to leave its blocking call.
    pub fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        self.join()
    }

    fn join(&mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let failure = self
            .failure
            .lock()
            .map_err(|e| MediaError::message(e.to_string()))?
            .take();
        match failure {
            Some(e) => Err(MediaError::message(e)),
            None => Ok(()),
        }
    }
}

impl<F> Drop for DeviceCapture<F> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Wait for the device to report its format, turning one that never opens into
/// an error instead of a take that records nothing and only says so at the end.
///
/// Every failure path here stops and joins the thread: a detached capture keeps
/// a device open for as long as the process lives, and the next take then finds
/// it busy.
fn wait_for_open<F>(
    opened: Receiver<std::result::Result<F, String>>,
    stop: &Arc<AtomicBool>,
    thread: JoinHandle<()>,
    timeout_msg: &'static str,
) -> Result<(F, JoinHandle<()>)> {
    match opened.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(format)) => Ok((format, thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(MediaError::message(e))
        }
        Err(_) => {
            // A blocking COM open cannot be interrupted, so this is best
            // effort — but it does mean a device that finishes opening late
            // exits its loop at once instead of recording into a dead take.
            stop.store(true, Ordering::SeqCst);
            let _ = thread.join();
            Err(MediaError::message(timeout_msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A format that needs no device — the skeleton is generic over it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FakeFormat(u32);

    fn silent_out() -> SyncSender<MediaFrame> {
        std::sync::mpsc::sync_channel(1).0
    }

    #[test]
    fn the_format_reaches_the_caller_and_stop_joins_the_loop() {
        let ran = Arc::new(AtomicBool::new(false));
        let thread_ran = ran.clone();
        let cap = DeviceCapture::<FakeFormat>::spawn(
            "unused in this test",
            silent_out(),
            TakeClock::start(),
            move || {
                Ok((
                    FakeFormat(7),
                    Box::new(move |_ctx| {
                        thread_ran.store(true, Ordering::SeqCst);
                        Ok(())
                    }) as Pump,
                ))
            },
        )
        .unwrap();

        assert_eq!(cap.format(), FakeFormat(7), "open reports the format");
        cap.stop().unwrap();
        assert!(
            ran.load(Ordering::SeqCst),
            "the read loop must have run before stop returned"
        );
    }

    /// A device that dies mid-take has no other way to say so: the read loop's
    /// error has to come out of `stop`, not vanish with the thread.
    #[test]
    fn an_error_in_the_read_loop_surfaces_from_stop() {
        let cap = DeviceCapture::<FakeFormat>::spawn(
            "unused in this test",
            silent_out(),
            TakeClock::start(),
            || {
                Ok((
                    FakeFormat(0),
                    Box::new(|_ctx| Err(MediaError::message("the device died"))) as Pump,
                ))
            },
        )
        .unwrap();

        let err = cap.stop().unwrap_err().to_string();
        assert!(err.contains("the device died"), "{err}");
    }

    /// A device that never opens must fail the start, and the thread must be
    /// joined rather than left holding the device.
    #[test]
    fn an_open_failure_is_a_start_failure() {
        let started = DeviceCapture::<FakeFormat>::spawn(
            "unused in this test",
            silent_out(),
            TakeClock::start(),
            || Err(MediaError::message("no such camera")),
        );
        match started {
            Ok(_) => panic!("a capture with no device started anyway"),
            Err(e) => assert!(e.to_string().contains("no such camera"), "{e}"),
        }
    }
}
