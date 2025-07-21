+++
title = "Sharing state in async code by using the reactor pattern"
description = "Comparison of patterns for sharing mutable state in concurrent applications, and a case for the reactor pattern"
date = 2025-07-04
draft = false

[taxonomies]
tags = ["rust", "programming", "memory-management", "concurrency", "IO"]
categories = ["rust", "patterns"]

[extra]
toc = true
+++


Correctly sharing mutable state in concurrent Rust code is difficult. To achieve this, we normally use either mutexes or channels. I want to propose an alternative, the **reactor pattern**.

In this article I'll present a simple example of a network application, and progressively apply these patterns, in order to consider their trade-offs, showing you how the reactor pattern allows you to share mutable state in a straightforward manner.

<!-- more -->

## Motivation

This post is specifically about sharing mutable state in IO-bound applications. These kinds of applications benefit heavily from concurrency, meaning that tasks don't block the execution of the rest of the program while waiting for IO events to occur. To deal with this, we normally use async runtimes, and that's what I'll focus on.

The most common way to achieve concurrency in an async runtime is to spawn new `Task`s to handle IO events. `Task` is a primitive provided by the async runtime that represents a unit of work. As IO events unblock this unit of work, it can then be scheduled on this thread or another to run concurrently with other `Task`s.

There's a price to pay for that convenience; any `Future` that `Task` is meant to execute must be `'static` and possibly `'Send`. For the latter, it's only a requirement if it can be scheduled in different threads. This can be prevented by using constructs like `tokio::task::LocalSet`, although it can be a bit unwieldy, it will get rid of the `'Send` requirement. However, `'static` is a much harder requirement to get rid of.

`'static` is a requirement because the spawned `Future` can be run at any later time, even outliving the scope of the block that originally spawned it. There are two ways to meet this requirement and have a mutable state: either by using a structure with internal mutability to store state, or by having single ownership of the different pieces of state in different tasks and coordinating between them using channels.

Another option could be to stop using `spawn` altogether and instead try to run all the futures in a single task. To achieve this there are functions available like `futures::future::select`, `futures::future::select_all`, or `tokio::select`, which indeed don't require `'static`. Still, the `Future`s are themselves multiple units of work that exist at the same time; they can't share `&mut` references to the state. For that, again, you need structures with internal mutation.

However, if we could segregate the IO futures from the state mutation, we could potentially have futures with no references to the shared state and a common block for handling the IO event for the future. That's the idea behind the reactor pattern.

The idea is you can wait for any IO event to happen, and when an event occurs, you get a descriptor of the event and can just handle it, having an `&mut` reference to state. But this can be taken a step further.

Normally, calling `.await` in an `async` function schedules a `Waker` associated with a task to be woken at some point in the future. The compiler automatically keeps track of the state of the future by generating an enum that represents the `await` point and keeps the state stored. Notice that this coupling only exists so that the task can resume execution at the same point; we could structure our code so that each time the task is woken, we try to sequentially advance work on all our IO and poll all our IO for new events. That way, all IO can also share references to mutable state. It's okay if this is a bit confusing right now; it will become quite clearer once we move into the concrete examples.

This is another approach to the reactor pattern with its own trade-offs that we will also discuss.

The first version of the reactor pattern I mentioned probably rings familiar to anyone who has been working with async Rust code. A loop that selects between multiple futures is a common pattern, and that's indeed the reactor pattern in use, but the key is that we can architect the whole program around this without spawning any task. 

In fact, the reactor pattern is what `tokio` and most other runtimes use under the hood to implement their execution engine. But we can go beyond that and use the pattern even in application-level contexts where you have an async runtime.

To show this, I introduce a simple example that we will evolve with different modeling techniques for I/O concurrent code. It will give us some context to discuss the benefits and drawbacks of each of these patterns.

## The toy problem

This is a small and simplified, yet almost realistic, scenario. I've picked some artificial constraints just to save us from writing a few extra lines. This way we can focus on the core of the issue, namely, sharing mutable state in concurrent code.

We will be writing a server that clients can connect to and subsequently exchange messages between them through it.

{{ note(body="All numbers in the wire are encoded in network-order.") }}

<!-- Diagram of the problem -->

Clients will connect to the server over a TCP socket, then they will immediately be assigned an ID, which will be sent back as a 4-byte response.

<!-- Diagram of the ID message -->

The clients can exchange this ID over an out-of-band channel, then use the other client's ID to craft a message to be forwarded by the server to the client with that assigned ID.

<!-- Diagram of the Message -->

The crafted message is composed of the other receiving client's ID followed by a null-terminated stream of bytes.

Once a complete message is read by the server, it will forward the null-terminated stream of bytes to the corresponding peer, without including the ID header. 

<!-- Full sequence diagram -->

We will assume these unrealistic simplifications.

* Every client is well-behaved and will never abuse the protocol.
  * This means clients will always send complete messages and won't start a new one without finishing the one before.
  * Every message a client sends follows the protocol, and the message always starts with the 4-byte ID.
* A client will never send a message to itself.
* Once started, a server will never stop.
* Random u32 IDs will never overlap.

This protocol could cause the clients to receive segmented messages from different clients without the possibility to distinguish between them, but let's ignore that too.

## Implementations

We can easily identify that we need some kind of concurrency to react either to a message from any client or a new TCP connection.

This can be done with threads, manually with OS functions, or with an async runtime. Since we are writing an IO-bound application, we will go for an async runtime, `tokio` in this case.

Having established the scene, let's move on to the details.

## Tests

First, let's write a test to get a feeling of how the protocol should behave. Note that this isn't done to catch edge cases, only to encode a few key expectations.

The interface for the server will be this. 

```rs
struct Server;

impl Server {
  pub async fn new() -> Server;
  pub async fn handle_connections(self: Arc<Self>) -> tokio::task::JoinHandle<()>;
}
```

{{ note(body="The signature of `Server::handle_connections` will change slightly to use `&mut self` in a later section, but updating the tests is trivial. Anyways, there will be a full implementation, including a test for each of the examples.", header="Tip") }}

With that in mind, we can write this test.

```rs
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
```

Simply put:

1. We start the server in the background.
1. Create two sockets and connect each to the router, getting the IDs in response.
1. Send a message from one socket to the other conforming to the previously laid out protocol.
1. Assert that each client can get a message from the other.

Before moving on to the specifics, we'll move on to a common parsing function used by all implementations.

### Message parsing

We will use the `bytes` crate as it'll allow us to manipulate the messages with less copying, and although it'll not be our focus, I want to discuss some points on copying.

This function will work by taking the bytes of the incoming message and trying to parse them according to the protocol. If successful, it'll return a tuple of `(<id>, <bytes>)` with the bytes for the message with the intended recipient, consuming the bytes corresponding to the message from the original buffer. Otherwise, it'll return an error and leave the original buffer intact. It will only parse a single message at a time.

```rs
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
```

The function is quite simple; the implementation details are unimportant for the rest of the examples.

Now let's go to the first implementation using `Mutex` to share sockets.

### Mutex


Naively trying to implement the `handle_connections` method without any synchronization, like this.

```rs,linenos,hl_lines=19-21,hl_lines=17
struct Server {
    listener: TcpListener,
    connections: HashMap<u32, TcpStream>,
}

impl Server {
    pub async fn new() -> Server {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        Server {
            listener,
            connections: Default::default(),
        }
    }

    pub async fn handle_connections(&mut self) {
        loop {
            let (socket, _) = self.listener.accept().await.unwrap();

            tokio::spawn(async move {
                self.handle_connection(socket).await;
            });
        }
    }

    async fn handle_connection(&mut self, mut socket: TcpStream) {
        let id = rand::rng().random();
        socket.write_u32(id).await.unwrap();
        socket.flush().await.unwrap();
        self.connections.insert(id, socket);

        let mut buffer = BytesMut::new();

        loop {
            let Ok(n) = self
                .connections
                .get_mut(&id)
                .unwrap()
                .read_buf(&mut buffer)
                .await
            else {
                self.connections.remove(&id);
                break;
            };

            if n == 0 {
                self.connections.remove(&id);
                break;
            }

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            let Some(socket) = self.connections.get_mut(&dest) else {
                continue;
            };

            if let Err(e) = socket.write_all(&m).await {
                eprintln!("Failed to write to socket {e}");
            }
        }
    }
}
```

{% note() %}
The code for this example can be found in the {{ github(file="content/sharing-io-resources/naive") }} directory
{% end %}

Leads the compiler to be quick to point out that in line 19 `self` is borrowed for a `'static` lifetime, which escapes the scope of `handle_connection`. Furthermore, in line 18, `self` is used after it was moved in the previous iteration of the loop.

This means we need both asynchronous reference counting and internal mutability, so `Arc<Mutex<T>>` it is. If one tries to do this with a conventional `std::sync::Mutex`, the compiler will disallow it; since we need to hold the lock while we send a message, we need to use Tokio's `Mutex`.

But, again, just wrapping the connections with a mutex, like in the following implementation, will result in problems.

```rs,linenos,hl_lines=35-42,hl_lines=57
struct Server {
    listener: TcpListener,
    connections: tokio::sync::Mutex<HashMap<u32, TcpStream>>,
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
        let id: u32 = rand::rng().random();
        socket.write_u32(id).await.unwrap();
        socket.flush().await.unwrap();
        self.connections.lock().await.insert(id, socket);

        let mut buffer = BytesMut::new();

        loop {
            let Ok(n) = self
                .connections
                .lock()
                .await
                .get_mut(&id)
                .unwrap()
                .read_buf(&mut buffer)
                .await
            else {
                self.connections.lock().await.remove(&id);
                break;
            };

            if n == 0 {
                self.connections.lock().await.remove(&id);
                break;
            }

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            let mut connections = self.connections.lock().await;

            let Some(socket) = connections.get_mut(&dest) else {
                continue;
            };

            if let Err(e) = socket.write_all(&m).await {
                eprintln!("Failed to write to socket {e}");
            }
        }
    }
}

```

{% note() %}
The code for this example can be found in the {{ github(file="content/sharing-io-resources/deadlock-mutex") }} directory
{% end %}

Once a task locks a mutex for reading from the socket, it'll prevent any other socket from being read or written, causing a deadlock. So even if the program compiles, the test hangs forever.

<!-- Diagram here perhaps? -->

We can fix this by noting that we only need to share the write side of the socket:

```rs,linenos,hl_lines=55
struct Server {
    listener: TcpListener,
    connections: tokio::sync::Mutex<HashMap<u32, OwnedWriteHalf>>,
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
            let router = self.clone();

            tokio::spawn(async move {
                router.handle_connection(socket).await;
            });
        }
    }

    async fn handle_connection(&self, socket: TcpStream) {
        let (mut read, mut write) = socket.into_split();
        let id: u32 = rand::rng().random();
        write.write_u32(id).await.unwrap();
        write.flush().await.unwrap();
        self.connections.lock().await.insert(id, write);

        let mut buffer = BytesMut::new();

        loop {
            let Ok(n) = read.read_buf(&mut buffer).await else {
                self.connections.lock().await.remove(&id);
                break;
            };

            if n == 0 {
                self.connections.lock().await.remove(&id);
                break;
            }

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            let mut connections = self.connections.lock().await;
            let Some(connection) = connections.get_mut(&dest) else {
                continue;
            };

            connection.write_all(&m).await.unwrap();
        }
    }
}

```

{% note() %}
The code for this example can be found in the {{ github(file="content/sharing-io-resources/mutex") }} directory
{% end %}

The test passes, but this is still pretty bad; when the write buffer of a socket is full, trying to write to that socket will block the task, causing it to hold the lock indefinitely. This will prevent any other client from making progress.

You could try to fix this by wrapping every writer with an `Arc<Mutex<OwnedWriteHalf>>`, but this just keeps adding to the complexity and potential pitfalls. This already illustrates how bad it can get with `Mutex`; correctly using them is hard even with this relatively simple application and very little state.

This is especially accentuated by the fact that our state includes an IO resource. But with any complex state, even those that don't include IO resources, as soon as there is more than one `Mutex`, it becomes a minefield of deadlocks. Mutexes can be properly used, but it's hard and probably not the best idea for most IO-bound applications.

Having that established, let's move to a very well-known alternative with the purpose of sharing state in concurrent applications: channels.

### Channels

To use channels instead of a `Mutex` to synchronize access to state, we use a single task that owns the state, and then other tasks manage the I/O and use channels to send messages to update and retrieve state.

This pattern comes in many flavors, but I want to focus on how using channels affects sharing mutable state, so I'll just keep it simple. Let's take a look at how this is implemented.

```rs,linenos,hl_lines=52-75
struct Server {
    tx: mpsc::Sender<Message>,
    listener: TcpListener,
}

impl Server {
    pub async fn new() -> Server {
        let (tx, rx) = mpsc::channel(100);
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        tokio::spawn(message_dispatcher(rx));
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

async fn message_dispatcher(mut rx: mpsc::Receiver<Message>) {
    let mut connections = HashMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            Message::New(id, mut connection) => {
                if let Err(e) = connection.write_u32(id).await {
                    eprintln!("Failed to write to socket: {e}");
                };
                connections.insert(id, connection);
            }
            Message::Send(id, items) => {
                let Some(connection) = connections.get_mut(&id) else {
                    continue;
                };
                if let Err(e) = connection.write_all(&items).await {
                    eprintln!("Failed to write to socket: {e}");
                }
            }
            Message::Disconnect(id) => {
                connections.remove(&id);
            }
        }
    }
}

enum Message {
    New(u32, OwnedWriteHalf),
    Send(u32, Bytes),
    Disconnect(u32),
}
```

{% note() %}
The code for this example can be found in the {{ github(file="content/sharing-io-resources/channels") }} directory
{% end %}

In this version we have a single `message_dispatcher` that owns `connections`. By nature of being the single owner, `message_dispatcher` has `&mut` access to `connections` without any additional synchronization. This is much simpler than using a `Mutex` as there can't be any deadlock[^1]. But there's head-of-the-line blocking.

In this case, head-of-the-line blocking means that writing to a socket could potentially block the task preventing any other message from being processed until that's done. A possible fix is to wrap the sockets in a `Mutex` to move them into a new task for writing, and in that way `message_dispatcher` can continue processing messages, but we're trying to avoid mutexes.

A better solution is to have a single task owning each writer, in this way:

```rs
async fn central_dispatcher(mut rx: mpsc::Receiver<Message>) {
    let mut connections = HashMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            Message::New(id, connection) => {
                let (tx, rx) = mpsc::channel(100);

                let handle = tokio::spawn(client_dispatcher(connection, rx)).abort_handle();
                tx.send(Bytes::from_owner(id.to_be_bytes())).await.unwrap();
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
```

{% note() %}
The code for this example can be found in the {{ github(file="content/sharing-io-resources/channel-per-writer") }} directory
{% end %}

By moving the socket's ownership into the individual `client_dispatcher`, we are able to keep scheduling messages to the socket even when its buffer is full. But the send channel works just as an additional buffering layer; a single burst of messages to one client could block `central_dispatcher` when that buffer gets full.

At some point this kind of back pressure is necessary, but what if instead of blocking the whole `central_dispatcher` we wanted to stop reading messages from the particular client? Then you'd need to add a channel like this.

```rs
async fn central_dispatcher(mut rx: mpsc::Receiver<Message>) {
        <...>
            Message::Send(id, items, permission_token) => {
                let Some((connection, _)) = connections.get_mut(&id) else {
                    continue;
                };

                tokio::spawn(async {
                    if let Err(e) = connection.send(items).await {
                        eprintln!("Failed to write to socket: {e}");
                    }

                    let _ = permission_token.send(());
                })
            }
        <...>
        }
    }
}

```

On the IO side we will simply await the channel.

```rs
    async fn handle_connection(&self, socket: TcpStream) {
        <...>

        loop {
            let Ok(n) = read.read_buf(&mut buffer).await else {
                let _ = self.tx.send(Message::Disconnect(id)).await;
                break;
            };

            <...>

            let (permission_token, permission_listener) = oneshot::channel();
            self.tx.send(Message::Send(dest, m, permission_token)).await.unwrap();
            permission_token.recv().await;
        }
    }
```

But look at how each interaction between state and IO requires a new channel and a new task. In particular, you need to manage the task's lifetime carefully, making sure you don't leak it. [^2]

Let's take a look at another example of this. Imagine, we didn't just want to silently fail sending a message when we fail to write it, instead, we wanted to send back a message to the original sender. It'll look a bit like this. 

```rs
async fn central_dispatcher(reader_rx: mpsc::Receiver<Message>) {
    let mut connections = HashMap::new();
    let (write_errors_tx, write_error_rx) = mpsc::channel(100);
    let write_error_rx = ReceiverStream::new(write_error_rx);
    let reader_rx = ReceiverStream::new(reader_rx);
    let mut rx = write_error_rx.merge(reader_rx);
    while let Some(msg) = rx.next().await {
        match msg {
            Message::New(id, connection) => {
                let (tx, rx) = mpsc::channel(100);

                let errors_tx = write_errors_tx.clone();
                let handle =
                    tokio::spawn(client_dispatcher(connection, rx, errors_tx)).abort_handle();
                if let Err(e) = tx.send((Bytes::from_owner(id.to_be_bytes()), None)).await {
                    eprintln!("Failed to write to socket: {e}");
                }
                connections.insert(id, (tx, handle));
            }
            Message::Send(id, items, src) => {
                let Some((connection, _)) = connections.get_mut(&id) else {
                    continue;
                };
                if let Err(e) = connection.send((items, Some(src))).await {
                    eprintln!("Failed to write to socket: {e}");
                }
            }
            Message::Disconnect(id) => {
                if let Some((_, handle)) = connections.remove(&id) {
                    handle.abort();
                }
            }
            Message::Error(e, src) => {
                eprintln!("Failed to write to socket: {e}");
                let Some(src) = src else {
                    continue;
                };

                let Some((connection, _)) = connections.get(&src) else {
                    continue;
                };

                let mut error = BytesMut::new();
                error.put_u8(0xFF);
                error.put(&format!("Error: {}\0", e).into_bytes()[..]);

                let _ = connection.send((error.freeze(), None)).await;
            }
        }
    }
}

async fn client_dispatcher(
    mut connection: OwnedWriteHalf,
    mut rx: mpsc::Receiver<(Bytes, Option<u32>)>,
    errors_tx: mpsc::Sender<Message>,
) {
    loop {
        let (m, src) = rx.recv().await.unwrap();
        if let Err(e) = connection.write_all(&m).await {
            let _ = errors_tx.send(Message::Error(e, src)).await;
            break;
        }
    }
}

```

{% note() %}
The code for this example can be found in the {{ github(file="content/sharing-io-resources/channel-with-error") }} directory
{% end %}

We were able to reuse the same channel this time, but there's a separation between the callsite of the IO function and the handler of the error. This interrupts the normal error flow, where `client_dispatcher` can't use a `?` or handle the error by altering the state. The awkwardness with this is made evident by that `src` we needed to pass around between `central_dispatcher` and `client_dispatcher` back and forth to keep context on the error. The same happens with the `disconnect` message; instead of just handling the `read` error, we're creating a different message so that the `central_dispatcher` can update the state. 

This is a consequence of the biggest downside of using this model to mutate state: by completely decoupling the IO from the state, there is no way to know from where the values that alter the state are emitted within the context of that state. To see what I mean take a look at the latest implementation of `central_dispatcher`.

```rs,linenos,hl_lines=20-27
async fn central_dispatcher(reader_rx: mpsc::Receiver<Message>) {
    let mut connections = HashMap::new();
    let (write_errors_tx, write_error_rx) = mpsc::channel(100);
    let write_error_rx = ReceiverStream::new(write_error_rx);
    let reader_rx = ReceiverStream::new(reader_rx);
    let mut rx = write_error_rx.merge(reader_rx);
    while let Some(msg) = rx.next().await {
        match msg {
            Message::New(id, connection) => {
                let (tx, rx) = mpsc::channel(100);

                let errors_tx = write_errors_tx.clone();
                let handle =
                    tokio::spawn(client_dispatcher(connection, rx, errors_tx)).abort_handle();
                if let Err(e) = tx.send((Bytes::from_owner(id.to_be_bytes()), None)).await {
                    eprintln!("Failed to write to socket: {e}");
                }
                connections.insert(id, (tx, handle));
            }
            Message::Send(id, items, src) => {
                let Some((connection, _)) = connections.get_mut(&id) else {
                    continue;
                };
                if let Err(e) = connection.send((items, Some(src))).await {
                    eprintln!("Failed to write to socket: {e}");
                }
            }
            Message::Disconnect(id) => {
                if let Some((_, handle)) = connections.remove(&id) {
                    handle.abort();
                }
            }
            Message::Error(e, src) => {
                eprintln!("Failed to write to socket: {e}");
                let Some(src) = src else {
                    continue;
                };

                let Some((connection, _)) = connections.get(&src) else {
                    continue;
                };

                let mut error = BytesMut::new();
                error.put_u8(0xFF);
                error.put(&format!("Error: {}\0", e).into_bytes()[..]);

                let _ = connection.send((error.freeze(), None)).await;
            }
        }
    }
}
```

Imagine you wanted to know where the `Message::Send(...)` comes from and what the parameters are. One way you could try to do this in normal sync code is to set a breakpoint at that line, look at the backtrace, and try to work backwards to where the variables come from. But, in this case, if you print the backtrace at that point, you get the following.


```
frame #0: 0x0000000100032164 channel-per-writer`channel_per_writer::central_dispatcher at main.rs:115:45
frame #1: channel-per-writer`tokio::runtime::task::core::Core::poll at core.rs:331:17``
...

```


This means, it's reasonable to expect the value of the variable to come from some local function call, as there's nothing up stack we can look at in our own code. Let's trace back where `Message::Send(...)` comes from then, first we can see there's a match with `msg`, so we need to find `msg`, which is defined a few lines above from `let Some(msg) = rx.next().await`. From just the line above`rx` is a combination of `write_error_rx` and `reader_rx`, `write_reader_rx` is a channel we created locally, so its corresponding `Sender` we can trace down but `reader_rx` comes from the caller of `central_dispatcher` which we can't find in the backtrace!

This is potentially a problem with any callback, but it's worse with channels. Even if you manage to track down where the channel is created, it doesn't tell you what is the source of the received values is; for that you need to find the corresponding `Sender`. But the `Sender` is cloneable, so you could find it in multiple places. And more than one of those places could potentially send the same variant, making it impossible to correlate the function that sent the values with the value received in a normal debugger. This can make programs very difficult both to understand and to debug.

Contrast this with the `Mutex` version of `handle_connection`:


```rs,linenos,hl_lines=34,10-17
async fn handle_connection(&self, mut socket: TcpStream) {
    let id: u32 = rand::rng().random();
    socket.write_u32(id).await.unwrap();
    socket.flush().await.unwrap();
    self.connections.lock().await.insert(id, socket);

    let mut buffer = BytesMut::new();

    loop {
        let Ok(n) = self
            .connections
            .lock()
            .await
            .get_mut(&id)
            .unwrap()
            .read_buf(&mut buffer)
            .await
        else {
            self.connections.lock().await.remove(&id);
            break;
        };

        if n == 0 {
            self.connections.lock().await.remove(&id);
            break;
        }

        let Ok((dest, m)) = parse_message(&mut buffer) else {
            continue;
        };

        let mut connections = self.connections.lock().await;

        let Some(socket) = connections.get_mut(&dest) else {
            continue;
        };

        if let Err(e) = socket.write_all(&m).await {
            eprintln!("Failed to write to socket {e}");
        }
    }
}
```


In this case, if you wanted to know where the value of `m`, `dest`, or `id` came from, you can just look them up locally, since the IO is done in the same backtrace as where they are used. For example, `m` comes from a pattern match in `parse_message` by passing `buffer` into the function, `buffer` comes from `read_buf` just a few lines above, and we can see that we created the buffer locally. So we know the buffer comes from reading on a socket on `connections`, which we can find if we look at `&self`.


<!-- Diagram of strict hierarchical calls and sharing memory through channels -->

The question then is, can we still have locality of the IO data without having to use a `Mutex` to share mutable access to state? And in fact we can if we use a single task to do everything.


### A single task to rule them all

#### Racing futures

Handling multiple concurrent events in a single task in async Rust is actually a pretty common pattern; a typical reason to do this is to wait for both a timeout and a channel or to handle cancellation while blocking on some IO. This is often done through the [`tokio::select!`](https://docs.rs/tokio/latest/tokio/macro.select.html) macro. However, `tokio::select!` isn't ideal for our case; we have a variable number of futures we want to listen to, so we can use something like [`futures::future::select_all`](https://docs.rs/futures/latest/futures/future/fn.select_all.html) or [`futures_concurrency::future::Race`](https://docs.rs/futures-concurrency/latest/futures_concurrency/future/trait.Race.html).


 As explained [in Tokio's docs,](https://docs.rs/tokio/latest/tokio/macro.select.html#merging-streams) this can also be achieved using streams; that way we never drop an incomplete future. By doing it that way, we don't have to worry about cancellation safety. Nevertheless, all futures we're using are cancel-safe, so we will go with `futures_concurrency::future::Race`, as that makes adaptors as simple as possible.

With this in mind, our goal is to wait concurrently on any of our possible IO events:
* A new TCP connection.
* A new message from a socket.
And react as they happen synchronously. Concurrently to waiting on those events, we want to forward bytes to the clients.

The first thing we require to make this work is to homogenize the return types from our futures, this will simply be done by an `Event` enum.

```rs
enum Event<'a> {
    Read(&'a mut BytesMut),
    Connection(TcpStream),
}
```

This is similar to the `Message` enum we had before, but the `Read` variant doesn't contain the parsed message; instead, it has the raw bytes from the socket. The parsing will be done after the event is generated.

```rs
type Result<T> = std::result::Result<T, (io::Error, u32)>;
```

Now, this socket abstraction makes it all come together. Let's take a look at it.

```rs
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
    buffer: BytesMut,
    writer: OwnedWriteHalf,
    id: u32,
}

impl WriteSocket {
    fn new(writer: OwnedWriteHalf, id: u32) -> WriteSocket {
        WriteSocket {
            buffer: BytesMut::new(),
            writer,
            id,
        }
    }

    async fn advance_send(&mut self) -> Result<Event> {
        loop {
            if !self.buffer.has_remaining() {
                std::future::pending::<Event>().await;
            }

            self.writer
                .write_all_buf(&mut self.buffer)
                .await
                .map_err(|e| (e, self.id))?;
        }
    }

    fn send(&mut self, buf: &[u8]) {
        self.buffer.put_slice(buf);
    }
}
```

There's nothing outlandish with the `ReadSocket`; it owns its read buffer so as to make the `read` function only take `&mut self`. This way sockets don't need to share buffers and allows multiple `read` calls to run concurrently.

`WriteSocket` in contrast is pretty interesting. We don't really need to handle any event from sending bytes down to the clients, but we do need to drive that future forward. In order to do this, `send` doesn't block, instead it schedules the bytes to be sent at a later point. This way, as soon as we read some new bytes we can `send` them without blocking the task. In order to drive the socket forward we'll use the future created by `advance_send`.

In its signature `advance_send` returns an `Event`; however, the only purpose of the return type is to be able to use it with `Race` at a later time; the function itself never returns. Instead, it loops indefinitely; if its buffer is empty, it will await on `pending`. This leaves the future in a `Pending` state without any wake condition, meaning `advance_send` will never do any more work. If the buffer has anything left on it, the function will try to write all its contents into the socket and loop around into the pending state once it's done. If at any point while writing its contents the `advance_send` future is dropped, `write_all_buf` only advances `BytesMut` the number of bytes that has been written. Meaning, next time we call `advance_send` on the same `WriteSocket`, it'll continue sending bytes from the last byte we left off.

Now, if we were to simply `await` on `advance_send`, that would block a task forever, but if we do it concurrently with other futures we `select` we can react to the other futures and continue driving the `WriteSocket` forward.

With these new structs in place, we need need to know how we "wait for the next event".

```rs
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

```

When there are no connections yet, the only possible event is a new connection emitted by the `listener`. Otherwise, we `race` among all the futures from all writers, readers, and the listener, and emit an event based on the first one to finish. [`race`](https://docs.rs/futures-concurrency/latest/futures_concurrency/future/trait.Race.html#tymethod.race) is a method provided by the `Race` trait. `race` is also fair[^4][^5], in the sense that none of the futures will hog the executor.

With this `next_event` in place, we can now handle all events concurrently in a single task, using this `handle_connections` function.


```rs
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
                    writer.send(&d);
                }
                Event::Connection(tcp_stream) => {
                    let id = rand::rng().random();
                    let (r, w) = tcp_stream.into_split();
                    let mut write_sock = WriteSocket::new(w, id);
                    write_sock.send(&id.to_be_bytes());
                    self.read_connections.insert(id, ReadSocket::new(r, id));
                    self.write_connections.insert(id, write_sock);
                }
            }
        }
    }
```

Notice how now we have `&mut self` access to the state; we can simply modify `read_connections` and `write_connections` as our IO generates events. And we have the benefit of concurrency provided by the runtime to execute all this IO concurrently. I think this is pretty neat.

Yet, we can go further down this path. First, let's get something out of the way: we can make this approach zero-copy with a few modifications. We change `WriteSocket` to this.

```rs
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
```

Now, `send` no longer copies data; instead, it keeps it in an internal vector. Then with a slight modification to `handle_connection` we can make this all work.

```rs
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
```

This is all well and good, but there are still some other trade-offs we can make. First, note that we need to make an adaptor for every future to return a `Event`; this can get awkward when you've multiple futures from multiple sources. Furthermore, errors need to be wrapped with an event to be able to correlate it back to one of the futures with some indication of the causing future. `select_all` does give us exactly what the future that finished was, but it required some additional handling. Finally, there are some rare cases when you need to share state while the futures are advancing; for example, if for some reason you didn't have access to something like the bytes crate and didn't want to clone, you'd need to share access to the same buffer while writing and reading. This is simply impossible like this without a Mutex.

So let's explore an option that lifts these limitations.

#### Hand-rolled future

Instead of merging all futures together and waiting for any of them to be ready, we can poll them one by one, see which are ready, and react immediately. As we poll each future, we register their waker in the background, and any event that wakes the waker will cause us to poll every future again. 

Let's begin at how the `Socket` implementation will look here. We can't split the IO no longer, as the splitted, version don't expose a convinent `poll` function for writing and reading.


```rs
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
```

If you take a careful look at this, it's very similar to the previous version. `poll_read` tries to read from the `TcpStream` if there are bytes available it'll return a reference to them. Otherwise, it'll have its waker registered and return pending.

`poll_send` is a bit more complicated, just because I wanted to keep the zero-copy from the latest version. First, if there's nothing to write it saves the waker so that when there's something to write we can be polled again. Then, if there's something to write, we check if the socket is ready for writing, otherwise we register the waker and return pending. If we manage to write, we update the buffers.

Notice in both these functions we check for readiness, then we try to read or write, and normally you would handle explicitly the case of `WouldBlock`. In this case we assume there're no OS errors, so it must be a `WouldBlock`, then we need to re-register as the try_read/try_write doesn't do that for us, that's why we `continue`. 

Lastly, `send` is pretty simple, we enqueue the buffer for writing and if there was a waker registered by `poll_send` previously, we wake it up, so that `poll_send` can be called again.

With this in place we will add this function to the server.

```rs
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
```

First, we check if listener has a new socket, in case there's we send the id to the socket and push it into our list of sockets. Then for each socket, we advance their internal send queue. Finally, for each socket, we try to read a message, if there's a complete message we enqueue it to the corresponding destination socket.

This is no different from what we've been doing before, the only difference is that now instead of the executor polling each future for us we're doing it manually.

There's some special care taken here, for one, there's fairness by always polling every future in every call to `poll_next`. This way, no particular socket can keep generating readiness events and prevent other future from advancing. Also, either a future returns pending after we stop polling it or we call `cx.waker().wake_by_ref()` so that we are immediately polled again, otherwise there's a chance the future is ready again and we are never awaken. 

Finally, to expose a nice async interface we use `poll_fn`.

```rs
    async fn handle_connections(&mut self) {
        poll_fn(move |cx| self.poll_next(cx)).await;
    }
```

And there we have it; we're manually polling the futures. If we were handling errors, `poll_send` could surface it directly as `Poll<io::Result<()>>`, for example. And then in the main loop we can handle it by doing `self.connection.remove[dest]`[^6]. There's also no need to homogenize the return types from the futures, as we handle them separately.

## Picking a pattern

### Multitask vs Single task

We've worked through these patterns and hopefully demonstrated a bit of their tradeoffs. Now let's expand on that to help you pick which one you should use in your application.

This isn't a pick-one-or-the-other situation. You can mix and match; you can lock on a synchronous `Mutex` anywhere and race with an asynchronous one, although it doesn't expose a `poll` API, so you can't trivially use it if you're hand-rolling your own polling logic. You can always use a channel on a task that's polling on multiple futures or as part of a race; this way you can have more than one managing multiple futures with their own internal state and yet have them communicate. 

Even so, you need to consider the trade-offs of these concurrency patterns to pick which one to use.

There's generally not much of a need to use a `Mutex`, considering how easy it is to mess it up. In practice, the cases where you want a mutex are limited to either very constrained environments on performance or memory; or if you don't have an async runtime and need to share mutable access to some state between threads.

Barring those constraints, we can either multiplex all futures in a single task, or use a task per I/O and communicate state updates through channels. For most cases it's better to use a single task, the ergonomics lost from having to explicitly buffer the state of the future are worth having direct mutable access to state instead of managing channels and tasks. After all, functionally, the channel is just a buffering layer. 

The big reason for sacrificing the ergonomics of having a single task with shared access to mutable state, is to have the different futures scheduled in parallel in multiple threads. <This is important if the synchronous code handling events or the sequential polling of IO is a bottleneck for the throughput of IO events produced. So if more events are produced than a single thread can handle, multiple tasks could be necessary.> This is however, a very complicated case; you lose data locality and the channels blocking and allocations can also negatively impact performance, so if you need to go down this route don't think just spawning new tasks will magically solve your performance issues.

Of course, if your tasks don't need mutable access to the same state it's a good idea to use them to represent separated units of work and might be more performant. 

### Hand-rolled future vs Combinators

In the end, it comes down to a choice of either manually polling your futures, or using combinators and adaptors to race all the futures with typical `async`/`await` syntax. I'd tend to go for the first one, since there's the benefit that the IO polling itself can share mutable state, this is normally not *necessary* but it gives you more versatility on how to structure your IO objects; another key point is that the combinators and adaptors can become very verbose and hard to follow when the codebase grows. Furthermore, when manually polling you have a plain view into the polling states, which can be very useful for debugging. However, this comes at the cost of losing some of the ergonomics of calling `await`, but you can still wrap your function manually polling for IO with `poll_fn` and use it in a greater `async` context. Finally, there's the benefit of having the IO errors immediately be surfaced on `poll`.

So, in conclusion, if you have multiple futures that require access to shared mutable state, try to keep the polling within a single task, only go to multiple task if the benchmarks hint at improvements. If the number of IO streams is small you can use combinators and wrappers to listen to the events concurrently, but as soon as it gets verbose or hard to debug you should try manually polling those events.

## Sans-IO

Sans-IO is a model for network protocol, where you implement them in an IO-agnostic way, as a state machine that's evolved by outside events.

You might immediately see the link with the single-task model, where we segregate the events emitted by the IO to the state of the application.

Both of these approaches work very well in tandem, one might say they are the same approach, reactor pattern seen from the perspective of how to actually structure the IO and Sans-IO on how to organize your code to react to those events.

But actually Sans-IO has a standard way to be organized, using methods like `poll_timeout`, `handle_timeout`, `poll_event`, etc... there are a few more patterns. And you could actually integrate it with any of the I/O models in the previous section.

And in theory, you could still do I/O when reacting to an event in the reactor pattern, though most of the time it'd defeat the point.

So it's good to see them as 2 different approaches.

To learn more about Sans-IO you could read...

---

[^1]: In fairness, deadlocks are still possible, if 2 tasks are waiting from one another preventing from making any other work.
[^2]: https://draft.ryhl.io/blog/actors-with-tokio/ Has a discussion of how to do this.
[^4]: https://github.com/yoshuawuyts/futures-concurrency/issues/85
[^5]: https://github.com/yoshuawuyts/futures-concurrency/pull/104
[^6]: Of course there should be extra care to no longer use the index into the connections like we were before, a Map ofIDs to sockets would work much better.
