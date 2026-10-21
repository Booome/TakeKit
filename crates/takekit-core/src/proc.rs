//! Process liveness — the one question a client has about a `control.json`
//! it found on disk.

/// Whether a process with this pid is still running.
///
/// A failed `OpenProcess` is the common answer: the pid is gone, or it now
/// belongs to something else. A handle that opens is checked for a live exit
/// code, because on Windows a handle can outlive the process it names.
pub fn alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: `OpenProcess` is a lookup by pid with no pointer preconditions,
    // and the handle it answers with is closed on every path below. Nothing
    // derived from it is dereferenced as a pointer.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let read = GetExitCodeProcess(handle, &mut code);
        let _ = CloseHandle(handle);
        read.is_ok() && code == STILL_ACTIVE.0 as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_a_dead_pid_is_not() {
        assert!(alive(std::process::id()));
        // Pid 0 is the idle process and cannot be opened; pid reuse makes any
        // *specific* dead pid a guess, so the test asks the one question with
        // a stable answer.
        assert!(!alive(0));
    }
}
