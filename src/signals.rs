use crate::{
    application::ApplyError,
    production_backend::{SignalGuard, SignalScope},
    task_runtime::CancellationToken,
};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

fn record_signal(state: &AtomicU8, token: &CancellationToken, value: u8) -> bool {
    if state
        .compare_exchange(0, value, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        token.cancel();
        true
    } else {
        false
    }
}

#[cfg(unix)]
pub(crate) struct UnixSignalScope;

#[cfg(unix)]
pub(crate) struct UnixSignalGuard {
    ids: [signal_hook::SigId; 2],
    state: Arc<AtomicU8>,
}

#[cfg(unix)]
impl SignalGuard for UnixSignalGuard {
    fn exit_override(&self) -> Option<u8> {
        match self.state.load(Ordering::Relaxed) {
            2 => Some(130),
            15 => Some(143),
            _ => None,
        }
    }
}

#[cfg(unix)]
impl Drop for UnixSignalGuard {
    fn drop(&mut self) {
        for id in self.ids {
            signal_hook::low_level::unregister(id);
        }
    }
}

#[cfg(unix)]
impl SignalScope for UnixSignalScope {
    type Guard = UnixSignalGuard;

    fn install(&self, token: &CancellationToken) -> Result<Self::Guard, ApplyError> {
        let state = Arc::new(AtomicU8::new(0));
        let int_state = Arc::clone(&state);
        let int_token = token.clone();
        // Captured atomics and token operations avoid allocation and locks in handlers.
        let int = unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
                if !record_signal(&int_state, &int_token, 2) {
                    signal_hook::low_level::exit(130);
                }
            })
        }
        .map_err(|_| ApplyError {
            code: "signal_adapter_error",
            message: "signal registration failed",
            exit_override: None,
        })?;

        let term_state = Arc::clone(&state);
        let term_token = token.clone();
        let term = match unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGTERM, move || {
                if !record_signal(&term_state, &term_token, 15) {
                    signal_hook::low_level::exit(143);
                }
            })
        } {
            Ok(id) => id,
            Err(_) => {
                signal_hook::low_level::unregister(int);
                return Err(ApplyError {
                    code: "signal_adapter_error",
                    message: "signal registration failed",
                    exit_override: None,
                });
            }
        };

        Ok(UnixSignalGuard {
            ids: [int, term],
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SignalGuard, SignalScope, UnixSignalScope, record_signal};
    use crate::task_runtime::CancellationToken;
    use std::sync::atomic::{AtomicU8, Ordering};

    #[test]
    fn first_signal_records_and_cancels_without_exiting() {
        let state = AtomicU8::new(0);
        let token = CancellationToken::default();
        assert!(record_signal(&state, &token, 2));
        assert!(!record_signal(&state, &token, 15));
        assert_eq!(state.load(Ordering::Relaxed), 2);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_scopes_install_and_drop_repeatedly() {
        for _ in 0..3 {
            let token = CancellationToken::default();
            let scope = UnixSignalScope;
            let guard = scope.install(&token).expect("signals should install");
            drop(guard);
        }
    }

    #[cfg(unix)]
    fn run_signal_child(marker: &str, test_name: &str, expected: i32) {
        if std::env::var_os("EWTM_SIGNAL_CHILD").as_deref() == Some(std::ffi::OsStr::new(marker)) {
            let token = CancellationToken::default();
            let scope = UnixSignalScope;
            let _guard = scope.install(&token).expect("signals should install");
            let pid = rustix::process::getpid();
            let signal = if marker == "int" {
                rustix::process::Signal::INT
            } else {
                rustix::process::Signal::TERM
            };
            rustix::process::kill_process(pid, signal).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            rustix::process::kill_process(pid, signal).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(2));
            panic!("second signal did not terminate child");
        }

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env("EWTM_SIGNAL_CHILD", marker)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(expected));
    }

    #[cfg(unix)]
    fn run_single_signal_child(marker: &str, test_name: &str, expected: i32) {
        if std::env::var_os("EWTM_SINGLE_SIGNAL_CHILD").as_deref()
            == Some(std::ffi::OsStr::new(marker))
        {
            let token = CancellationToken::default();
            let scope = UnixSignalScope;
            let guard = scope.install(&token).expect("signals should install");
            let signal = if marker == "int" {
                rustix::process::Signal::INT
            } else {
                rustix::process::Signal::TERM
            };
            rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(guard.exit_override(), Some(expected as u8));
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env("EWTM_SINGLE_SIGNAL_CHILD", marker)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn single_sigint_transports_exit_override_in_child() {
        run_single_signal_child(
            "int",
            "signals::tests::single_sigint_transports_exit_override_in_child",
            130,
        );
    }

    #[cfg(unix)]
    #[test]
    fn single_sigterm_transports_exit_override_in_child() {
        run_single_signal_child(
            "term",
            "signals::tests::single_sigterm_transports_exit_override_in_child",
            143,
        );
    }

    #[cfg(unix)]
    #[test]
    fn double_sigint_exits_only_signal_child() {
        run_signal_child(
            "int",
            "signals::tests::double_sigint_exits_only_signal_child",
            130,
        );
    }

    #[cfg(unix)]
    #[test]
    fn double_sigterm_exits_only_signal_child() {
        run_signal_child(
            "term",
            "signals::tests::double_sigterm_exits_only_signal_child",
            143,
        );
    }
}

#[cfg(not(unix))]
pub(crate) struct UnixSignalScope;

#[cfg(not(unix))]
impl SignalScope for UnixSignalScope {
    type Guard = ();

    fn install(&self, _token: &CancellationToken) -> Result<Self::Guard, ApplyError> {
        Ok(())
    }
}
