//! A small bounded worker pool. Callbacks enqueue
//! work here and return, keeping fuser's serial dispatch loop free to answer a
//! `FLUSH` (the fork/exec deadlock; see the crate docs). The number of OS
//! threads is fixed, so this is **not** a thread-per-callback design.

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
                    // mount). At worst that one callback's FUSE reply is dropped;
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
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
