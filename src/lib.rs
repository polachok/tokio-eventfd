use std::io::{self, Result};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use libc;

use mio::unix::EventedFd;
use mio::{self, Evented, PollOpt, Ready, Token};

use futures::{try_ready, Async, AsyncSink, Poll, Sink, Stream};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_reactor::PollEvented;

struct Inner(RawFd);

impl Inner {
    fn new(sem: bool) -> Result<Self> {
        let flags = libc::EFD_NONBLOCK | libc::EFD_CLOEXEC;
        let flags = if sem {
            flags | libc::EFD_SEMAPHORE
        } else {
            flags
        };
        let rv = unsafe { libc::eventfd(0, flags) };
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
    pub fn new(semaphore: bool) -> Result<Self> {
        let inner = Inner::new(semaphore)?;
        Ok(EventFd(PollEvented::new(inner)))
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
        let ready = Ready::readable();
        let ready = try_ready!(self.0.poll_read_ready(ready));
        let mut buf = [0; 8];
        match self.poll_read(&mut buf)? {
            Async::NotReady => {
                self.0.clear_read_ready(ready)?;
                Ok(Async::NotReady)
            }
            Async::Ready(_) => Ok(Async::Ready(Some(u64::from_ne_bytes(buf)))),
        }
    }
}

impl Sink for EventFd {
    type SinkItem = u64;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> Result<AsyncSink<Self::SinkItem>> {
        if let Async::NotReady = self.0.poll_write_ready()? {
            return Ok(AsyncSink::NotReady(item));
        }

        match self.poll_write(&item.to_ne_bytes()) {
            Ok(Async::Ready(_)) => Ok(AsyncSink::Ready),
            Ok(Async::NotReady) => {
                self.0.clear_write_ready()?;
                Ok(AsyncSink::NotReady(item))
            }
            Err(err) => Err(err),
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
    use std::time::{Duration, Instant};
    use tokio::prelude::*;
    use tokio::timer::Interval;

    #[test]
    fn increment_many() {
        let writer = EventFd::new(true).unwrap();
        let reader = writer.try_clone().unwrap();

        tokio::run(future::lazy(move || {
            tokio::spawn(
                stream::repeat(1)
                    .take(5)
                    .map_err(|_: ()| std::io::Error::new(std::io::ErrorKind::Other, "timer!"))
                    .forward(writer)
                    .map_err(|err| panic!(err))
                    .map(|_| ()),
            );
            Stream::take(reader, 5)
                .for_each(|x| {
                    println!("{}", x);
                    assert_eq!(x, 1);
                    Ok(())
                })
                .map_err(|err| panic!(err))
        }));
    }

    #[test]
    fn it_works() {
        let writer = EventFd::new(true).unwrap();
        let reader = writer.try_clone().unwrap();

        tokio::run(future::lazy(move || {
            let sender = Interval::new_interval(Duration::from_secs(1))
                .take(5)
                .map(|_| {
                    println!("{:?} sent {}", Instant::now(), 1);
                    1u64
                })
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "timer!"))
                .fuse()
                .forward(writer)
                .map_err(|err| panic!(err))
                .map(|_| ());

            let receiver = Stream::take(reader, 5)
                .for_each(|msg| {
                    println!("{:?} received {}", Instant::now(), msg);
                    assert_eq!(msg, 1);
                    Ok(())
                })
                .map_err(|err| panic!(err));
            receiver.join(sender).map(|_| ()).map_err(|_| ())
        }))
    }
}
