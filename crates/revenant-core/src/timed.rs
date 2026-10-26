//! Running a call that may never return (a read of a dying device) on its own thread, waiting for it at most a given
//! time. A call that runs late is left behind: its thread stays blocked until the system gives up on the read.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::scan::CancelToken;

/// How often a wait looks at its cancel token.
const TICK: Duration = Duration::from_millis(10);

/// How a timed call ended.
#[derive(Debug)]
pub(crate) enum Timed<T> {
    Done(T),
    /// Not finished in time: its thread is left running.
    Late,
    /// The cancel token was set while waiting: its thread is left running.
    Cancelled,
}

/// Runs `f` on a new thread and waits for it at most `timeout` (and, with `cancel`, until it is cancelled). A thread
/// that cannot be started counts as timed out; a panic in `f` is passed on to the caller.
pub(crate) fn timed<T: Send + 'static>(
    timeout: Duration,
    cancel: Option<&CancelToken>,
    f: impl FnOnce() -> T + Send + 'static,
) -> Timed<T> {
    let (tx, rx) = mpsc::sync_channel(1);
    let Ok(handle) = std::thread::Builder::new()
        .name("revenant-timed".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
    else {
        return Timed::Late;
    };
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Timed::Cancelled;
        }
        let now = Instant::now();
        if now >= deadline {
            return Timed::Late;
        }
        let wait = if cancel.is_some() {
            (deadline - now).min(TICK)
        } else {
            deadline - now
        };
        match rx.recv_timeout(wait) {
            Ok(value) => return Timed::Done(value),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => match handle.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                // Sent nothing and did not panic: cannot happen; counted as late.
                Ok(()) => return Timed::Late,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_waited_for_at_most_its_timeout() {
        let start = Instant::now();
        let r = timed(Duration::from_millis(50), None, || {
            std::thread::sleep(Duration::from_secs(2));
        });
        assert!(matches!(r, Timed::Late));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            timed(Duration::from_secs(5), None, || 7),
            Timed::Done(7)
        ));
        let cancel = CancelToken::default();
        cancel.cancel();
        let r = timed(Duration::from_secs(5), Some(&cancel), || {
            std::thread::sleep(Duration::from_secs(2));
        });
        assert!(matches!(r, Timed::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[should_panic(expected = "inside")]
    fn a_panic_reaches_the_caller() {
        let _ = timed(Duration::from_secs(5), None, || panic!("inside"));
    }
}
