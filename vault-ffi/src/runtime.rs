// SPDX-License-Identifier: MIT
//! The tokio runtime owned by the factory (shared with its handles).
//!
//! Every exported async call runs its body as a task on this runtime and
//! awaits the task's handle, so the future UniFFI polls (from Kotlin's
//! coroutine machinery) needs no tokio context of its own. Cancelling the
//! Kotlin coroutine drops that future, which aborts the task.

use std::future::Future;
use std::sync::Arc;

use tokio::runtime::{Builder, Handle, Runtime};

use crate::error::FfiError;

pub(crate) struct OwnedRuntime {
    rt: Option<Runtime>,
    handle: Handle,
}

impl OwnedRuntime {
    pub(crate) fn new() -> Result<Arc<Self>, FfiError> {
        let rt = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("vault-ffi")
            .enable_all()
            .build()
            .map_err(|e| FfiError::internal(format!("tokio runtime: {}", e.kind())))?;
        let handle = rt.handle().clone();
        Ok(Arc::new(Self {
            rt: Some(rt),
            handle,
        }))
    }

    pub(crate) fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Run `fut` on the runtime and wait for it; dropping the returned future
    /// aborts the task.
    pub(crate) async fn run<T, F>(&self, fut: F) -> Result<T, FfiError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, FfiError>> + Send + 'static,
    {
        let task = self.handle.spawn(fut);
        let _abort = AbortOnDrop(task.abort_handle());
        match task.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Err(FfiError::internal("cancelled")),
            Err(_) => Err(FfiError::internal("task panicked")),
        }
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        // The last owner may be dropped on one of the runtime's own threads
        // (e.g. a handle released inside a task): never block there.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
