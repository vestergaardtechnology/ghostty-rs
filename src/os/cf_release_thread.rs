//! Port of `src/os/cf_release_thread.zig`.
//!
//! STATUS: ported (macOS-only; best effort, not compiled on Linux)
//!
//! Represents the CFRelease thread. Pools of CFTypeRefs are sent to
//! this thread to be released, so that their release callback logic
//! doesn't block the execution of a high throughput thread like the
//! renderer thread.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};

use crate::datastruct::BlockingQueue;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    // TODO(port-dedupe): replace with `pkg/macos` foundation bindings.
    fn CFRelease(cf: *const c_void);
}

/// A CFTypeRef that may be sent to the release thread.
#[derive(Debug, Clone, Copy)]
pub struct CFTypeRef(pub *const c_void);
// SAFETY: CFRelease is thread-safe; ownership of the reference moves to
// the release thread.
unsafe impl Send for CFTypeRef {}

pub enum Message {
    /// Release a list of CFTypeRefs. The list is freed after releasing all
    /// the refs.
    Release { refs: Vec<CFTypeRef> },
}

/// The type used for sending messages to the thread. For now this is
/// hardcoded with a capacity. We can make this a comptime parameter in
/// the future if we want it configurable.
pub type Mailbox = BlockingQueue<Message, 64>;

/// Stand-in for the xev loop + async handles (wakeup/stop).
///
/// TODO(port-dedupe): replace with libxev `Loop`/`Async` once ported.
#[derive(Default)]
struct Signals {
    wakeup: bool,
    stop: bool,
    stopped: bool,
}

#[derive(Default)]
struct Flags {
    /// This is set to true only when an abnormal exit is detected. It
    /// tells our mailbox system to drain and ignore all messages.
    drain: bool,
}

pub struct Thread {
    /// The mailbox that can be used to send this thread messages. Note
    /// this is a blocking queue so if it is full you will get errors.
    pub mailbox: Arc<Mailbox>,

    signals: Arc<(Mutex<Signals>, Condvar)>,

    flags: Mutex<Flags>,
}

/// A handle to wake up or stop the thread from another thread.
#[derive(Clone)]
pub struct Handle {
    signals: Arc<(Mutex<Signals>, Condvar)>,
}

impl Handle {
    /// This can be used to wake up the thread.
    pub fn notify_wakeup(&self) {
        let (m, c) = &*self.signals;
        m.lock().unwrap().wakeup = true;
        c.notify_all();
    }

    /// This can be used to stop the thread on the next loop iteration.
    pub fn notify_stop(&self) {
        let (m, c) = &*self.signals;
        m.lock().unwrap().stop = true;
        c.notify_all();
    }
}

impl Thread {
    /// Initialize the thread. This does not START the thread. This only
    /// sets up all the internal state necessary prior to starting the
    /// thread. It is up to the caller to start the thread with the
    /// thread_main entrypoint.
    pub fn init() -> Thread {
        Thread {
            mailbox: Arc::new(Mailbox::new()),
            signals: Arc::new((Mutex::new(Signals::default()), Condvar::new())),
            flags: Mutex::new(Flags::default()),
        }
    }

    pub fn handle(&self) -> Handle {
        Handle {
            signals: self.signals.clone(),
        }
    }

    /// The main entrypoint for the thread.
    pub fn thread_main(&self) {
        // Call child function so we can use errors...
        self.thread_main_();

        // If our loop is not stopped, then we need to keep running so that
        // messages are drained and we can wait for the surface to send a
        // stop message.
        if !self.signals.0.lock().unwrap().stopped {
            log::warn!(
                target: "cf_release_thread",
                "abrupt cf release thread exit detected, starting xev to drain mailbox"
            );
            self.flags.lock().unwrap().drain = true;
            self.run_loop();
            log::debug!(target: "cf_release_thread", "cf release thread fully exiting after abnormal failure");
        }
    }

    fn thread_main_(&self) {
        // Right now, on Darwin, we can only name the current thread, so
        // we use this code to name the thread instead.
        // SAFETY: valid NUL-terminated name.
        unsafe { super::macos::pthread_setname_np(c"cf_release".as_ptr()) };

        // Run
        log::debug!(target: "cf_release_thread", "starting cf release thread");
        self.run_loop();
        log::debug!(target: "cf_release_thread", "starting cf release thread shutdown");
        log::debug!(target: "cf_release_thread", "cf release thread exited");
    }

    /// Run until stopped: the wakeup callback drains the mailbox and the
    /// stop callback stops the loop.
    fn run_loop(&self) {
        let (m, c) = &*self.signals;
        let mut s = m.lock().unwrap();
        loop {
            if s.wakeup {
                s.wakeup = false;
                drop(s);
                // When we wake up, we check the mailbox. Mailbox producers
                // should wake up our thread after publishing.
                self.drain_mailbox();
                s = m.lock().unwrap();
                continue;
            }
            if s.stop {
                s.stopped = true;
                return;
            }
            s = c.wait(s).unwrap();
        }
    }

    /// Drain the mailbox, handling all the messages.
    fn drain_mailbox(&self) {
        // If we're draining, we just drain the mailbox and return.
        if self.flags.lock().unwrap().drain {
            while self.mailbox.pop().is_some() {}
            return;
        }

        while let Some(message) = self.mailbox.pop() {
            match message {
                Message::Release { refs } => {
                    for r in refs {
                        // SAFETY: the sender transferred ownership of one
                        // reference.
                        unsafe { CFRelease(r.0) };
                    }
                }
            }
        }
    }
}
