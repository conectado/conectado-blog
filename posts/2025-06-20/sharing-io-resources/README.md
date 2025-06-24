---
slug: sharing-io-resources
title: Ways of sharing IO resources in async code
authors: [conectado]
tags: [async, rust, futures, polling, concurrency, IO]
date: 2025-06-20
draft: true
---

## Introduction

Correctly sharing mutable state in concurrenct code is a pain, this is true regardless of the language.

In Rust it can be even more frustrating, in an effort to prevent you from shooting yourself in the foot it enforces XOR-aliasing rules that prevents you from simply sharing `&mut` references to state between tasks and additional safety mechanisms like require `'Send` and `'static` lifetime.

It does prevent a lot of potential mistakes but you can still be left in a very poor situation, due to deadlocks, extra copies or allocations that can't all be captured by the borrow-checker and type-safety.

This is not to say that Rust's approach is bad or even misguided, on the contrary it's my favored approach, but I believe the key to untangle the potential mess of managing state in these situations is intelligently modelling the I/O resources of your application.

The last sentence suggest I'll be focusing on I/O-bound applications, and that's in fact the case, I'll not focus on CPU-bound task that need to execute parallely.

In this post I introduce a simple example that we will evolve with different modeling techniques for I/O concurrent code, that will give us some context to discuss the benefits and drawbacks of each of these.

## Motivation

More often than not, when writing async code in Rust you are dealing with IO-bound tasks.

This means a set of tasks which spend most of its execution time waiting for I/O instead of running in the CPU. 

![Structure of an IO-bound task](io-bound-task.png)

By using `await` you signal the point where you want to suspend execution until that event happen. The goal is that tasks can collaboratively share the CPU thread while waiting for an IO-event.

IO-bound apps often don't benefit as much from parallelism as CPU-bound apps, instead they care mostly about concurrency. There's an exception to this rule, when there're enough IO-events happening that having a single thread to schedule all tasks stalls the execution of already ready tasks.

Particularly, in a runtime like Tokio, the executor can either function in single or multi-threaded mode. When tasks are spawned, the runtime will schedule them accordingly and for the most part `async` functions can ignore what's the configuration of the runtime.

<!-- Diagram here on how tasks share execution-->

This means that tasks need to be `'Send`, this can be prevented by using [`LocalSet`](https://docs.rs/tokio/latest/tokio/task/struct.LocalSet.html), a set of tasks that's promised to be run in the same thread removing the requirements of `'Send`.

I'll not focus on this, instead the other consequence of this model of encapsulating computations of signals from the IO as independent task that will be run at a later time have the requirement of the task being `'static` which means, you can keep `&mut` references to an state shared by all tasks. 

With the following example, I'll try to show how that can be inconvinient and different models that we can use to deal with this.

## The problem

This is a small and simplified yet realistic scenario. The simplifications though very artificial are there to save us some error handling and other code-branches while still illustrating how to handle futures.

All the numbers are sent in the wire in network-order.

We will writer a "Router", each client connects to the router over TCP and is assigned an id, that's immediatley return over that same socket.

Once a client receive this id  it can be shared with any peer through a side-channel. And peers can use this ID to send messages to any other clients connected to the router.

<!-- Diagram of the problem -->

The ID is always returned immediatley from the router to the client and it it's always a 4-bytes message that contains the ID as u32.

<!-- Diagram of the ID message -->

At any point afterwards any client can send a message to another by sending a message composed of the client's ID followed by any message terminated by a nul-byte

<!-- Diagram of the Message -->

This message will be forwarded to the destination client after being stripped of the ID header.

<!-- Diagram of the forwarded message -->

We will assume these unrealistic simplifications.

* Every client is well-behaved and will never abuse the protocol
  * This also means clients will always send complete messages
* There are no OS errors
* Once a client connects it never disconnects
* A client will never sends a message to itself

This protocol could cause the clients to recieve segmented messages without possibility to distinguish between them but let's ignore that too.

## Solutions

### Tests

First, let's write some test to get a feeling of how the protocol should behave.

Just something simple to show expectations, not a way to catch edge cases.

For our first implementation the interface will be an struct:

```rs
struct Router;

impl Router {
  pub fn new() -> Router;
  pub async fn handle_connections(self: Arc<Self>) -> tokio::task::JoinHandle<()>;
}
```

We will evolve the signatures slightly in the different implementations but the test can be trivially fixed.

Anyways, each implementation can be found in a directory that'll be linked in each section of the article if you want to see the details

So the test will be:

```rs
#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    use crate::Router;

    #[tokio::test]
    async fn it_works() {
        const INT_MSG1: &str = "hello, number 2\0";
        const INT_MSG2: &str = "hello back, number 1\0";

        let router = Arc::new(Router::new());
        router.handle_connections().await;

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
```

Simply put:

1. We start the router in the background
1. Create to sockets and connect each to the router getting the ids in response
1. Send a message on one socket to the other
1. Assert that we got the message on the other end


### Naive

> [!NOTE]
> The code for this example can be found in the [naive](./naive) directory

A first approach one can make is using `Mutex` and it looks a bit like this:

```rs

#[derive(Default)]
struct Router {
    connections: tokio::sync::Mutex<Vec<OwnedWriteHalf>>,
}

impl Router {
    pub fn new() -> Router {
        Default::default()
    }

    pub async fn handle_connections(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();

        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let router = self.clone();

                tokio::spawn(async move {
                    router.handle_connection(socket).await;
                });
            }
        })
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

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            let mut connections = self.connections.lock().await;
            connections[dest as usize].write_all(&m).await.unwrap();
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
```

If you use this example with the previous test and run `cargo test` it passes.

In this version, we model the shared-state as a single map that any of the related task running can access by acquiring a lock.

This brings some problem with it, one if we had additional state that we want to mutate at the same time, it'd require careful handling to avoid deadlock.

In fact, in this particular case we need careful handling as I've used `socket.into_split()` but nothing prevents us from simply using a map like:

```rs
Mutex<Vec<Arc<tokio::sync::Mutex<TcpStream>>>>,
```

And write something like:

```rs
    async fn handle_connection(&self, socket: TcpStream) {
        let socket = Arc::new(tokio::sync::Mutex::new(socket));
        let id = {
            let mut connections = self.connections.lock().unwrap();
            connections.push(socket.clone());
            connections.len() - 1
        };

        socket.lock().await.write_u32(id as u32).await.unwrap();
        socket.lock().await.flush().await.unwrap();

        let mut buffer = BytesMut::new();

        loop {
            let n = socket.lock().await.read_buf(&mut buffer).await.unwrap();
            assert!(n != 0);

            let Ok((dest, m)) = parse_message(&mut buffer) else {
                continue;
            };

            let conn = {
                let connections = self.connections.lock().unwrap();
                connections[dest as usize].clone()
            };

            // Here we deadlock!
            let mut conn = conn.lock().await;
            conn.write_all(&m).await.unwrap();
        }
    }
```

This creates a deadlock the first time a client tries to message another! when doing `conn.lock()` before locking the socket to write, even when the destination is not itself - Remember we prohibited that - we have another task, that might be running in paralel, holding a mutex trying to read from the same connection. 

<!-- Add a diagram illustrating the contention -->

But that's not all, even if you manage to avoid deadlocks this isn't very good, there's still head-of-the-line blocking, we need to hold a lock as long as it takes one socket to write, this means only one client can write at a time, slowing down the whole processing pipeline. A slow client can hold the queue for a very long time, and even if there's no slow client at some point this doesn't scale.

<!-- Add a diagram showing how a task is making the other wait? -->

For anyone familiar with async coding in Rust this is expected, particularly, when sharing IO resources there's another very recommended model.


#### Further reading 

* [Tokio's Shared State docs](https://tokio.rs/tokio/tutorial/shared-state)
* [Alice Rhyl's Shared mutable state in Rust](https://draft.ryhl.io/blog/shared-mutable-state/)

### CSP-style (Actor model)

> [!NOTE]
> The code for this section can be found in the [csp](./csp) directory.

So the classic way to avoid this pitfall is to use channels.

There are actually 2 patterns that use the channel primitive, CSP-style and actor model. The differences are subtle and they are not the focus of this article.

Instead of having a mutex to share state among connections, we have a single task that manages the state, therefore a Mutex isn't needed, instead we use a channel to send messages to that task.

It looks like this:

```rs
struct Router {
    tx: mpsc::Sender<Message>,
}

impl Router {
    pub fn new() -> Router {
        let (tx, rx) = mpsc::channel(100);
        tokio::spawn(message_dispatcher(rx));
        Router { tx }
    }

    pub async fn handle_connections(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();

        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let router = self.clone();

                tokio::spawn(async move {
                    router.handle_connection(socket).await;
                });
            }
        })
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

This is very nice, as there's no mutex and thus no deadlocks, but still, head-of-the-line blocking is there!

After receiving a message in the `message_dispatcher` we call `connections[id].write_all().await` that blocks until we write to that connection preventing us from handling the next message.

We could have `connections` be a `Vec<Arc<Mutex<OwnedWriteHalf>>>` and spawn a new task where we write to that owned write half instead of doing it in-task, but again, mutexes are a possible foot-gun.

So we could instead apply a channel per writing half too. We can change the `message_dispatcher` to something like this.

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

enum Message {
    New(OwnedWriteHalf),
    Send(u32, Bytes),
}
```

> [!NOTE]
> The full code for this version can be found in the [csp-channel-per-writer](./csp-channel-per-writer) directory.

This is a bit better, channel's `send` with complete immediatley except when the channel is full. But it still has some drawbacks.

For one, it could be full, a very slow client could cause that then we are back again at a point where it's blocking. We could always fix it by spawning a new task to send to the client's dispatcher.

We could also do something like this:

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

Now this solves the problem, but then you have new tasks to manage, those can't access the shared state. So as you add more state and more tasks to complete the number of tasks and channels keep scaling.

This can quickly become hard to manage if you don't find the right abstractions.

And the problem with channel is that it's very prone to action-at-a-distance. Meaning, when you spawn a channel, the reciever and transmitter starts moving around and you can quickly lose where those came from.

Furthermore, when we created a channel, we set the limit for that channel, afterwards it creates backpressure. Which is a nice feature for bounded channel, but you need to tune that number and it's not always obvious what number should go there.

You could always use unbounded channels and save yourselve all these problems, this isn't recommended as unbounded memory allocation can lead to a new set of different problems.

### Reactor

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

#### Race (select)

There's an alternative to select in the crate `futures-util` called `Race`. It's very simlar to `select` but only gives us the result as a return value and it works with tuples.

Let's re-write the hand-rolled version using this.

#### Benefits and drawbacks
 
#### Sans-IO

### Conclusion
