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

This is a small and simplified, yet almost realistic, scenario. I've picked some artificial constraints just to save us from some error handling and a few extra code paths. This way we can focus on the core of the issue, namely, sharing mutable state in concurrent code.

We will be writing a server that clients can connect to and subsequently exchange messages between them through it.

{{ note(body="All numbers in the wire are encoded in network-order.") }}

<!-- Diagram of the problem -->

Clients will connect to the server over a TCP socket, then they will immediately be assigned an id, which will be sent back as a 4-byte response.

<!-- Diagram of the ID message -->

The clients can exchange this ID over a side channel, then use the other client's ID to craft a message to be forwarded by the server to the client with that assigned ID.

<!-- Diagram of the Message -->

The crafted message is composed of the other receiving client's ID followed by a nul-terminated stream of bytes.

Once a complete message is read by the server, it will forward the nul-terminated stream of bytes to the corresponding peer, without including the ID header. 

<!-- Full sequence diagram -->

We will assume these unrealistic simplifications.

* Every client is well-behaved and will never abuse the protocol.
  * This means clients will always send complete messages and won't start a new one without finishing the one before.
  * Every message a client sends follows the protocol, and the message always starts with the 4-byte ID of an existing client.
* OS errors never happen.
* Once a client connects, it never disconnects.
* A client will never send a message to itself.
* Once started, a server will never stop.

This protocol could cause the clients to receive segmented messages from different clients without the possibility to distinguish between them, but let's ignore that too.

## Implementations

We can easily identify that we need some kind of concurrency to react either to a message from any client or a new TCP connection.

This can be done with threads, manually with OS functions, or with an async runtime. Since we are writing an IO-bound application, we will go for an async runtime, `tokio` in this case.

Having established that, let's move on to the details.

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

This function will work by taking the bytes of the incoming message and trying to parse them according to the protocol. If successful, it'll return a tuple of `(<id>, <bytes>)` with the bytes for the message with the intended recipient, consuming the bytes corresponding to the message from the original buffer. Otherwise, it'll return an error and leave the original buffer intact It will only parse a single message at a time. 

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

{{ note(body="The code for this example can be found in the [mutex](./mutex) directory") }}

If tries to implement the `handle_connections` naively without any synchronization, like this.

```rs,linenos,hl_lines=20
struct Server {
    listener: TcpListener,
    connections: Vec<TcpStream>,
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
        let id = self.connections.len();
        socket.write_u32(id as u32).await.unwrap();
        socket.flush().await.unwrap();
        self.connections.push(socket);

        let mut buffer = BytesMut::new();

        loop {
            let n = self.connections[id].read_buf(&mut buffer).await.unwrap();
            assert!(n != 0);

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            self.connections[dest as usize].write_all(&m).await.unwrap();
        }
    }
}
```

The compiler is quick to point out that in line 21 `self` is borrowed for a `'static` lifetime, which escapes the scope of `handle_connection`. Furthermore, in line 18, `self` is used after being moved.

This means we need both multi-thread reference counting and internal-mutability so `Arc<Mutex<T>>` it is.

There are a few ways to implement this; we can just wrap `connections` or the whole struct. I'll just use an `Arc` for the whole struct, so we can move `self` into the new task, and a `Mutex` on connections so we can just lock on that shared vector. If one tries to do this with a conventional `std::sync::Mutex` the compiler will disallow it. Since we need to hold the lock while we send a message, we need to use Tokio's `Mutex`.

But, again, a naive implementation like the following will result in problems.

```rs,linenos,hl_lines=27,hl_lines=35,hl_lines=45
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
```

If you run `cargo test` now, this will simply deadlock. On line 35 we grab the lock for the client and hold it until the client sends a new message. For our test, the first client to connect doesn't send a message until it receives the ID from the second client. And the second client can't get the ID until it obtains the lock in line 27, so a deadlock.

We could try to fix this by using a different ID scheme or holding the number of clients in a different variable. Even with that refactor, there are plenty of conditions for deadlocks between lines 35 and 45. Instead, the more correct implementation will look like this.


```rs,linenos,hl_lines=47
struct Server {
    listener: TcpListener,
    connections: tokio::sync::Mutex<Vec<OwnedWriteHalf>>,
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
        {
            let mut connections = self.connections.lock().await;
            let id = connections.len();
            write.write_u32(id as u32).await.unwrap();
            write.flush().await.unwrap();
            connections.push(write);
        }

        let mut buffer = BytesMut::new();

        loop {
            let n = read.read_buf(&mut buffer).await.unwrap();
            assert!(n != 0);

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            let mut connections = self.connections.lock().await;
            connections[dest as usize].write_all(&m).await.unwrap();
        }
    }
}
```

By splitting the socket into a writer and a reader, we only need to hold a `Mutex` for writing; this way we lock in a single place. But this is still pretty bad; a single client can hold the lock for an indefinitely long time. In line 47, if the socket's buffer is full, the `MutexGuard` won't be released until there's room in the buffer. That'll prevent any message from being forwarded to other clients, even when their buffers aren't filled.

You could try to fix this by wrapping every writer with an `Arc<Mutex<OwnedWriteHalf>>`, but this keeps adding to the complexity and potential pitfalls. Hopefully, this already illustrates how bad it can get with `Mutex`; correctly using them is being proved difficult, even with this relatively simple application and little state.

This is especially accentuated by the fact that our state includes an IO resource. But with any complex state, even those that don't include IO resources, as soon as there is more than one `Mutex`, it becomes a minefield of deadlocks. Mutexes can be properly used, but it's hard and probably not the best idea for most IO-bound applications.

Then let's move to a very well known alternative to share state in concurrent applications, channels.

### Channels

> [!NOTE]
> The code for this section can be found in the [channels](./channels) directory.

To use channels instead of a `Mutex` to synchronize access to state, we use a single task that owns the state, and the other tasks use channels to send messages to update and retrieve state.

There are many flavors of this pattern; actors[^1] is one of the most popular. Sometimes we use a response channel; sometimes we don't need it. Regardless of the specifics, I want to focus on more general properties of this pattern, so I'll use channels as plainly as possible.

This is a simple reimplementation using channels.

```rs,linenos,hl_lines=46-60
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
        let (mut read, write) = socket.into_split();
        self.tx.send(Message::New(write)).await.unwrap();

        let mut buffer = BytesMut::new();

        loop {
            let n = read.read_buf(&mut buffer).await.unwrap();
            assert!(n != 0);

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            self.tx.send(Message::Send(dest, m)).await.unwrap();
        }
    }
}

async fn message_dispatcher(mut rx: mpsc::Receiver<Message>) {
    let mut connections = Vec::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            Message::New(mut connection) => {
                let id = connections.len();
                connection.write_u32(id as u32).await.unwrap();
                connections.push(connection);
            }
            Message::Send(id, items) => {
                connections[id as usize].write_all(&items).await.unwrap();
            }
        }
    }
}

enum Message {
    New(OwnedWriteHalf),
    Send(u32, Bytes),
}
```

In this version we have a single `message_dispatcher` that owns `connections`. By nature of being the single owner, `message_dispatcher` has `&mut` access to `connections` without any additional synchronization. This is much simpler than using a `Mutex` as there can't be any deadlock[^2]. But there's head-of-the-line blocking.

In this case, head-of-the-line block means that line 56 prevents messages from any other client from being processed if the socket is not immediately ready. A possible fix is to wrap the sockets in a `Mutex` to be able to move them into a new task for writing, and in that way `message_dispatcher` can continue processing messages, but we're trying to avoid mutexes.

A better solution is to have a single task owning each writer, in this way:

```rs
async fn central_dispatcher(mut rx: mpsc::Receiver<Message>) {
    let mut writers = Vec::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            Message::New(connection) => {
                let (tx, rx) = mpsc::channel(100);

                tokio::spawn(client_dispatcher(connection, rx));
                let id = writers.len();
                tx.send(Bytes::from_owner((id as u32).to_be_bytes()))
                    .await
                    .unwrap();
                writers.push(tx);
            }
            Message::Send(id, items) => {
                writers[id as usize].send(items).await.unwrap();
            }
        }
    }
}

async fn client_dispatcher(mut connection: OwnedWriteHalf, mut rx: mpsc::Receiver<Bytes>) {
    loop {
        let m = rx.recv().await.unwrap();
        connection.write_all(&m).await.unwrap();
    }
}
```

> [!NOTE]
> The full code for this version can be found in the [channel-per-writer](./channel-per-writer) directory.

By moving the writer's ownership into the individual `client_dispatcher`, we are able to keep scheduling messages to the socket even when its buffer is full. This is just an additional buffering layer, with the benefit that we're in control of its size, but a single burst of messages to one client could block `central_dispatcher`.

We can instead use `try_send`, which isn't blocking, and manage our own buffering logic for all clients. Yet, managing the message buffer can grow into a quite complex problem. To solve this, we can spawn a new task for each message we send to the `client_dispatcher` without using any mutex, since now `writers[i]` is cloneable.


```rs
        match msg {
            Message::New(connection) => {
                let (tx, rx) = mpsc::channel(100);

                let w = tx.clone();
                let id = writers.len();
                tokio::spawn(client_dispatcher(connection, rx));
                tokio::spawn(async move {
                    w.send(Bytes::from_owner((id as u32).to_be_bytes()))
                        .await
                        .unwrap();
                });
                writers.push(tx);
            }
            Message::Send(id, items) => {
                let w = writers[id as usize].clone();
                tokio::spawn(async move {
                    w.send(items).await.unwrap();
                });
            }
        }
```

This solves most of the problems; at some point back-pressure needs to be handled and having the task that sends messages to the `client_dispatcher` be blocked seems good enough. However, one might want to stop reading packets as channels get filled; that way the TCP queues get full and new packets don't get acknowledge and clients will automatically retransmit packets. Notice, however, this would require a channel back from the anonymous tasks that send the bytes to the `client_dispatcher` and then back from the `central_dispatcher` to `handle_connection`.

Another way we will require more channels is if we wanted to start handling errors on writing to the socket. Instead of being able to handle errors idiomatically in-place in `central_dispatcher`, `client_dispatcher` would need a way to send the error back, if that error would kill the connection.

Furthermore, each additional IO requires its own task, additionally, each piece of state requires either its own task or to be added to the `central_dispatch`. The first way would also require more channels for cross-comunication between state owners, and the problem with all these channels is that they are decoupled from the tasks generating messages. This is what we want from channels, but it potentially makes code harder to follow. Look at `central_dispatcher` by itself.

```rs
async fn central_dispatcher(mut rx: mpsc::Receiver<Message>) {
    let mut writers = Vec::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            Message::New(connection) => {
                let (tx, rx) = mpsc::channel(100);

                let w = tx.clone();
                let id = writers.len();
                tokio::spawn(client_dispatcher(connection, rx));
                tokio::spawn(async move {
                    w.send(Bytes::from_owner((id as u32).to_be_bytes()))
                        .await
                        .unwrap();
                });
                writers.push(tx);
            }
            Message::Send(id, items) => {
                let w = writers[id as usize].clone();
                tokio::spawn(async move {
                    w.send(items).await.unwrap();
                });
            }
        }
    }
}

```

There's no way to tell how `Message` was created, if for example, we wanted to know if the bytes somehow correlate to a valid string, we would need to hunt down where `Message::Send` is created. That could potentially be anyhwere. Considering the producers can be sent back and forth between tasks, cloned, it can become very complicated.

All of this to say, while it's very nice no longer requiring mutexes, by decoupling IO and state it becomes harder to communicate errors back to the state  and correlate data that alters the state with the IO that generates the data. I mentioned that there is another way to handle additional state, other than having new tasks handling it. We can move it into `central_dispatcher` and have it multiplex messages from multiple channels. This doesn't solve the complexity of data and IO lack of co-location, but it tells us that we could potentially handle all IO-events in a single task concurrently. So let's look into that.

### A single task to rule them all

#### Racing futures

There's an alternative to select in the crate `futures-concurrency` called `Race`. It's very simlar to `select` but only gives us the result as a return value and it works with tuples.

Let's re-write the hand-rolled version using this.

First we will have a slightly different version of the socket.

```rs
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
```
We change the `Event` to

```rs
enum Event<'a> {
    Read(&'a mut BytesMut),
    Connection(TcpStream),
}
```

and now `Router` looks like this.

```rs
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
```

Now we're modeling I/O as a stream of events. We suspend execution while awaiting any of those events, each time one of the events happen we're signaled about it.

In response we handle the event synchroneously, in a context where we have mutable access to the state.
#### Hand-rolled future

There's another option that's way less discussed, if you think about it, what we want to do is:

If there's a new connection, grab it, and store it.
If there's a message in a socket try to parse it and forward it if it's complete.
As the sender becomes available try to send pending messages.
If none of the before are true, just don't do anything until you're signaled that one of these has happened.

Well, we could manually express this.

First let's begin by implementing this abstraction

```rs
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
```

now we can re-implement the `Router` like this.

```rs
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
```

Notice that there's no blocking operations here, some care has been taken to preserve fairness, meaning that all sockets are always polled for readiness in each iteratoion of the loop.

Also, we need to be very careful about always registering the waker into a pending future before returning pending otherwise we might miss the event that wake us.

But in this case, no care has to be taken to share mutable state, the `poll_next` function just takes an `&mut self`.

The difference  with the previous method, is here instead of calling `.await` and immediatley suspend the task, we are able to poll any future for readiness before suspending.

This way none of the futures needs hold a mutable reference, instead we just ask if any of the futures is ready and if it's execute our mutations right there otherwise we ask for the otehr futures.

We just suspend after we know that all futures aren't ready and that's it.

In this way sure, if we are calling `Router::handle_connections` and awaiting on that no one else can mutate `handle_connections` but within `Router`, our shared state, `poll_next` only holds and `&mut self` as long as it's running. 

But taking a careful look at this we can notice another way to put this.

We could have a single place where we ask "Is any I/O ready? if it's let me handle it synchroneously, tell me what exactly and I'll handle it synchroneously, otherwise we keep waiting".

And there's an async/await construct that does basically this, `select`.

 

#### Benefits and drawbacks

This model, recovers the async/await format that can be very useful, as it's harder to put your application in a state where it'll never continue doing work.

However, it comes at a price, in the previous version any time we `poll` a future we can use the shared mutable state this give us more flexibility on how we organize state and very importantly it can prevent additional copies.

For example, both of this versions aren't zero-copy but on the first version we could modify it a little to be zero-copy. In all fairness, thanks to the `Bytes` crate  

<!-- TODO: examples on both of these -->

Another benefit of hand-rolling your future instead of using something like `select` or `race` is that the polling and fairness becomes very transparent, and also cancellation-safety is plain to see.

It becomes easier to debug why your application hangs, as you can easily see how the polling and registration of wakers is done.
 
#### Sans-IO

Sans-IO is a model for network protocol, where you implement them in an IO-agnostic way, as a state machine that's evolved by outside events.

You might immediatley see the link with the reactor model, where we seggregate the events emitted by the IO to the state of the application.

Both of these approaches work very well in tandem, one might say they are the same approach, reactor pattern seen from the perspective of how to actually structure the IO and Sans-IO on how to organize your code to react to those events.

But actually Sans-IO has a standard way to be organized, using methods like `poll_timeout`, `handle_timeout`, `poll_event`, etc... there are a few more patterns. And you could actually integrate it with any of the I/O models in the previous section.

And in theory, you could still do I/O when reacting to an event in the reactor pattern, though most of the time it'd defeat the point.

So it's good to see them as 2 different approaches.

To learn more about Sans-IO you could read...

### Conclusion

We've seen how modelling your I/O in your application has long lasting consequences and can change fundamentally how you need to handle mutable state.

I believe for I/O heavy applications, probably the reactor pattern will be the easiest to maintain. But this isn't neccesarily a "xor" situation.

You can mix and match as needed, channels are I/O events for your reactor. Mutexes can be made to work along with any of these models, and I haven't touched on atomics.

What I hope you take away from this article, is that, particularly in Rust, you can fundamentally chose how you approach shared mutable state by chosing how to model I/O. And also a better understanding of the reactor pattern which is normally relegated to executor implementation but it can be used in user-facing code.

---

[^1]: https://draft.ryhl.io/blog/actors-with-tokio/
[^2]: In fairness, deadlocks are still possible, if 2 tasks are waiting from one another preventing from making any other work.
