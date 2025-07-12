use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
#[tokio::main]
async fn main() {
    let router = Arc::new(Server::new().await);
    router.handle_connections().await;
}

struct Server {
    listener: TcpListener,
    connections: tokio::sync::Mutex<Vec<TcpStream>>,
}

impl Server {
    pub async fn new() -> Server {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        Server {
            listener,
            connections: Default::default(),
        }
    }

    pub async fn handle_connections(self: Arc<Self>) {
        loop {
            let (socket, _) = self.listener.accept().await.unwrap();
            let router = Arc::clone(&self);

            tokio::spawn(async move {
                router.handle_connection(socket).await;
            });
        }
    }

    async fn handle_connection(&self, mut socket: TcpStream) {
        let id = self.connections.lock().await.len();
        socket.write_u32(id as u32).await.unwrap();
        socket.flush().await.unwrap();
        self.connections.lock().await.push(socket);

        let mut buffer = BytesMut::new();

        loop {
            let n = self.connections.lock().await[id]
                .read_buf(&mut buffer)
                .await
                .unwrap();
            assert!(n != 0);

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            self.connections.lock().await[dest as usize]
                .write_all(&m)
                .await
                .unwrap();
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

    let mut data = message.split_to(end);

    let dest = data.split_to(4);
    let dest = u32::from_be_bytes(dest[..].try_into().unwrap());

    Ok((dest, data.freeze()))
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    use crate::Server;

    #[tokio::test]
    async fn it_works() {
        const MSG1: &str = "hello, number 2\0";
        const MSG2: &str = "hello back, number 1\0";

        let router = Arc::new(Server::new().await);
        tokio::spawn(router.handle_connections());

        let mut sock1 = TcpStream::connect("127.0.0.1:8080").await.unwrap();
        let mut sock2 = TcpStream::connect("127.0.0.1:8080").await.unwrap();

        let id1 = sock1.read_u32().await.unwrap();
        let id2 = sock2.read_u32().await.unwrap();

        sock1.write_u32(id2).await.unwrap();
        sock1.write_all(MSG1.as_bytes()).await.unwrap();
        sock1.flush().await.unwrap();

        let mut buf = [0; MSG1.len()];
        sock2.read_exact(&mut buf).await.unwrap();

        assert_eq!(str::from_utf8(&buf).unwrap(), MSG1);

        sock2.write_u32(id1).await.unwrap();
        sock2.write_all(MSG2.as_bytes()).await.unwrap();
        sock2.flush().await.unwrap();

        let mut buf = [0; MSG2.len()];
        sock1.read_exact(&mut buf).await.unwrap();

        assert_eq!(str::from_utf8(&buf).unwrap(), MSG2);
    }
}
