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

This solves most of the problems; at some point back-pressure needs to be handled, and having the task that sends messages to the `client_dispatcher` be blocked seems good enough. However, one might want to stop reading packets as channels get filled; that way the TCP queues get full and new packets get dropped, and clients will automatically retransmit packets. Notice, however, this would require a channel back from the anonymous tasks that send the bytes to the `client_dispatcher` and then back from the `central_dispatcher` to `handle_connection`.

Another way we will require more channels is if we want to start handling errors on writing to the socket. Instead of being able to handle errors idiomatically in place in `central_dispatcher`, `client_dispatcher` would need a way to send the error back if that error would kill the connection.

Furthermore, each additional IO requires its own task; additionally, each piece of state requires either its own task or to be added to the `central_dispatch`. The first way would also require more channels for cross-communication between state owners.

All these channels and tasks add complexity. For one, tasks need to be managed and dropped eventually[^3]. But there's another complication: we lose co-location of IO and state by creating these channels. Channels do move around, so now when you see the place where state is managed, you have no idea where the events are produced. 

Take a look again at our `central_dispatcher`.

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

There's no way to know where `rx` comes from other than looking for the place where the channel is created, which can be very far up the stack. Then, to know where `Message` can be produced, you need to look at all the places where the corresponding `tx` moves to, which again can be very far down the stack.

Not having mutexes does simplify things a lot, but there's still complexity related to having multiple tasks. There's, however, a clue on how we could further simplify things. Remember that I said that if we had additional state, we could manage it in the `central_dispatcher` and have it act as a multiplexer between IO events. For example, if we had a new source of events, such as another socket, it can also use `tx` to inform the `central_dispatcher` of the events with a new `Message` variant. If that also required a new state, `central_dispatcher` can also manage that.

Let's say, for example, a new administration socket that can subscribe multiple clients to a "topic" that can look something like this. 

```rs
    async fn handle_administration_connection(&self, socket: TcpStream) {
        let (mut read, write) = socket.into_split();
        self.tx.send(Message::New(write)).await.unwrap();

        let mut buffer = BytesMut::new();

        loop {
            let n = read.read_buf(&mut buffer).await.unwrap();
            assert!(n != 0);

            let Ok((topic, clients)) = parse_admin_message(&mut buffer) else {
                continue;
            };

            self.tx.send(Message::NewTopic(topic, clients)).await.unwrap();
        }
    }

    async fn central_dispatcher(mut rx: mpsc::Receiver<Message>) {
        let mut writers = Vec::new();
        let mut topics = HashMap::new();
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
                Message::NewTopic(topic, clients) => {
                    topics.insert(topic, clients.iter().map(|c| writers[c].clone()).collect_vec());
                }
                Message::SendTopic(topic, message) => {
                    for w in topics.get(topic).unwrap() {
                        let w = w.clone();
                        tokio::spawn(async move {
                            w.send(items).await.unwrap();
                        });
                    }
                }
            }
        }
    }

```


{{ note(body="This code is merely illustrative, it's not written to be compiled.") }}

 Well, we still have multiple incoming and outgoing channels, but this shows that we can have a single task managing state while listening concurrently to multiple IO events. So perhaps, we can move the IO inside that same task and simplify things further. 

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

Now, this socket abstraction makes it all come together. Let's take a look at it.

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

    async fn read(&mut self) -> Event {
        self.reader.read_buf(&mut self.buffer).await.unwrap();
        Event::Read(&mut self.buffer)
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
                std::future::pending::<Infallible>().await;
            }

            self.writer.write_all_buf(&mut self.buffer).await.unwrap();
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
    fn next_event(&mut self) -> impl Future<Output = Event> {
        let listen = self
            .listener
            .accept()
            .map(|stream| Event::Connection(stream.unwrap().0));

        // Neither `Race` nor `select_all` works with empty vectors
        if self.read_connections.is_empty() {
            return listen.boxed();
        }

        let reads = self
            .read_connections
            .iter_mut()
            .map(|reader| reader.read())
            .collect::<Vec<_>>()
            .race();

        let writes = self
            .write_connections
            .iter_mut()
            .map(|write| write.advance_send())
            .collect::<Vec<_>>()
            .race();

        (listen, reads, writes).race().boxed()
    }
```

When there are no connections yet, the only possible event is a new connection emitted by the `listener`. Otherwise, we `race` among all the futures from all writers, readers, and the listener, and emit an event based on the first one to finish. [`race`](https://docs.rs/futures-concurrency/latest/futures_concurrency/future/trait.Race.html#tymethod.race) is a method provided by the `Race` trait. `race` is also fair[^4][^5], in the sense that none of the futures will hog the executor.

With this `next_event` in place, we can now handle all events concurrently in a single task, using this `handle_connections` function.


```rs
    async fn handle_connections(&mut self) {
        loop {
            let ev = self.next_event().await;
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
```

Notice how now we have `&mut self` access to the state; we can simply modify `read_connections` and `write_connections` as our IO generates events. And we have the benefit of concurrency provided by the runtime to execute all this IO concurrently. I think this is pretty neat.

Yet, we can go further down this path. First, let's get something out of the way: we can make this approach zero-copy with a few modifications. We change `WriteSocket` to this.

```rs
struct WriteSocket {
    buffers: Vec<Bytes>,
    writer: OwnedWriteHalf,
}

impl WriteSocket {
    fn new(writer: OwnedWriteHalf) -> WriteSocket {
        WriteSocket {
            buffers: Vec::new(),
            writer,
        }
    }

    async fn advance_send(&mut self) -> Event {
        loop {
            let Some(buffer) = self.buffers.first_mut() else {
                std::future::pending::<Infallible>().await;
                unreachable!();
            };

            self.writer.write_all_buf(buffer).await.unwrap();

            self.buffers.pop();
        }
    }

    fn send(&mut self, buf: Bytes) {
        self.buffers.push(buf);
    }
}
```

Now, `send` no longer copies data; instead, it keeps it in an internal vector. Then with a slight modification to `handle_connection` we can make this all work.
```rs
    pub async fn handle_connections(&mut self) {
        loop {
            let ev = self.next_event().await;
            match ev {
                Event::Read(bytes_mut) => {
                    let Ok((i, d)) = parse_message(bytes_mut) else {
                        continue;
                    };
                    self.write_connections[i as usize].send(d);
                }
                Event::Connection(tcp_stream) => {
                    let (r, w) = tcp_stream.into_split();
                    let mut write_sock = WriteSocket::new(w);
                    write_sock.send(Bytes::from_owner(
                        (self.write_connections.len() as u32).to_be_bytes(),
                    ));
                    self.read_connections.push(ReadSocket::new(r));
                    self.write_connections.push(write_sock);
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
    write_buffers: Vec<Bytes>,
    read_buffer: BytesMut,
    stream: TcpStream,
    waker: Option<Waker>,
}

impl Socket {
    fn new(stream: TcpStream) -> Socket {
        Socket {
            write_buffers: Vec::new(),
            read_buffer: BytesMut::new(),
            waker: None,
            stream,
        }
    }

    fn send(&mut self, buf: Bytes) {
        self.write_buffers.push(buf);
        let Some(w) = self.waker.take() else {
            return;
        };

        w.wake_by_ref();
    }

    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.write_buffers.is_empty() {
            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        loop {
            ready!(self.stream.poll_write_ready(cx)).unwrap();

            let buffer = self.write_buffers.first_mut().unwrap();

            let Ok(n) = self.stream.try_write(buffer) else {
                continue;
            };

            buffer.advance(n);

            if buffer.is_empty() {
                self.write_buffers.pop();
            }

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

If you take a careful look at this, it's very similar to the previous version. `poll_read` tries to read from the `TcpStream` if there are bytes available it'll return a reference to them. Otherwise, it'll have its waker registered and return pending.

`poll_send` is a bit more complicated, just because I wanted to keep the zero-copy from the latest version. First, if there's nothing to write it saves the waker so that when there's something to write we can be polled again. Then, if there's something to write, we check if the socket is ready for writing, otherwise we register the waker and return pending. If we manage to write, we update the buffers.

Notice in both these functions we check for readiness, then we try to read or write, and normally you would handle explicitly the case of `WouldBlock`. In this case we assume there're no OS errors, so it must be a `WouldBlock`, then we need to re-register as the try_read/try_write doesn't do that for us, that's why we `continue`. 

Lastly, `send` is pretty simple, we enqueue the buffer for writing and if there was a waker registered by `poll_send` previously, we wake it up, so that `poll_send` can be called again.

With this in place we will add this function to the server.

```rs
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Poll::Ready(Ok((connection, _))) = self.listener.poll_accept(cx) {
            let mut socket = Socket::new(connection);
            let id = self.connections.len() as u32;
            socket.send(Bytes::from_owner(id.to_be_bytes()));
            self.connections.push(socket);
            cx.waker().wake_by_ref();
        }

        for conn in &mut self.connections {
            if conn.poll_send(cx).is_ready() {
                cx.waker().wake_by_ref();
            }
        }

        for i in 0..self.connections.len() {
            let Poll::Ready(buf) = self.connections[i].poll_read(cx) else {
                continue;
            };

            cx.waker().wake_by_ref();

            let Ok((dest, b)) = parse_message(buf) else {
                continue;
            };

            self.connections[dest as usize].send(b);
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

And there we've it, we're manually polling the futures. If we were handling errors, `poll_send` couuld surface it directly as `Poll<io::Result<()>>`, for example. And then in the main loop we can handle it by doing `self.connection.remove[dest]`[^6].

#### Benefits and drawbacks

#### Sans-IO

Sans-IO is a model for network protocol, where you implement them in an IO-agnostic way, as a state machine that's evolved by outside events.

You might immediately see the link with the single-task model, where we seggregate the events emitted by the IO to the state of the application.

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
[^3]: https://draft.ryhl.io/blog/actors-with-tokio/ has a very good explanation on how this can be done!
[^4]: https://github.com/yoshuawuyts/futures-concurrency/issues/85
[^5]: https://github.com/yoshuawuyts/futures-concurrency/pull/104
[^6]: Of course there should be extra care to no longer use the index into the connections like we were before, a Map ofIDs to sockets would work much better.
