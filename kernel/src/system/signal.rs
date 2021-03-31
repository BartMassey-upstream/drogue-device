use core::cell::{UnsafeCell};
use core::mem;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};

extern crate std;

pub struct SignalSlot {
    free: AtomicBool,
    signal: Signal,
}

impl SignalSlot {
    pub fn acquire(&self) -> bool {
        if self.free.swap(false, Ordering::AcqRel) {
            self.signal.reset();
            true
        } else {
            false
        }
    }

    pub fn poll_wait(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.signal.poll_wait(cx)
    }

    pub fn signal(&self) {
        self.signal.signal()
    }

    pub fn release(&self) {
        self.free.store(true, Ordering::Release)
    }
}

impl Default for SignalSlot {
    fn default() -> Self {
        Self {
            free: AtomicBool::new(true),
            signal: Signal::new(),
        }
    }
}

pub(crate) struct Signal {
    state: UnsafeCell<State>,
    lock: std::sync::Mutex<()>,
}

enum State {
    None,
    Waiting(Waker),
    Signaled,
}

unsafe impl Send for Signal {}

unsafe impl Sync for Signal {}

impl Signal {
    pub fn new() -> Self {
        Self {
            state: UnsafeCell::new(State::None),
            lock: std::sync::Mutex::new(()),
        }
    }

    fn critical_section<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = self.lock.lock().unwrap();
        f()
    }

    #[allow(clippy::single_match)]
    pub fn signal(&self) {
        self.critical_section(|| unsafe {
            let state = &mut *self.state.get();
            match mem::replace(state, State::Signaled) {
                State::Waiting(waker) => waker.wake(),
                _ => {}
            }
        })
    }

    pub fn reset(&self) {
        self.critical_section(|| unsafe {
            let state = &mut *self.state.get();
            *state = State::None
        })
    }

    pub fn poll_wait(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.critical_section(|| unsafe {
            let state = &mut *self.state.get();
            match state {
                State::None => {
                    *state = State::Waiting(cx.waker().clone());
                    Poll::Pending
                }
                State::Waiting(w) if w.will_wake(cx.waker()) => Poll::Pending,
                State::Waiting(_) => Poll::Pending,
                State::Signaled => match mem::replace(state, State::None) {
                    State::Signaled => Poll::Ready(()),
                    _ => Poll::Pending,
                },
            }
        })
    }
}

impl Default for Signal {
    fn default() -> Self {
        Self::new()
    }
}
