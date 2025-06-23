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

But that's not all, even if you manage to avoid deadlocks this isn't very good, we need to hold a lock as long as it takes one socket to write, this means only one client can write at a time, slowing down the whole processing pipeline. A slow client can hold the queue for a very long time, and even if there's no slow client at some point this doesn't scale.

<!-- Add a diagram showing how a task is making the other wait? -->

For anyone familiar with async coding in Rust this is expected, particularly, when sharing IO resources there's another very recommended model.


#### Further reading 

* [Tokio's Shared State docs](https://tokio.rs/tokio/tutorial/shared-state)
* [Alice Rhyl's Shared mutable state in Rust](https://draft.ryhl.io/blog/shared-mutable-state/)

### CSP-style (Actor model)

So the classic way to avoid this is to use channels.

### Reactor

#### Hand-rolled future

#### Recoverying async/await

#### Benefits and drawbacks
 
