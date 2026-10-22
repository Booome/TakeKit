//! System audio capture through WASAPI loopback.
//!
//! Loopback records what the speakers would play: the render endpoint is
//! asked for a copy of the mix instead of asking a microphone for input. Two
//! flavours cover the use cases — the whole mix, and just the windows that
//! belong to one process tree.
//!
//! The per-process flavour goes through `ActivateAudioInterfaceAsync`, because
//! that is the only place Windows accepts the process-activation parameters;
//! `IMMDevice::Activate` cannot carry them. It is asynchronous, so a small COM
//! completion handler rendezvous with the caller.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVE_FORMAT_PCM, WAVEFORMATEX, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemAlloc,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows_core::{IUnknown, Interface, Ref, implement};

use crate::clock::TakeClock;
use crate::error::{MediaError, Result};
use crate::frame::{AudioChunk, MediaFrame};

/// What a loopback should listen to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopbackTarget {
    /// Everything the speakers play.
    All,
    /// Only the windows of one process tree — the process and its children,
    /// which is what a window really is to a user.
    ProcessTree(u32),
}

/// What the capture produces. Always PCM16 stereo at 48 kHz:
/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` makes Windows hand over exactly that
/// whatever the endpoint runs at, so the encoder never has to guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopbackFormat {
    pub sample_rate: u32,
    pub channels: u32,
}

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;

/// A running loopback capture. Dropping it stops it.
pub struct LoopbackCapture {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    format: LoopbackFormat,
}

impl LoopbackCapture {
    pub fn start(
        target: LoopbackTarget,
        out: Sender<MediaFrame>,
        clock: TakeClock,
    ) -> Result<Self> {
        let (ready, opened) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));

        let thread_stop = stop.clone();
        let thread_failure = failure.clone();
        // The capture and its COM objects are made on this thread and stay
        // there, like every other device in this crate.
        let thread = std::thread::spawn(move || match open(target) {
            Ok((client, capture)) => {
                let _ = ready.send(Ok(LoopbackFormat {
                    sample_rate: SAMPLE_RATE,
                    channels: CHANNELS,
                }));
                if let Err(e) = pump(&client, &capture, thread_stop, out, clock)
                    && let Ok(mut slot) = thread_failure.lock()
                {
                    *slot = Some(e.to_string());
                }
            }
            Err(e) => {
                let _ = ready.send(Err(e.to_string()));
            }
        });

        let format = match opened.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(format)) => format,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(MediaError::message(e));
            }
            Err(_) => {
                let _ = thread.join();
                return Err(MediaError::message(
                    "the system audio capture did not open within ten seconds",
                ));
            }
        };

        Ok(Self {
            stop,
            failure,
            thread: Some(thread),
            format,
        })
    }

    pub fn format(&self) -> LoopbackFormat {
        self.format
    }

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

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Bring up a loopback capture client and its capture client.
fn open(target: LoopbackTarget) -> Result<(IAudioClient, IAudioCaptureClient)> {
    // SAFETY: COM has to be initialised on this thread before WASAPI is used
    // from it.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let client = match target {
        LoopbackTarget::All => activate_default()?,
        LoopbackTarget::ProcessTree(pid) => activate_process_tree(pid)?,
    };

    // SAFETY: `client` is owned here; the format is a plain struct copied in,
    // and the capture client outlives this function only with `client`.
    unsafe {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * CHANNELS * 2,
            nBlockAlign: (CHANNELS * 2) as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000i64, // 20 ms; loopback has no event to wait on anyway
            0i64,
            &format,
            None::<*const windows_core::GUID>,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        Ok((client, capture))
    }
}

/// The plain route: whatever the default playback device is mixing.
fn activate_default() -> Result<IAudioClient> {
    // SAFETY: the enumerator and device are owned; the client outlives this
    // function only through the caller.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        Ok(device.Activate::<IAudioClient>(CLSCTX_ALL, None::<*const PROPVARIANT>)?)
    }
}

/// The per-process route: only the audio of `pid` and its children.
///
/// Windows takes the process filter only through
/// `ActivateAudioInterfaceAsync`, which answers on a COM callback, so the
/// activation parameters travel there as a `PROPVARIANT` blob. The device is
/// not a real endpoint but the virtual `VAD\Process_Loopback` one — that is
/// the id this activation type answers to.
fn activate_process_tree(pid: u32) -> Result<IAudioClient> {
    // SAFETY: every object here is owned by this frame except the client that
    // comes back through the channel; the blob is a heap copy freed by the
    // `PROPVARIANT` drop, and the callback is joined on before that.
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };

        // `PROPVARIANT` clears itself with `CoTaskMemFree` on drop, so the blob
        // it points at must come from `CoTaskMemAlloc` — a stack copy would be
        // freed as if it were heap and take the process down with it.
        let size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
        let blob = CoTaskMemAlloc(size) as *mut u8;
        if blob.is_null() {
            return Err(MediaError::message(
                "out of memory for the process-loopback activation",
            ));
        }
        std::ptr::copy_nonoverlapping(&params as *const _ as *const u8, blob, size);

        let mut variant = PROPVARIANT::default();
        {
            let top = &mut variant.Anonymous;
            let inner = &mut *top.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob.cbSize = size as u32;
            inner.Anonymous.blob.pBlobData = blob;
        }

        let (tx, rx) = std::sync::mpsc::channel::<Result<IAudioClient>>();
        let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone { done: tx }.into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant as *const PROPVARIANT),
            &handler,
        )?;

        // The answer comes on an RPC worker thread, so the caller waits here.
        let client = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| MediaError::message("the process-loopback activation never answered"))??;
        std::mem::drop(operation);
        Ok(client)
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationDone {
    done: Sender<Result<IAudioClient>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(
        &self,
        activateoperation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows_core::Result<()> {
        // SAFETY: out-parameters are live locals; the interface comes back
        // reference-counted and is cast to the type we asked for.
        unsafe {
            let Some(operation) = activateoperation.as_ref() else {
                return Ok(());
            };
            let mut result: windows_core::HRESULT = S_OK;
            let mut unknown: Option<IUnknown> = None;
            let _ = operation.GetActivateResult(&mut result, &mut unknown);
            let outcome = match (result, unknown) {
                (r, Some(unknown)) if r.is_ok() => unknown.cast::<IAudioClient>().map_err(|e| {
                    MediaError::message(format!("activation returned the wrong type: {e}"))
                }),
                (r, _) => Err(MediaError::message(format!(
                    "the process-loopback activation failed ({r:?})"
                ))),
            };
            let _ = self.done.send(outcome);
        }
        Ok(())
    }
}

/// Read the mix until told to stop.
fn pump(
    client: &IAudioClient,
    capture: &IAudioCaptureClient,
    stop: Arc<AtomicBool>,
    out: Sender<MediaFrame>,
    clock: TakeClock,
) -> Result<()> {
    // SAFETY: starting and stopping the client only moves its state.
    unsafe { client.Start()? };

    let bytes_per_frame = CHANNELS as usize * 2;
    let mut anchor: Option<i64> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // SAFETY: `pending` is filled by the API on this frame.
        let pending = unsafe { capture.GetNextPacketSize()? };
        if pending == 0 {
            // The mix runs at its own pace; sleeping keeps this thread from
            // spinning a core for the length of a take.
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        // SAFETY: `data` is filled by the API and released below; the bytes are
        // copied out before that.
        unsafe {
            let mut data = std::ptr::null_mut::<u8>();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut device_time = 0u64;
            capture.GetBuffer(
                &mut data,
                &mut frames,
                &mut flags,
                Some(&mut device_time),
                None,
            )?;
            let bytes = frames as usize * bytes_per_frame;
            let mut chunk = vec![0u8; bytes];
            if !data.is_null() && flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
                std::ptr::copy_nonoverlapping(data, chunk.as_mut_ptr(), bytes);
            }
            capture.ReleaseBuffer(frames)?;

            // WASAPI's device time is in 100 ns units, the take's timeline too.
            let device_time = device_time as i64;
            let anchor = *anchor.get_or_insert_with(|| clock.device_anchor(device_time));
            if out
                .send(MediaFrame::Audio(AudioChunk {
                    data: chunk,
                    time: device_time - anchor,
                }))
                .is_err()
            {
                break;
            }
        }
    }
    // SAFETY: the client is owned here and stopping it is idempotent.
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

/// The process that owns `hwnd` — what a window handle turns into before the
/// per-process loopback filter can use it.
pub fn window_pid(hwnd: isize) -> Option<u32> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    // SAFETY: `hwnd` is only passed through; `pid` is a live out-parameter.
    unsafe {
        let thread = GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid));
        if thread == 0 || pid == 0 {
            None
        } else {
            Some(pid)
        }
    }
}
