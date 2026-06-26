//! A small bounded worker pool. Callers enqueue work here and return, keeping a
//! caller's serial dispatch loop free (in the FUSE backend this avoids the
//! fork/exec `FLUSH` deadlock; see the fuse crate docs). The number of OS
//! threads is fixed, so this is **not** a thread-per-callback design.
//!
//! Backend-independent (pure `std`), so both the FUSE mount and the worktree
//! projection (for off-callback speculative prefetch) share it.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A fixed-size pool of worker threads draining one job queue.
pub struct Pool {
    tx: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    /// Spawn `n` worker threads (at least 1).
    pub fn new(n: usize) -> Pool {
        let n = n.max(1);
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let mut workers = Vec::with_capacity(n);
        for _ in 0..n {
            let rx: Arc<Mutex<Receiver<Job>>> = Arc::clone(&rx);
            workers.push(std::thread::spawn(move || loop {
                // Hold the lock only across `recv`, then run the job unlocked so
                // other workers can pull concurrently.
                let job = {
                    let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.recv()
                };
                match job {
                    // Isolate panics per job: a panicking callback must never kill
                    // the worker (which would shrink the pool toward a wedged
                    // mount). At worst that one job's effect is dropped;
                    // it can never cascade.
                    Ok(job) => {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                    Err(_) => break, // all senders dropped → drain + exit
                }
            }));
        }
        Pool {
            tx: Some(tx),
            workers,
        }
    }

    /// Enqueue a job. Dropped silently if the pool is shutting down.
    pub fn spawn(&self, f: impl FnOnce() + Send + 'static) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Box::new(f));
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Close the queue so workers exit once drained, then join them.
        self.tx.take();
        // A pool whose jobs hold an `Arc` to the pool's owner can have its owner
        // (and thus this `Pool`) dropped *from one of its own worker threads*
        // when the last such `Arc` is released inside a job. Never self-join in
        // that case — `JoinHandle::join` on the current thread blocks forever.
        // That worker exits on its own once this job returns and the closed
        // queue makes its `recv()` yield `Err`.
        let current = std::thread::current().id();
        for w in self.workers.drain(..) {
            if w.thread().id() == current {
                continue;
            }
            let _ = w.join();
        }
    }
}
