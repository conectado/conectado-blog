use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{FutureExt, future::select_all};
use futures_concurrency::future::Race;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() {
    let mut router = Router::new().await;
    router.handle_connections().await;
}

struct Router {
    read_connections: Vec<ReadSocket>,
    write_connections: Vec<WriteSocket>,
    listener: TcpListener,
}

#[derive(Debug)]
enum Event<'a> {
    Read(&'a mut BytesMut),
    Connection(TcpStream),
}

impl Router {
    async fn new() -> Router {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        Router {
            read_connections: vec![],
            write_connections: vec![],
            listener,
        }
    }

    async fn handle_connections(&mut self) {
        loop {
            let s3 = self
                .listener
                .accept()
                .boxed()
                .map(|r| Event::Connection(r.unwrap().0));

            let ev = if self.read_connections.is_empty() || self.write_connections.is_empty() {
                s3.await
            } else {
                let s1 = select_all(self.read_connections.iter_mut().map(|c| c.read().boxed()))
                    .map(|(d, ..)| Event::Read(d));
                let s2 = select_all(
                    self.write_connections
                        .iter_mut()
                        .map(|c| c.advance_send().boxed()),
                )
                .map(|(e, _, _)| e);
                (s1, s2, s3).race().await
            };
            match ev {
                Event::Read(bytes_mut) => {
                    let Ok((i, d)) = parse_message(bytes_mut) else {
                        continue;
                    };
                    self.write_connections[i as usize].send(&d);
                }
                Event::Connection(tcp_stream) => {
                    let (r, w) = tcp_stream.into_split();
                    let mut write_sock = WriteSocket::new(w);
                    write_sock.send(&(self.write_connections.len() as u32).to_be_bytes());
                    self.read_connections.push(ReadSocket::new(r));
                    self.write_connections.push(write_sock);
                }
            }
        }
    }
}

struct ReadSocket {
    buffer: BytesMut,
    reader: OwnedReadHalf,
}

impl ReadSocket {
    fn new(reader: OwnedReadHalf) -> ReadSocket {
        ReadSocket {
            buffer: BytesMut::new(),
            reader,
        }
    }

    async fn read(&mut self) -> &mut BytesMut {
        self.reader.read_buf(&mut self.buffer).await.unwrap();
        &mut self.buffer
    }
}

struct WriteSocket {
    buffer: BytesMut,
    writer: OwnedWriteHalf,
}

impl WriteSocket {
    fn new(writer: OwnedWriteHalf) -> WriteSocket {
        WriteSocket {
            buffer: BytesMut::new(),
            writer,
        }
    }

    async fn advance_send(&mut self) -> Event {
        loop {
            if !self.buffer.has_remaining() {
                std::future::pending::<Event>().await;
            }

            self.writer.write_all_buf(&mut self.buffer).await.unwrap();
        }
    }

    fn send(&mut self, buf: &[u8]) {
        self.buffer.put_slice(buf);
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

    let mut data = message.split_to(end);

    let dest = data.split_to(4);
    let dest = u32::from_be_bytes(dest[..].try_into().unwrap());

    Ok((dest, data.freeze()))
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
