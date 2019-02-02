extern crate futures;
extern crate tokio_current_thread;
extern crate tokio_io;

use futures::{sync::mpsc, Async, Poll, Sink, Stream};
use std::io::{Error as IOError, ErrorKind, Read, Write};
use tokio_io::{AsyncRead, AsyncWrite};

pub trait WritePipe {
    fn new(sender: mpsc::Sender<Vec<u8>>) -> Self;
}

pub trait ReadPipe {
    fn new(receiver: mpsc::Receiver<Vec<u8>>) -> Self;
}

pub trait IsAsync {}

pub trait IsSync {}

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

impl WritePipe for AsyncWritePipe {
    fn new(sender: mpsc::Sender<Vec<u8>>) -> Self {
        AsyncWritePipe { sender }
    }
}

impl IsAsync for AsyncWritePipe {}

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

impl WritePipe for SyncWritePipe {
    fn new(sender: mpsc::Sender<Vec<u8>>) -> Self {
        SyncWritePipe {
            writer: AsyncWritePipe::new(sender),
        }
    }
}

impl IsSync for SyncWritePipe {}

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

impl ReadPipe for AsyncReadPipe {
    fn new(receiver: mpsc::Receiver<Vec<u8>>) -> Self {
        AsyncReadPipe {
            receiver,
            buf: vec![],
            pos: 0,
        }
    }
}

impl IsAsync for AsyncReadPipe {}

pub struct SyncReadPipe {
    reader: AsyncReadPipe,
}

impl Read for SyncReadPipe {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IOError> {
        let fut = tokio_io::io::read(&mut self.reader, buf);
        tokio_current_thread::block_on_all(fut).map(|(_, _, len)| len)
    }
}

impl ReadPipe for SyncReadPipe {
    fn new(receiver: mpsc::Receiver<Vec<u8>>) -> Self {
        SyncReadPipe {
            reader: AsyncReadPipe::new(receiver),
        }
    }
}

impl IsSync for SyncReadPipe {}

pub fn pipe<W: WritePipe, R: ReadPipe>() -> (W, R) {
    let (sender, receiver) = mpsc::channel(0);
    (W::new(sender), R::new(receiver))
}

pub struct Channel<R: ReadPipe, W: WritePipe> {
    reader: R,
    writer: W,
}

impl<R: ReadPipe + Read, W: WritePipe> Read for Channel<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IOError> {
        self.reader.read(buf)
    }
}

impl<R: ReadPipe, W: Write + WritePipe> Write for Channel<R, W> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IOError> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> Result<(), IOError> {
        self.writer.flush()
    }
}

impl<R: ReadPipe + AsyncRead, W: WritePipe> AsyncRead for Channel<R, W> {}

impl<R: ReadPipe, W: WritePipe + AsyncWrite> AsyncWrite for Channel<R, W> {
    fn shutdown(&mut self) -> Poll<(), IOError> {
        self.writer.shutdown()
    }
}

pub fn channel<FR, FW, SR, SW>() -> (Channel<FR, FW>, Channel<SR, SW>)
where
    FR: ReadPipe,
    FW: WritePipe,
    SR: ReadPipe,
    SW: WritePipe,
{
    let (fst_tx, fst_rx) = mpsc::channel(0);
    let (snd_tx, snd_rx) = mpsc::channel(0);
    (
        Channel {
            reader: FR::new(fst_rx),
            writer: FW::new(snd_tx),
        },
        Channel {
            reader: SR::new(snd_rx),
            writer: SW::new(fst_tx),
        },
    )
}

pub type AsyncChannel = Channel<AsyncReadPipe, AsyncWritePipe>;
pub type SyncChannel = Channel<SyncReadPipe, SyncWritePipe>;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future, Future};
    use std::thread;

    const TEST_WRITE_DATA_A: &[u8] = b"Hello ";
    const TEST_WRITE_DATA_B: &[u8] = b"World";
    const TEST_EXPECT_DATA: &[u8] = b"Hello World";

    fn sync_sender(mut tx: SyncWritePipe) -> thread::JoinHandle<()> {
        thread::spawn(move || {
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

    fn sync_receiver(mut rx: SyncReadPipe) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut buf = Vec::new();
            rx.read_to_end(&mut buf).unwrap();
            assert_eq!(buf, TEST_EXPECT_DATA);
        })
    }

    fn async_sender(tx: AsyncWritePipe) -> impl Future<Item = (), Error = ()> {
        tokio_io::io::write_all(tx, TEST_WRITE_DATA_A)
            .and_then(|(tx, _)| tokio_io::io::write_all(tx, TEST_WRITE_DATA_B))
            .then(|result| {
                assert!(result.is_ok());
                Ok(())
            })
    }

    fn async_receiver(rx: AsyncReadPipe) -> impl Future<Item = (), Error = ()> {
        tokio_io::io::read_to_end(rx, Vec::new()).then(|result| {
            let (_, buf) = result.unwrap();
            assert_eq!(buf, TEST_EXPECT_DATA);
            Ok(())
        })
    }

    fn run_and_wait(
        thds: Vec<thread::JoinHandle<()>>,
        futs: Vec<Box<dyn Future<Item = (), Error = ()>>>,
    ) {
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

    #[test]
    fn normal_pipe() {
        let (tx, rx) = pipe();
        run_and_wait(vec![sync_sender(tx), sync_receiver(rx)], vec![]);
        let (tx, rx) = pipe();
        run_and_wait(vec![sync_sender(tx)], vec![Box::new(async_receiver(rx))]);
        let (tx, rx) = pipe();
        run_and_wait(vec![sync_receiver(rx)], vec![Box::new(async_sender(tx))]);
        let (tx, rx) = pipe();
        run_and_wait(
            vec![],
            vec![Box::new(async_sender(tx)), Box::new(async_receiver(rx))],
        );
    }

    #[test]
    fn zero_read_write_pipe() {
        let (mut tx, mut rx): (SyncWritePipe, SyncReadPipe) = pipe();
        assert_eq!(tx.write(&[]).unwrap(), 0);
        let mut buf = [0u8; 0];
        assert_eq!(rx.read(&mut buf).unwrap(), 0);

        let (mut tx, mut _rx): (AsyncWritePipe, AsyncReadPipe) = pipe();
        assert_eq!(tx.write(&[]).unwrap(), 0);
    }

    #[test]
    fn broken_pipe() {
        let (mut tx, rx): (SyncWritePipe, SyncReadPipe) = pipe();
        drop(rx);
        assert_eq!(tx.write(&[]).unwrap(), 0);
        assert_eq!(
            tx.write(&TEST_EXPECT_DATA).err().unwrap().kind(),
            ErrorKind::WriteZero
        );
        let (tx, mut rx): (SyncWritePipe, SyncReadPipe) = pipe();
        drop(tx);
        let mut buf = [0u8; 1];
        assert_eq!(rx.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn flush_pipe() {
        let (mut tx, mut _rx): (SyncWritePipe, SyncReadPipe) = pipe();
        assert_eq!(tx.flush().unwrap(), ());
    }

    fn sync_send_receive(ch: SyncChannel, reverse: bool) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let send = |mut ch: SyncChannel| {
                assert_eq!(
                    ch.write(TEST_WRITE_DATA_A).unwrap(),
                    TEST_WRITE_DATA_A.len()
                );
                assert_eq!(
                    ch.write(TEST_WRITE_DATA_B).unwrap(),
                    TEST_WRITE_DATA_B.len()
                );
                ch
            };
            let receive = |mut ch: SyncChannel| {
                let mut buf = vec![0u8; TEST_EXPECT_DATA.len()];
                ch.read_exact(&mut buf).unwrap();
                assert_eq!(buf, TEST_EXPECT_DATA);
                ch
            };
            if reverse {
                let ch = receive(ch);
                send(ch);
            } else {
                let ch = send(ch);
                receive(ch);
            }
        })
    }

    fn async_send_receive(ch: AsyncChannel, reverse: bool) -> Box<Future<Item = (), Error = ()>> {
        let send = |tx| {
            tokio_io::io::write_all(tx, TEST_WRITE_DATA_A)
                .and_then(|(tx, _)| tokio_io::io::write_all(tx, TEST_WRITE_DATA_B))
                .then(|result| {
                    let (tx, _) = result.unwrap();
                    Ok(tx)
                })
        };
        let receive = |rx| {
            let buf = vec![0u8; TEST_EXPECT_DATA.len()];
            tokio_io::io::read_exact(rx, buf).then(|result| {
                let (rx, buf) = result.unwrap();
                assert_eq!(buf, TEST_EXPECT_DATA);
                Ok(rx)
            })
        };
        if reverse {
            Box::new(receive(ch).and_then(move |ch| send(ch)).map(|_| ()))
        } else {
            Box::new(send(ch).and_then(move |ch| receive(ch)).map(|_| ()))
        }
    }

    #[test]
    fn normal_channel() {
        let (fst, snd) = channel();
        run_and_wait(
            vec![sync_send_receive(fst, false), sync_send_receive(snd, true)],
            vec![],
        );
        let (fst, snd) = channel();
        run_and_wait(
            vec![sync_send_receive(fst, false)],
            vec![async_send_receive(snd, true)],
        );
        let (fst, snd) = channel();
        run_and_wait(
            vec![sync_send_receive(snd, false)],
            vec![async_send_receive(fst, true)],
        );
        let (fst, snd) = channel();
        run_and_wait(
            vec![],
            vec![
                async_send_receive(fst, false),
                async_send_receive(snd, true),
            ],
        );
    }
}
