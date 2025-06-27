---
slug: sharing-io-resources
title: Using the reactor pattern to share mutable state
authors: [conectado]
tags: [async, rust, futures, polling, concurrency, IO]
date: 2025-06-20
draft: true
---

## Introduction

Correctly sharing mutable state in concurrent code is a pain, this is true regardless of the language.

In Rust it can be even more frustrating, in an effort to prevent you from shooting yourself in the foot it enforces XOR-aliasing rules that prevents simply sharing `&mut` references between tasks, additional safety mechanisms require tasks to be `'Send` and `'static`.

This does prevent a lot of bugs and UB but you can still be left in a very poor situation, due to deadlocks that can't be prevented by the borrow checker of forcing extra copies and/or allocations.

This is not to say that Rust's approach is bad or even misguided, on the contrary it's my favored approach, but I believe the key to untangle the potential mess of managing state in these situations is intelligently modelling the I/O resources of your application.

As the last sentence suggest I'll be focusing on I/O-bound applications, and that's in fact the case, I'll not focus on CPU-bound task that need to execute parallely.

What I'll try to focus on is the reactor pattern, showing how it can prevent some of the pain points of other common approaches in Rust.

In order to show this I introduce a simple example that we will evolve with different modeling techniques for I/O concurrent code. It will give us some context to discuss the benefits and drawbacks of each of these patterns.

## Motivation

More often than not, when writing async code in Rust you are dealing with IO-bound tasks; Tasks which spend most of its execution time waiting for I/O instead of running in the CPU. 

![Structure of an IO-bound task](io-bound-task.png)

`await` is the way to signal the compiler the point where you want to suspend execution until that IO-event happen. The goal is that tasks can collaboratively share CPU resources with other tasks, so that they can continue work, while suspended.

IO-bound apps which are mostly composed of IO-bound tasks benefit greatly from this, otherwise, at any point where the application would need to wait for an IO-event it'd prevent the whole thread from doing any more work.

<!-- Diagram here on how tasks share execution-->

Since calling `await` suspends the execution of a task you need to spawn a new one to allow other work to continue, the new task can outlive the spawnee so it requires the spawned future to be `'static`.

This means, as I mentioned before, that you can't hold an `&mut` reference to the state in your new task. Even if you use a `LocalSet` the `'static` requirement is still there. This means to share mutable state you need to either use `Mutex` or a `channel`.

We will explore the limitations of these two approaches in the next sections. 

Yet, if we could wait concurrently for IO-events in a single task, we could run the whole IO-bound app in that task. That's the idea behind the reactor pattern. 

There are some limitations and drawbacks to this approach which we will discuss it in later sections. However, the benefit in having all IO share a single task it that you can have `&mut` access to the state without any syncronization mechanism.

So my idea, with the following example is to evolve it with the different more common synchronization mechanisms used in Rust. To show how they contrast with the reactor pattern and give you the tools to decide which will be best for your particular case.

## The toy problem

This is a small and simplified, yet almost realistic, scenario. I'll pick some artificial constraints just to save us from some error handling and a few lines of code so we can focus on the core of the issue, namely, sharing mutable state across IO events.

> [!NOTE]
> All the numbers are sent in the wire in network-order.

We will be writing a server that clients can connect to and subsequently exchange messages between them, through it.

<!-- Diagram of the problem -->

Clients will connect over a TCP socket to the server, and they will immediatley be assigned an id, which will be sent as a 4 bytes response.

<!-- Diagram of the ID message -->

The clients can exchange the ID over a side channel, and then use it to craft messages directed to other clients.

<!-- Diagram of the Message -->

To send a message, a client sends the recipient’s ID, followed by the message bytes, ending with a null byte (\0).

The server, once a complete message is read will forward the bytes to the corresponding peer, without including its ID. 

<!-- Full sequence diagram -->

We will assume these unrealistic simplifications.

* Every client is well-behaved and will never abuse the protocol
  * This also means clients will always send complete messages and won't start a new one without finishing the one before
  * Every message the client send conform to the protocol and the id of the client it wants to communicate with is always valid.
* There are no OS errors
* Once a client connects it never disconnects
* A client will never sends a message to itself
* The Server once started will never stop

This protocol could cause the clients to recieve segmented messages without possibility to distinguish between them but let's ignore that too.

## Tests

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

## Implementations

### Mutex

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
