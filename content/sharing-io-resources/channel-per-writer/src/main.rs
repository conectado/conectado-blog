use bytes::Bytes;
use bytes::BytesMut;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
#[tokio::main]
async fn main() {
    let router = Arc::new(Server::new().await);
    router.handle_connections().await;
}

struct Server {
    tx: mpsc::Sender<Message>,
    listener: TcpListener,
}

impl Server {
    pub async fn new() -> Server {
        let (tx, rx) = mpsc::channel(100);
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        tokio::spawn(central_dispatcher(rx));
        Server { tx, listener }
    }

    pub async fn handle_connections(self: Arc<Self>) {
        loop {
            let (socket, _) = self.listener.accept().await.unwrap();
            let router = self.clone();

            tokio::spawn(async move {
                router.handle_connection(socket).await;
            });
        }
    }

    async fn handle_connection(&self, socket: TcpStream) {
        let id: u32 = rand::rng().random();
        let (mut read, write) = socket.into_split();
        self.tx.send(Message::New(id, write)).await.unwrap();

        let mut buffer = BytesMut::new();

        loop {
            let Ok(n) = read.read_buf(&mut buffer).await else {
                let _ = self.tx.send(Message::Disconnect(id)).await;
                break;
            };

            if n == 0 {
                let _ = self.tx.send(Message::Disconnect(id)).await;
                break;
            }

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            self.tx.send(Message::Send(dest, m)).await.unwrap();
        }
    }
}

async fn central_dispatcher(mut rx: mpsc::Receiver<Message>) {
    let mut connections = HashMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            Message::New(id, connection) => {
                let (tx, rx) = mpsc::channel(100);

                let handle = tokio::spawn(client_dispatcher(connection, rx)).abort_handle();
                if let Err(e) = tx.send(Bytes::from_owner(id.to_be_bytes())).await {
                    eprintln!("Failed to write to socket: {e}");
                }
                connections.insert(id, (tx, handle));
            }
            Message::Send(id, items) => {
                let Some((connection, _)) = connections.get_mut(&id) else {
                    continue;
                };
                if let Err(e) = connection.send(items).await {
                    eprintln!("Failed to write to socket: {e}");
                }
            }
            Message::Disconnect(id) => {
                if let Some((_, handle)) = connections.remove(&id) {
                    handle.abort();
                }
            }
        }
    }
}

async fn client_dispatcher(mut connection: OwnedWriteHalf, mut rx: mpsc::Receiver<Bytes>) {
    loop {
        let m = rx.recv().await.unwrap();
        if let Err(e) = connection.write_all(&m).await {
            eprintln!("Failed to write to socket: {e}");
            break;
        }
    }
}

enum Message {
    New(u32, OwnedWriteHalf),
    Send(u32, Bytes),
    Disconnect(u32),
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

    use std::sync::Arc;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    use crate::Server;

    #[tokio::test]
    async fn it_works() {
        const INT_MSG1: &str = "hello, number 2\0";
        const INT_MSG2: &str = "hello back, number 1\0";

        let router = Arc::new(Server::new().await);
        tokio::spawn(async { router.handle_connections().await });

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
