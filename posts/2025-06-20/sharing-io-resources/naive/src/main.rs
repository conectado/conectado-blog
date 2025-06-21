use std::sync::Arc;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
#[tokio::main]
async fn main() {
    // Shared state: mapping from number to TCP stream let connections: Arc<Mutex<Vec<Arc<TcpStream>>>> = Arc::new(Mutex::new(Vec::new())); let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap(); println!("Server listening on 127.0.0.1:8080");
    let router = Arc::new(Router::default());

    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    println!("Server listening on 127.0.0.1:8080");

    loop {
        let (socket, addr) = listener.accept().await.unwrap();
        println!("New connection from {}", addr);

        let router = router.clone();

        // Spawn a task to handle this connection
        tokio::spawn(async move {
            router.handle_connection(socket).await;
        });
    }
}

#[derive(Default)]
struct Router {
    connections: Mutex<Vec<Arc<tokio::sync::Mutex<TcpStream>>>>,
}

impl Router {
    async fn handle_connection(&self, socket: TcpStream) {
        let socket = Arc::new(tokio::sync::Mutex::new(socket));
        let id = {
            let mut connections = self.connections.lock().unwrap();
            connections.push(socket.clone());
            connections.len()
        };

        socket.lock().await.write_u32(id as u32).await.unwrap();
        socket.lock().await.flush().await.unwrap();

        let mut buffer = Vec::new();

        loop {
            let n = socket.lock().await.read(&mut buffer).await.unwrap();
            assert!(n != 0);

            let id: u32 = u32::from_ne_bytes(buffer[..4].try_into().unwrap());

            let conn = {
                let connections = self.connections.lock().unwrap();
                connections[id as usize].clone()
            };

            conn.lock().await.write_all(&buffer[4..]).await.unwrap();
        }
    }
}
