//! This crate provides eventfd file-like objects support for tokio.
//! eventfd object can be used as an event
//! wait/notify mechanism by user-space applications, and by
//! the kernel to notify user-space applications of events.
//! The object contains an unsigned 64-bit integer counter
//! that is maintained by the kernel.
use std::io::{self, Read, Result, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_lite::ready;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct Inner(RawFd);

impl Inner {
    fn new(init: u32, is_semaphore: bool) -> Result<Self> {
        let flags = libc::EFD_NONBLOCK | libc::EFD_CLOEXEC;
        let flags = if is_semaphore {
            flags | libc::EFD_SEMAPHORE
        } else {
            flags
        };
        let rv = unsafe { libc::eventfd(init, flags) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Inner(rv))
    }

    fn try_clone(&self) -> Result<Self> {
        let rv = unsafe { libc::dup(self.0) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Inner(rv))
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

impl AsRawFd for Inner {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl<'a> io::Read for &'a Inner {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let rv =
            unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut std::ffi::c_void, buf.len()) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rv as usize)
    }
}

impl<'a> io::Write for &'a Inner {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let rv = unsafe { libc::write(self.0, buf.as_ptr() as *const std::ffi::c_void, buf.len()) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rv as usize)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct EventFd(AsyncFd<Inner>);

impl EventFd {
    /// Create new Eventfd. `init` is the initial value of the counter
    /// `is_semaphore` determines eventfd behaviour:
    ///   - if true and counter has non-zero value read returns 8 bytes containing the value 1,
    ///   and the counter's value is decremented by 1
    ///   - if false and counter has non-zero value read returns the value and the counter's value
    ///   is reset to 0.
    pub fn new(init: u32, is_semaphore: bool) -> Result<Self> {
        let inner = Inner::new(init, is_semaphore)?;
        Ok(EventFd(AsyncFd::new(inner)?))
    }

    pub fn try_clone(&self) -> Result<Self> {
        let inner = self.0.get_ref().try_clone()?;
        Ok(EventFd(AsyncFd::new(inner)?))
    }
}

impl AsRawFd for EventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.get_ref().0
    }
}

impl FromRawFd for EventFd {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        EventFd(AsyncFd::new(Inner(fd)).unwrap())
    }
}

impl AsyncRead for EventFd {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        let mut guard = ready!(self.0.poll_read_ready(cx))?;

        let count = match guard.try_io(|inner| {
            let buf = unsafe {
                &mut *(buf.unfilled_mut() as *mut [std::mem::MaybeUninit<u8>] as *mut [u8])
            };
            inner.get_ref().read(buf)
        }) {
            Ok(result) => result?,
            Err(_) => return Poll::Pending,
        };
        unsafe { buf.assume_init(count) };
        buf.advance(count);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for EventFd {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;

            match guard.try_io(|inner| inner.get_ref().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::sleep;

    #[tokio::test]
    async fn not_semaphore_reads_and_resets() {
        const VALUE: u64 = 42;

        let mut writer = EventFd::new(0, false).unwrap();
        let mut reader = writer.try_clone().unwrap();

        writer.write(&VALUE.to_ne_bytes()).await.unwrap();
        let mut buf = [0; 8];
        reader.read(&mut buf).await.unwrap();
        assert_eq!(buf, VALUE.to_ne_bytes());

        // check it blocks on zero
        let delay = sleep(Duration::from_secs(1));
        let read_should_block = reader.read(&mut buf);
        tokio::select! {
            _ = delay => {},
            val = read_should_block => {
                panic!("{:?}", val)
            },
        }
    }

    #[tokio::test]
    async fn semaphore_reads_ones() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const VALUE: u64 = 42;

        let mut writer = EventFd::new(0, true).unwrap();
        let mut reader = writer.try_clone().unwrap();

        writer.write(&VALUE.to_ne_bytes()).await.unwrap();
        let mut buf = [0; 8];
        for _ in 0..VALUE {
            reader.read(&mut buf).await.unwrap();
            assert_eq!(buf, 1u64.to_ne_bytes());
        }

        // check it blocks on zero
        let delay = sleep(Duration::from_secs(1));
        let read_should_block = reader.read(&mut buf);
        tokio::select! {
            _ = delay => {},
            val = read_should_block => {
                panic!("{:?}", val)
            },
        }
    }
}
