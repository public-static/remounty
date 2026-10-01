//! Runs mount operations one at a time on a dedicated thread.

use std::sync::mpsc::{self, Sender};
use std::thread;

use crate::deps::Dependencies;
use crate::error::{Error, Result};
use crate::ops::{self, Operation, Report};

struct Job {
    op: Operation,
    deps: Dependencies,
}

pub struct Worker {
    tx: Sender<Job>,
}

impl Worker {
    pub fn start(on_done: impl Fn(Operation, Report) + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("operations".into())
            .spawn(move || {
                for job in rx {
                    let ctx = ops::Context { deps: &job.deps };
                    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        ops::execute(&job.op, &ctx)
                    }))
                    .unwrap_or_else(|_| Report::Failed {
                        title: "Internal error".into(),
                        detail: "The operation stopped unexpectedly. Please check the volume's \
                                 state in the menu and the log for details."
                            .into(),
                    });
                    on_done(job.op, report);
                }
            })
            .map_err(|err| Error::new(format!("Could not start worker thread: {err}")))?;
        Ok(Self { tx })
    }

    pub fn submit(&self, op: Operation, deps: Dependencies) -> Result<()> {
        self.tx
            .send(Job { op, deps })
            .map_err(|_| Error::new("The operation thread is not running"))
    }
}
