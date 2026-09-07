//! Lifetime accounting for shell children using a claimed cache slot.
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct BuildCacheUsage(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    active: usize,
    closed: bool,
    on_idle: Option<Box<dyn FnOnce() + Send>>,
}

pub struct BuildCacheUseGuard(BuildCacheUsage);

impl BuildCacheUsage {
    /// Reserve one child before spawning; closing a claim rejects later work.
    pub fn begin(&self) -> Option<BuildCacheUseGuard> {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(BuildCacheUseGuard(self.clone()))
    }

    /// Close admission and release the claim after the last recorded child exits.
    /// The callback runs outside the mutex and may run immediately.
    pub fn close_and_when_idle(&self, callback: impl FnOnce() + Send + 'static) {
        let callback = {
            let mut state = self.0.lock().unwrap();
            assert!(!state.closed, "cache usage must only be closed once");
            state.closed = true;
            state.on_idle = Some(Box::new(callback));
            if state.active == 0 {
                state.on_idle.take()
            } else {
                None
            }
        };
        if let Some(callback) = callback {
            callback();
        }
    }
}
impl Drop for BuildCacheUseGuard {
    fn drop(&mut self) {
        let callback = {
            let mut state = self.0.0.lock().unwrap();
            state.active -= 1;
            if state.active == 0 {
                state.on_idle.take()
            } else {
                None
            }
        };
        if let Some(callback) = callback {
            callback();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn release_waits_for_all_children_and_rejects_new_uses() {
        let usage = BuildCacheUsage::default();
        let first = usage.begin().unwrap();
        let second = usage.begin().unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let flag = released.clone();
        usage.close_and_when_idle(move || flag.store(true, Ordering::SeqCst));
        assert!(!released.load(Ordering::SeqCst));
        assert!(usage.begin().is_none());
        drop(first);
        assert!(!released.load(Ordering::SeqCst));
        drop(second);
        assert!(released.load(Ordering::SeqCst));
    }
}
