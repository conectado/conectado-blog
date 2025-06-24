use bytes::Buf;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use std::future::poll_fn;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::task::ready;
use tokio::net::{TcpListener, TcpStream};
#[tokio::main]
async fn main() {
    let mut router = Router::new().await;
    router.handle_connections().await;
}

struct Router {
    connections: Vec<Socket>,
    listener: TcpListener,
}

impl Router {
    async fn new() -> Router {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        Router {
            connections: vec![],
            listener,
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if let Poll::Ready(Ok((connection, _))) = self.listener.poll_accept(cx) {
                let mut socket = Socket::new(connection);
                let id = self.connections.len() as u32;
                socket.send(&id.to_be_bytes());
                self.connections.push(socket);
                continue;
            }

            for conn in &mut self.connections {
                loop {
                    if conn.poll_send(cx).is_pending() {
                        break;
                    }
                }
            }

            for i in 0..self.connections.len() {
                let Poll::Ready(buf) = self.connections[i].poll_read(cx) else {
                    continue;
                };

                let Ok((dest, b)) = parse_message(buf) else {
                    continue;
                };

                self.connections[dest as usize].send(&b);
            }

            return Poll::Pending;
        }
    }

    async fn handle_connections(&mut self) {
        poll_fn(move |cx| self.poll_next(cx)).await;
    }
}

struct Socket {
    write_buffer: BytesMut,
    read_buffer: BytesMut,
    stream: TcpStream,
    waker: Option<Waker>,
}

impl Socket {
    fn new(stream: TcpStream) -> Socket {
        Socket {
            write_buffer: BytesMut::new(),
            read_buffer: BytesMut::new(),
            waker: None,
            stream,
        }
    }

    fn send(&mut self, buf: &[u8]) {
        self.write_buffer.put_slice(buf);
        let Some(w) = self.waker.take() else {
            return;
        };
        w.wake_by_ref();
    }

    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.write_buffer.is_empty() {
            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        loop {
            ready!(self.stream.poll_write_ready(cx)).unwrap();
            let Ok(n) = self.stream.try_write(&self.write_buffer) else {
                continue;
            };
            self.write_buffer.advance(n);
            return Poll::Ready(());
        }
    }

    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<&mut BytesMut> {
        loop {
            ready!(self.stream.poll_read_ready(cx)).unwrap();
            let Ok(_) = self.stream.try_read_buf(&mut self.read_buffer) else {
                continue;
            };

            return Poll::Ready(&mut self.read_buffer);
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

    use crate::Router;

    #[tokio::test]
    async fn it_works() {
        const INT_MSG1: &str = "hello, number 2\0";
        const INT_MSG2: &str = "hello back, number 1\0";

        let mut router = Router::new().await;
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
