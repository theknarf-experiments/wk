//! A pipe between two programs run by [`wk:exec`](crate::exec).
//!
//! `run` hands a child its stdin as a buffer and gives back its stdout as one,
//! which is enough to chain programs but not to *stream* between them: the
//! producer has to finish before the consumer starts. A shell pipeline is the
//! other thing — `seq 1 100000 | head -1` should stop early, and `yes | head`
//! must not buffer forever — and that needs both ends live at once.
//!
//! So this is an ordinary POSIX-shaped pipe: a bounded byte buffer with a
//! reader end and a writer end. Writes block (in the WASI sense: `check_write`
//! reports no room and the pollable parks) while it is full, reads park while
//! it is empty, and the reader sees EOF once the last writer is gone. Both
//! ends are ordinary `wasi:io` streams, so a child receives one as its stdin or
//! stdout with no notion that it is talking to a pipe rather than a file or a
//! terminal — and the guest can hold the other end itself.
//!
//! The halves deliberately do *not* share a runtime: a child runs on its own
//! thread with its own tokio runtime ([`run_program`](crate::plugin)), so the
//! two ends of a pipe are usually being polled from two different runtimes.
//! Everything here is therefore a plain mutex plus stored wakers, with no
//! spawned tasks and no runtime affinity.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use wasmtime_wasi::cli::{IsTerminal, StdinStream, StdoutStream};
use wasmtime_wasi::p2::{InputStream, OutputStream, StreamError, StreamResult};
use wasmtime_wasi_io::async_trait;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;

/// How much a pipe buffers before the writer has to wait. POSIX pipes are
/// famously 64 KiB on Linux; matching that keeps the backpressure point
/// somewhere a shell author would expect.
pub const PIPE_CAPACITY: usize = 64 * 1024;

#[derive(Default)]
struct PipeState {
    buf: VecDeque<u8>,
    /// Ends still able to write. EOF is "empty and none left".
    writers: usize,
    /// Ends still able to read. A write into a pipe nobody will read is a
    /// broken pipe rather than a wait that can never end.
    readers: usize,
    /// Parked reader waiting for bytes or EOF.
    reader_waker: Option<Waker>,
    /// Parked writer waiting for room or for the readers to leave.
    writer_waker: Option<Waker>,
}

impl PipeState {
    fn wake_reader(&mut self) {
        if let Some(w) = self.reader_waker.take() {
            w.wake();
        }
    }
    fn wake_writer(&mut self) {
        if let Some(w) = self.writer_waker.take() {
            w.wake();
        }
    }
}

#[derive(Clone, Default)]
pub struct Pipe(Arc<Mutex<PipeState>>);

impl Pipe {
    /// A new pipe with no ends yet. Take them with [`reader`](Self::reader)
    /// and [`writer`](Self::writer).
    pub fn new() -> Self {
        Pipe(Arc::default())
    }

    /// Take a reading end, as something `WasiCtxBuilder::stdin` accepts.
    pub fn reader(&self) -> PipeReader {
        self.0.lock().unwrap().readers += 1;
        PipeReader(self.clone())
    }

    /// Take a writing end, as something `WasiCtxBuilder::stdout` accepts.
    ///
    /// More than one is allowed and is the point: `cmd 2>&1 | ...` puts a
    /// child's stdout *and* stderr on one pipe, and the reader must not see
    /// EOF until both are gone. Each end is counted, and the last one to drop
    /// closes the pipe.
    pub fn writer(&self) -> PipeWriter {
        self.0.lock().unwrap().writers += 1;
        PipeWriter(self.clone())
    }
}

/// The reading end. Dropping it tells writers nobody is listening.
pub struct PipeReader(Pipe);

impl Drop for PipeReader {
    fn drop(&mut self) {
        let mut s = self.0 .0.lock().unwrap();
        s.readers = s.readers.saturating_sub(1);
        // A writer blocked for room will never get it now; let it fail.
        s.wake_writer();
    }
}

impl IsTerminal for PipeReader {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdinStream for PipeReader {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncRead + Send + Sync> {
        // Unused: component guests take the p2 path.
        Box::new(tokio::io::empty())
    }
    fn p2_stream(&self) -> Box<dyn InputStream> {
        Box::new(ReadEnd(self.0.clone()))
    }
}

/// The writing end. Dropping the last one is what closes the pipe, which is
/// how the reader ever sees EOF.
pub struct PipeWriter(Pipe);

impl Drop for PipeWriter {
    fn drop(&mut self) {
        let mut s = self.0 .0.lock().unwrap();
        s.writers = s.writers.saturating_sub(1);
        if s.writers == 0 {
            // EOF: wake the reader so it observes the close rather than
            // parking forever on a pipe nobody will write to again.
            s.wake_reader();
        }
    }
}

impl IsTerminal for PipeWriter {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for PipeWriter {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(tokio::io::sink())
    }
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(WriteEnd(self.0.clone()))
    }
}

struct ReadEnd(Pipe);

#[async_trait]
impl Pollable for ReadEnd {
    async fn ready(&mut self) {
        Readable(self.0.clone()).await
    }
}

impl InputStream for ReadEnd {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        let mut s = self.0 .0.lock().unwrap();
        if s.buf.is_empty() {
            // Empty with every writer gone is end-of-file; empty with a writer
            // still around just means "not yet", and the pollable parks.
            return if s.writers == 0 {
                Err(StreamError::Closed)
            } else {
                Ok(Bytes::new())
            };
        }
        let n = size.min(s.buf.len());
        let data: Vec<u8> = s.buf.drain(..n).collect();
        // Room freed: a writer waiting on a full pipe can go again.
        s.wake_writer();
        Ok(Bytes::from(data))
    }
}

struct WriteEnd(Pipe);

#[async_trait]
impl Pollable for WriteEnd {
    async fn ready(&mut self) {
        Writable(self.0.clone()).await
    }
}

impl OutputStream for WriteEnd {
    fn check_write(&mut self) -> StreamResult<usize> {
        let s = self.0 .0.lock().unwrap();
        if s.readers == 0 {
            // Nobody will ever drain this. A real shell would take SIGPIPE.
            return Err(StreamError::Closed);
        }
        Ok(PIPE_CAPACITY.saturating_sub(s.buf.len()))
    }

    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        let mut s = self.0 .0.lock().unwrap();
        if s.readers == 0 {
            return Err(StreamError::Closed);
        }
        s.buf.extend(bytes.iter().copied());
        s.wake_reader();
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }
}

/// Resolves once the pipe has bytes to read or has been closed for writing.
struct Readable(Pipe);
impl Future for Readable {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut s = self.0 .0.lock().unwrap();
        if !s.buf.is_empty() || s.writers == 0 {
            Poll::Ready(())
        } else {
            s.reader_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Resolves once the pipe has room, or once there is no reader left (in which
/// case the write should fail rather than wait).
struct Writable(Pipe);
impl Future for Writable {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut s = self.0 .0.lock().unwrap();
        if s.buf.len() < PIPE_CAPACITY || s.readers == 0 {
            Poll::Ready(())
        } else {
            s.writer_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(r: &mut dyn InputStream, n: usize) -> StreamResult<Vec<u8>> {
        Ok(r.read(n)?.to_vec())
    }

    #[test]
    fn bytes_written_come_out_the_other_end() {
        let pipe = Pipe::new();
        let w = pipe.writer();
        let r = pipe.reader();
        let mut wr = w.p2_stream();
        let mut rd = r.p2_stream();

        wr.write(Bytes::from_static(b"hello pipe")).unwrap();
        assert_eq!(read_all(&mut *rd, 64).unwrap(), b"hello pipe");
    }

    /// An empty pipe that still has a writer is "nothing yet", not EOF — the
    /// distinction a reader depends on to keep waiting.
    #[test]
    fn empty_with_a_live_writer_is_not_eof() {
        let pipe = Pipe::new();
        let w = pipe.writer();
        let r = pipe.reader();
        let mut rd = r.p2_stream();
        assert!(read_all(&mut *rd, 64).unwrap().is_empty());
        drop(w);
        assert!(matches!(rd.read(64), Err(StreamError::Closed)));
    }

    /// Buffered bytes survive the writer leaving: EOF is "drained *and*
    /// closed", or a pipeline would lose the producer's tail.
    #[test]
    fn closing_the_writer_still_delivers_buffered_bytes() {
        let pipe = Pipe::new();
        let w = pipe.writer();
        let r = pipe.reader();
        let mut rd = r.p2_stream();
        w.p2_stream().write(Bytes::from_static(b"tail")).unwrap();
        drop(w);
        assert_eq!(read_all(&mut *rd, 64).unwrap(), b"tail");
        assert!(matches!(rd.read(64), Err(StreamError::Closed)));
    }

    /// `2>&1 |`: two writers on one pipe, and EOF only once both have gone.
    #[test]
    fn eof_waits_for_the_last_of_several_writers() {
        let pipe = Pipe::new();
        let out = pipe.writer();
        let err = pipe.writer();
        let r = pipe.reader();
        let mut rd = r.p2_stream();
        out.p2_stream().write(Bytes::from_static(b"o")).unwrap();
        err.p2_stream().write(Bytes::from_static(b"e")).unwrap();
        assert_eq!(read_all(&mut *rd, 64).unwrap(), b"oe");
        drop(out);
        // one writer left: still not EOF
        assert!(read_all(&mut *rd, 64).unwrap().is_empty());
        drop(err);
        assert!(matches!(rd.read(64), Err(StreamError::Closed)));
    }

    #[test]
    fn writing_reports_remaining_room_and_fills_up() {
        let pipe = Pipe::new();
        let _r = pipe.reader();
        let mut wr = pipe.writer().p2_stream();
        assert_eq!(wr.check_write().unwrap(), PIPE_CAPACITY);
        wr.write(Bytes::from(vec![b'x'; PIPE_CAPACITY])).unwrap();
        assert_eq!(wr.check_write().unwrap(), 0);
    }

    /// Writing into a pipe whose reader is gone fails instead of blocking
    /// forever — the moral equivalent of SIGPIPE/EPIPE.
    #[test]
    fn losing_the_reader_closes_the_writer() {
        let pipe = Pipe::new();
        let r = pipe.reader();
        let mut wr = pipe.writer().p2_stream();
        drop(r);
        assert!(matches!(
            wr.write(Bytes::from_static(b"nobody home")),
            Err(StreamError::Closed)
        ));
    }
}
