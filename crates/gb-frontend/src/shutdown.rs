//! Cooperative shutdown on SIGINT/SIGTERM/SIGHUP (Ctrl-C, `kill`, terminal
//! closed; Ctrl-C/Break/close on Windows). The signal handler only sets a flag
//! and wakes the event loop; the loop then exits through the normal `exiting`
//! path, which flushes battery RAM.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Default)]
pub struct ShutdownFlag(Arc<AtomicBool>);

impl ShutdownFlag {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_visible_through_clones_and_across_threads() {
        let flag = ShutdownFlag::default();
        assert!(!flag.requested());
        let remote = flag.clone();
        std::thread::spawn(move || remote.request()).join().unwrap();
        assert!(flag.requested());
        flag.request(); // idempotent
        assert!(flag.requested());
    }
}
