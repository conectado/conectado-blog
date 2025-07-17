use bytes::Buf;
use bytes::BytesMut;
use std::future::poll_fn;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use tokio::net::{TcpListener, TcpStream};
#[tokio::main]
async fn main() {
    let mut router = Server::new().await;
    router.handle_connections().await;
}

struct Server {
    connections: Vec<Socket>,
    listener: TcpListener,
}

impl Server {
    async fn new() -> Server {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        Server {
            connections: vec![],
            listener,
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Poll::Ready(Ok((connection, _))) = self.listener.poll_accept(cx) {
            let id = self.connections.len() as u32;
            let socket = Socket::new(connection, id);
            self.connections.push(socket);
            cx.waker().wake_by_ref();
        }

        for i in 0..self.connections.len() {
            let (connections_left, c) = self.connections.split_at_mut(i);
            let (connection, connections_right) = c.split_at_mut(1);
            let connection = connection.first_mut().unwrap();

            connection.poll_id_message(cx);

            if connection.poll_read(cx).is_ready() {
                // If we're ready we want to immediatley reschedule ourselves
                cx.waker().wake_by_ref();
            };

            let Some((dest, message)) = connection.poll_next_message() else {
                continue;
            };

            let dest = dest as usize;

            let connection_dest = if dest > i {
                &connections_right[dest - i - 1]
            } else {
                &connections_left[dest]
            };

            let Poll::Ready(message_end) = connection_dest.poll_send(cx, message) else {
                continue;
            };

            if message_end {
                connection.pending_message_to.take();
            }

            cx.waker().wake_by_ref();
        }

        Poll::Pending
    }

    async fn handle_connections(&mut self) {
        poll_fn(move |cx| self.poll_next(cx)).await;
    }
}

struct Socket {
    buffer: BytesMut,
    id_message: BytesMut,
    stream: TcpStream,
    pending_message_to: Option<u32>,
}

impl Socket {
    fn new(stream: TcpStream, id: u32) -> Socket {
        Socket {
            buffer: BytesMut::new(),
            pending_message_to: None,
            stream,
            id_message: BytesMut::from(&id.to_be_bytes()[..]),
        }
    }

    fn poll_id_message(&mut self, cx: &mut Context<'_>) {
        loop {
            // As long as we're able we try to send all the id message bytes without a lot of attention for fairness
            if self.id_message.is_empty() {
                return;
            }

            if self.stream.poll_write_ready(cx).is_pending() {
                return;
            }

            let Ok(n) = self.stream.try_write(&self.id_message) else {
                continue;
            };

            self.id_message.advance(n);
        }
    }

    fn poll_next_message(&mut self) -> Option<(u32, &mut BytesMut)> {
        let dest = self.pending_message_to?;

        if self.buffer.is_empty() {
            return None;
        }

        Some((dest, &mut self.buffer))
    }

    fn poll_send(&self, cx: &mut Context<'_>, b: &mut BytesMut) -> Poll<bool> {
        loop {
            ready!(self.stream.poll_write_ready(cx)).unwrap();
            let Ok(n) = self.stream.try_write(message_bytes(b)) else {
                continue;
            };

            let message_end = b[..n].contains(&b'\0');

            b.advance(n);

            return Poll::Ready(message_end);
        }
    }

    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            ready!(self.stream.poll_read_ready(cx)).unwrap();
            let Ok(_) = self.stream.try_read_buf(&mut self.buffer) else {
                continue;
            };

            if self.pending_message_to.is_some() {
                return Poll::Ready(());
            }

            // Either we leave this function
            if let Ok(dest) = data_dest(&self.buffer) {
                self.buffer.advance(4);
                self.pending_message_to = Some(dest);
                return Poll::Ready(());
            } else {
                continue;
            }
        }
    }
}

struct ParseError;

fn data_dest(message: &[u8]) -> Result<u32, ParseError> {
    if message.len() < 5 {
        return Err(ParseError);
    }

    Ok(u32::from_be_bytes(message[..4].try_into().unwrap()))
}

fn message_bytes(buffer: &BytesMut) -> &[u8] {
    let Some(end) = buffer.iter().position(|&c| c == b'\0') else {
        return &buffer[..];
    };

    &buffer[..=end]
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
