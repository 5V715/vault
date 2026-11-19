// SPDX-License-Identifier: MIT
//! I/O progress tracking for plain HTTP connections: a connection is cut when
//! neither a read nor a write made progress for the stall limit (a stalled or
//! vanished reader), however long it lives in total. Slow clients that keep
//! reading, and keep-alive connections that a reverse proxy reuses for many
//! requests, are not affected.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// Time of the last successful read or write, shared with the watchdog.
#[derive(Clone)]
pub(crate) struct Progress {
    start: Instant,
    /// Milliseconds since `start`.
    last_ms: Arc<AtomicU64>,
}

impl Progress {
    pub(crate) fn new() -> Self {
        Self {
            start: Instant::now(),
            last_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.fetch_max(ms, Ordering::Relaxed);
    }

    fn last(&self) -> Instant {
        self.start + Duration::from_millis(self.last_ms.load(Ordering::Relaxed))
    }

    /// Resolves once no progress was made for `limit`.
    pub(crate) async fn stalled(&self, limit: Duration) {
        loop {
            let deadline = self.last() + limit;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// Wraps an I/O stream and records every successful read and write.
pub(crate) struct Tracked<S> {
    inner: S,
    progress: Progress,
}

impl<S> Tracked<S> {
    pub(crate) fn new(inner: S, progress: Progress) -> Self {
        Self { inner, progress }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Tracked<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.progress.touch();
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tracked<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.progress.touch();
        }
        r
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            self.progress.touch();
        }
        r
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
