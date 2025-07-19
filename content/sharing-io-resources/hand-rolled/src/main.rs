use bytes::Buf;
use bytes::Bytes;
use bytes::BytesMut;
use rand::Rng;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::io::ErrorKind;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::task::ready;
use tokio::net::{TcpListener, TcpStream};
#[tokio::main]
async fn main() {
    let mut router = Server::new().await;
    router.handle_connections().await;
}

struct Server {
    connections: HashMap<u32, Socket>,
    listener: TcpListener,
}

impl Server {
    async fn new() -> Server {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        Server {
            connections: HashMap::new(),
            listener,
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Poll::Ready(Ok((connection, _))) = self.listener.poll_accept(cx) {
            let mut socket = Socket::new(connection);
            let id: u32 = rand::rng().random();
            socket.send(Bytes::from_owner(id.to_be_bytes()));
            self.connections.insert(id, socket);
            cx.waker().wake_by_ref();
        }

        for conn in self.connections.values_mut() {
            if conn.poll_send(cx).is_ready() {
                cx.waker().wake_by_ref();
            }
        }

        let keys: Vec<_> = self.connections.keys().copied().collect();

        for k in keys {
            let buf = match self.connections.get_mut(&k).unwrap().poll_read(cx) {
                Poll::Ready(Ok(buf)) => buf,
                Poll::Ready(Err(e)) => {
                    self.connections.remove(&k);
                    println!("Failed to read from {k}: {e}");
                    continue;
                }
                Poll::Pending => {
                    continue;
                }
            };

            cx.waker().wake_by_ref();

            let Ok((dest, b)) = parse_message(buf) else {
                continue;
            };

            let Some(destination) = self.connections.get_mut(&dest) else {
                continue;
            };

            destination.send(b);
        }

        Poll::Pending
    }

    async fn handle_connections(&mut self) {
        poll_fn(move |cx| self.poll_next(cx)).await;
    }
}

struct Socket {
    write_buffers: VecDeque<Bytes>,
    read_buffer: BytesMut,
    stream: TcpStream,
    waker: Option<Waker>,
}

impl Socket {
    fn new(stream: TcpStream) -> Socket {
        Socket {
            write_buffers: VecDeque::new(),
            read_buffer: BytesMut::new(),
            waker: None,
            stream,
        }
    }

    fn send(&mut self, buf: Bytes) {
        self.write_buffers.push_back(buf);
        let Some(w) = self.waker.take() else {
            return;
        };

        w.wake_by_ref();
    }

    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(write_buffer) = self.write_buffers.front_mut() else {
            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        };

        loop {
            ready!(self.stream.poll_write_ready(cx))?;

            match self.stream.try_write(write_buffer) {
                Ok(n) => {
                    write_buffer.advance(n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => {
                    return Poll::Ready(Err(e));
                }
            }

            if write_buffer.is_empty() {
                self.write_buffers.pop_front();
            }

            return Poll::Ready(Ok(()));
        }
    }

    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<&mut BytesMut>> {
        loop {
            ready!(self.stream.poll_read_ready(cx))?;
            match self.stream.try_read_buf(&mut self.read_buffer) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => {
                    return Poll::Ready(Err(e));
                }
            };

            return Poll::Ready(Ok(&mut self.read_buffer));
        }
    }
}

struct ParseError;

fn parse_message(message: &mut BytesMut) -> Result<(u32, Bytes), ParseError> {
    if message.len() < 5 {
        return Err(ParseError);
    }

    let end = message[4..]
        .iter()
        .position(|&c| c == b'\0')
        .ok_or(ParseError)?
        + 5;

    let dest: u32 = u32::from_be_bytes(message[..4].try_into().unwrap());

    let data = message.split_to(end).split_off(4).freeze();

    Ok((dest, data))
}

#[cfg(test)]
mod tests {

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    use crate::Server;

    #[tokio::test]
    async fn it_works() {
        const INT_MSG1: &str = "hello, number 2\0";
        const INT_MSG2: &str = "hello back, number 1\0";

        let mut router = Server::new().await;
        tokio::spawn(async move {
            router.handle_connections().await;
        });

        let mut sock1 = TcpStream::connect("127.0.0.1:8080").await.unwrap();
        let mut sock2 = TcpStream::connect("127.0.0.1:8080").await.unwrap();
        let id1 = sock1.read_u32().await.unwrap();
        let id2 = sock2.read_u32().await.unwrap();

        let mut msg1 = Vec::new();
        msg1.extend_from_slice(&id2.to_be_bytes());
        msg1.extend_from_slice(INT_MSG1.as_bytes());

        sock1.write_all(&msg1).await.unwrap();
        sock1.flush().await.unwrap();
        let mut buf = [0; INT_MSG1.len()];
        sock2.read_exact(&mut buf).await.unwrap();

        assert_eq!(str::from_utf8(&buf).unwrap(), INT_MSG1);

        let mut msg2 = Vec::new();
        msg2.extend_from_slice(&id1.to_be_bytes());
        msg2.extend_from_slice(INT_MSG2.as_bytes());

        sock2.write_all(&msg2).await.unwrap();
        sock2.flush().await.unwrap();
        let mut buf = [0; INT_MSG2.len()];
        sock1.read_exact(&mut buf).await.unwrap();

        assert_eq!(str::from_utf8(&buf).unwrap(), INT_MSG2);
    }
}
