extern crate futures;
extern crate tokio_current_thread;
extern crate tokio_io;

use futures::{sync::mpsc, Async, Poll, Sink, Stream};
use std::io::{Error as IOError, ErrorKind, Read, Write};
use tokio_io::{AsyncRead, AsyncWrite};

pub struct AsyncWritePipe {
    sender: mpsc::Sender<Vec<u8>>,
}

impl AsyncWritePipe {
    fn inner_flush(&mut self) -> Poll<(), IOError> {
        self.sender
            .poll_complete()
            .map_err(|err| IOError::new(ErrorKind::BrokenPipe, err))
    }
}

impl Write for AsyncWritePipe {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IOError> {
        if self.sender.is_closed() {
            return Ok(0);
        }
        let len = buf.len();
        if len == 0 {
            return Ok(0);
        }
        self.sender
            .start_send(buf.to_vec())
            .map_err(|err| IOError::new(ErrorKind::BrokenPipe, err))
            .and_then(|ret| {
                if ret.is_not_ready() {
                    Err(IOError::new(ErrorKind::WouldBlock, ""))
                } else {
                    Ok(len)
                }
            })
    }

    fn flush(&mut self) -> Result<(), IOError> {
        self.inner_flush().and_then(|ret| {
            if ret.is_not_ready() {
                Err(IOError::new(ErrorKind::WouldBlock, ""))
            } else {
                Ok(())
            }
        })
    }
}

impl AsyncWrite for AsyncWritePipe {
    fn shutdown(&mut self) -> Poll<(), IOError> {
        self.inner_flush()
    }
}

pub struct SyncWritePipe {
    writer: AsyncWritePipe,
}

impl Write for SyncWritePipe {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IOError> {
        let fut = tokio_io::io::write_all(&mut self.writer, buf);
        tokio_current_thread::block_on_all(fut).map(|_| buf.len())
    }

    fn flush(&mut self) -> Result<(), IOError> {
        let fut = tokio_io::io::flush(&mut self.writer);
        tokio_current_thread::block_on_all(fut).map(|_| ())
    }
}

pub struct AsyncReadPipe {
    receiver: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for AsyncReadPipe {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IOError> {
        if self.pos == self.buf.len() {
            self.buf = match self.receiver.poll() {
                Ok(Async::Ready(Some(data))) => data,
                Ok(Async::Ready(None)) => return Ok(0),
                Ok(Async::NotReady) => {
                    return if buf.len() == 0 {
                        Ok(0)
                    } else {
                        Err(IOError::new(ErrorKind::WouldBlock, ""))
                    };
                }
                Err(_) => return Err(IOError::new(ErrorKind::BrokenPipe, "")),
            };
            self.pos = 0;
        }
        let ret_len = (self.buf.len() - self.pos).min(buf.len());
        buf[..ret_len].clone_from_slice(&self.buf[self.pos..(self.pos + ret_len)]);
        self.pos += ret_len;
        return Ok(ret_len);
    }
}

impl AsyncRead for AsyncReadPipe {}

pub struct SyncReadPipe {
    reader: AsyncReadPipe,
}

impl Read for SyncReadPipe {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IOError> {
        let fut = tokio_io::io::read(&mut self.reader, buf);
        tokio_current_thread::block_on_all(fut).map(|(_, _, len)| len)
    }
}

pub struct WritePipeBuilder {
    sender: mpsc::Sender<Vec<u8>>,
}

impl WritePipeBuilder {
    pub fn into_async(self) -> AsyncWritePipe {
        AsyncWritePipe {
            sender: self.sender,
        }
    }

    pub fn into_sync(self) -> SyncWritePipe {
        SyncWritePipe {
            writer: self.into_async(),
        }
    }
}

pub struct ReadPipeBuilder {
    receiver: mpsc::Receiver<Vec<u8>>,
}

impl ReadPipeBuilder {
    pub fn into_async(self) -> AsyncReadPipe {
        AsyncReadPipe {
            receiver: self.receiver,
            buf: vec![],
            pos: 0,
        }
    }

    pub fn into_sync(self) -> SyncReadPipe {
        SyncReadPipe {
            reader: self.into_async(),
        }
    }
}

pub fn pipe() -> (WritePipeBuilder, ReadPipeBuilder) {
    let (sender, receiver) = mpsc::channel(0);
    (WritePipeBuilder { sender }, ReadPipeBuilder { receiver })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future, Future};
    use std::thread;

    const TEST_WRITE_DATA_A: &[u8] = b"Hello ";
    const TEST_WRITE_DATA_B: &[u8] = b"World";
    const TEST_EXPECT_DATA: &[u8] = b"Hello World";

    fn sync_sender(tx: WritePipeBuilder) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut tx = tx.into_sync();
            assert_eq!(
                tx.write(TEST_WRITE_DATA_A).unwrap(),
                TEST_WRITE_DATA_A.len()
            );
            assert_eq!(
                tx.write(TEST_WRITE_DATA_B).unwrap(),
                TEST_WRITE_DATA_B.len()
            );
        })
    }

    fn sync_receiver(rx: ReadPipeBuilder) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut rx = rx.into_sync();
            let mut buf = Vec::new();
            rx.read_to_end(&mut buf).unwrap();
            assert_eq!(buf, TEST_EXPECT_DATA);
        })
    }

    fn async_sender(tx: WritePipeBuilder) -> impl Future<Item = (), Error = ()> {
        let tx = tx.into_async();
        tokio_io::io::write_all(tx, TEST_WRITE_DATA_A)
            .and_then(|(tx, _)| tokio_io::io::write_all(tx, TEST_WRITE_DATA_B))
            .then(|result| {
                assert!(result.is_ok());
                Ok(())
            })
    }

    fn async_receiver(rx: ReadPipeBuilder) -> impl Future<Item = (), Error = ()> {
        let rx = rx.into_async();
        tokio_io::io::read_to_end(rx, Vec::new()).then(|result| {
            let (_, buf) = result.unwrap();
            assert_eq!(buf, TEST_EXPECT_DATA);
            Ok(())
        })
    }

    #[test]
    fn normal_pipe() {
        for &sync_tx in &[true, false] {
            for &sync_rx in &[true, false] {
                let mut thds = Vec::new();
                let mut futs: Vec<Box<dyn Future<Item = (), Error = ()>>> = Vec::new();
                let (tx, rx) = pipe();
                if sync_tx {
                    thds.push(sync_sender(tx));
                } else {
                    futs.push(Box::new(async_sender(tx)));
                }
                if sync_rx {
                    thds.push(sync_receiver(rx));
                } else {
                    futs.push(Box::new(async_receiver(rx)));
                }
                tokio_current_thread::block_on_all(future::lazy(|| {
                    for fut in futs {
                        tokio_current_thread::spawn(fut);
                    }
                    future::ok::<(), ()>(())
                }))
                .unwrap();
                for thd in thds {
                    thd.join().unwrap();
                }
            }
        }
    }
}
