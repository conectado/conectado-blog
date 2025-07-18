use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use futures_concurrency::future::Race;
use rand::Rng;
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() {
    let mut router = Server::new().await;
    router.handle_connections().await;
}

struct Server {
    read_connections: HashMap<u32, ReadSocket>,
    write_connections: HashMap<u32, WriteSocket>,
    listener: TcpListener,
}

#[derive(Debug)]
enum Event<'a> {
    Read(&'a mut BytesMut),
    Connection(TcpStream),
}

impl Server {
    async fn new() -> Server {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        Server {
            read_connections: HashMap::new(),
            write_connections: HashMap::new(),
            listener,
        }
    }

    fn next_event(&mut self) -> impl Future<Output = Result<Event>> {
        let listen = self
            .listener
            .accept()
            .map(|stream| Ok(Event::Connection(stream.unwrap().0)));

        // Neither `Race` nor `select_all` works with empty vectors
        if self.read_connections.is_empty() {
            return listen.boxed();
        }

        let reads = self
            .read_connections
            .values_mut()
            .map(|reader| reader.read())
            .collect::<Vec<_>>()
            .race();

        let writes = self
            .write_connections
            .values_mut()
            .map(|write| write.advance_send())
            .collect::<Vec<_>>()
            .race();

        (listen, reads, writes).race().boxed()
    }

    pub async fn handle_connections(&mut self) {
        loop {
            let ev = self.next_event().await;
            let ev = match ev {
                Ok(ev) => ev,
                Err((e, i)) => {
                    eprintln!("Socket error: {e}");
                    self.read_connections.remove(&i);
                    self.write_connections.remove(&i);
                    continue;
                }
            };

            match ev {
                Event::Read(bytes_mut) => {
                    let Ok((i, d)) = parse_message(bytes_mut) else {
                        continue;
                    };
                    let Some(writer) = self.write_connections.get_mut(&i) else {
                        continue;
                    };
                    writer.send(d);
                }
                Event::Connection(tcp_stream) => {
                    let id = rand::rng().random();
                    let (r, w) = tcp_stream.into_split();
                    let mut write_sock = WriteSocket::new(w, id);
                    write_sock.send(Bytes::from_owner(id.to_be_bytes()));
                    self.read_connections.insert(id, ReadSocket::new(r, id));
                    self.write_connections.insert(id, write_sock);
                }
            }
        }
    }
}

type Result<T> = std::result::Result<T, (io::Error, u32)>;

struct ReadSocket {
    buffer: BytesMut,
    reader: OwnedReadHalf,
    id: u32,
}

impl ReadSocket {
    fn new(reader: OwnedReadHalf, id: u32) -> ReadSocket {
        ReadSocket {
            buffer: BytesMut::new(),
            reader,
            id,
        }
    }

    async fn read(&mut self) -> Result<Event> {
        self.reader
            .read_buf(&mut self.buffer)
            .await
            .map_err(|e| (e, self.id))?;
        Ok(Event::Read(&mut self.buffer))
    }
}

struct WriteSocket {
    buffers: VecDeque<Bytes>,
    writer: OwnedWriteHalf,
    id: u32,
}

impl WriteSocket {
    fn new(writer: OwnedWriteHalf, id: u32) -> WriteSocket {
        WriteSocket {
            buffers: VecDeque::new(),
            writer,
            id,
        }
    }

    async fn advance_send(&mut self) -> Result<Event> {
        loop {
            let Some(buffer) = self.buffers.front_mut() else {
                std::future::pending::<Infallible>().await;
                unreachable!();
            };

            self.writer
                .write_all_buf(buffer)
                .await
                .map_err(|e| (e, self.id))?;

            self.buffers.pop_front();
        }
    }

    fn send(&mut self, buf: Bytes) {
        self.buffers.push_back(buf);
    }
}

struct ParseError;

fn parse_message(message: &mut BytesMut) -> std::result::Result<(u32, Bytes), ParseError> {
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
