use std::io::{self, Result};
use std::os::unix::io::{RawFd, AsRawFd, FromRawFd};

use libc;

use mio::unix::EventedFd;
use mio::{self, Evented, PollOpt, Ready, Token};

use futures::{Async, Poll, Sink, Stream, AsyncSink, try_ready};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_reactor::PollEvented;

struct Inner(RawFd);

impl Inner {
    fn new() -> Result<Self> {
        let rv = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
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

impl io::Read for Inner {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let rv =
            unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut std::ffi::c_void, buf.len()) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rv as usize)
    }
}

impl io::Write for Inner {
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

impl Evented for Inner {
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> Result<()> {
        poll.register(&EventedFd(&self.0), token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> Result<()> {
        poll.reregister(&EventedFd(&self.0), token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> Result<()> {
        poll.deregister(&EventedFd(&self.0))
    }
}

pub struct EventFd(PollEvented<Inner>);

impl EventFd {
    pub fn new() -> Result<Self> {
        let inner = Inner::new()?;
        Ok(EventFd(PollEvented::new(inner)))
    }

    pub fn poll_write_ready(&self) -> Poll<Ready, io::Error> {
        self.0.poll_write_ready()
    }

    pub fn clear_write_ready(&self) -> Result<()> {
        self.0.clear_write_ready()
    }

    pub fn poll_read_ready(&self, mask: Ready) -> Poll<Ready, io::Error> {
        self.0.poll_read_ready(mask)
    }

    pub fn clear_read_ready(&self, mask: Ready) -> Result<()> {
        self.0.clear_read_ready(mask)
    }

    pub fn try_clone(&self) -> Result<Self> {
        let inner = self.0.get_ref().try_clone()?;
        Ok(EventFd(PollEvented::new(inner)))
    }
}

impl Stream for EventFd {
    type Item = u64;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut buf = [0; 8];
        try_ready!(self.poll_read(&mut buf));
        Ok(Async::Ready(Some(u64::from_ne_bytes(buf))))
    }
}

impl Sink for EventFd {
    type SinkItem = u64;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> Result<AsyncSink<Self::SinkItem>> {
        match self.poll_write(&item.to_ne_bytes()) {
            Ok(Async::Ready(_)) => Ok(AsyncSink::Ready),
            Ok(Async::NotReady) => Ok(AsyncSink::NotReady(item)),
            Err(err) => Err(err)
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}

impl AsRawFd for EventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.get_ref().0
    }
}

impl FromRawFd for EventFd {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        EventFd(PollEvented::new(Inner(fd)))
    }
}

impl io::Read for EventFd {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.0.read(buf)
    }
}

impl AsyncRead for EventFd {
    fn poll_read(&mut self, buf: &mut [u8]) -> Poll<usize, io::Error> {
        self.0.poll_read(buf)
    }
}

impl io::Write for EventFd {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush()
    }
}

impl AsyncWrite for EventFd {
    fn poll_write(&mut self, buf: &[u8]) -> Poll<usize, io::Error> {
        self.0.poll_write(buf)
    }

    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(Async::Ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, Duration};
    use tokio::prelude::*;
    use tokio::timer::Interval;

    #[test]
    fn it_works() {
        let writer = EventFd::new().unwrap();
        let reader = writer.try_clone().unwrap();

        tokio::run(future::lazy(|| {
            tokio::spawn(future::lazy(|| {
               reader 
                    .for_each(|msg| {
                        println!("{:?} received {}", Instant::now(), msg);
                        Ok(())
                    })
                    .map_err(|err| panic!(err))
                }));
            Interval::new_interval(Duration::from_secs(1))
                .map(|_| {
                    println!("{:?} sent {}", Instant::now(), 1);
                    1u64
                })
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "timer!"))
                .forward(writer)
                .map_err(|err| panic!(err))
                .map(|_| ())

        }))
    }
}
