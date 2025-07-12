+++
title = "Fun with Pin UB"
description = "What could happen if we used Pin wrong"
date = 2025-07-11
draft = false
+++

So, let's say you have a clean and nice function like this.

```rs
use std::future::poll_fn;
use std::pin::Pin;
async fn foo() -> u32 {
    let mut x = 1;
    tokio::time::sleep(std::time::Duration::from_secs(0)).await;
    x+= 1;
    x
}

#[tokio::main]
async fn main() {
    let mut x = foo();
    let mut a = unsafe { Pin::new_unchecked(&mut x) };
    
    println!("{}", a.await);
}
```

Everything looks nice except for that `Pin::new_unchecked` which we created with unsafe (oh no), so we can do evil things.

I've read a lot about how `async` creates internal state machines that keep self-referential structs that, if moved, everything could be disastrous.

But how does this look? How can we fail to enforce the safety requirements of `Pin`, specifically, with futues? Let's see what happens if you move a pinned value after it's created and polled!

Now let's rewrite this in the following way.

```rs
use std::task::Poll;
use std::task::Context;
use std::pin::Pin;
async fn foo() -> u32 {
    let mut x = 1;
    tokio::time::sleep(std::time::Duration::from_secs(0)).await;
    x+= 1;
    x
}

#[tokio::main]
async fn main() {

    println!("{}", BadFuture.await);
}

struct BadFuture;

impl Future for BadFuture {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut x = foo();
        let mut a = unsafe { Pin::new_unchecked(&mut x) };
        let mut i = 0;
        loop {
            i += 1;
            dbg!(i);
            if let Poll::Ready(a) = a.as_mut().poll(cx) {
                return Poll::Ready(a);
            }
        }
    }
}
```

Something I didn't expect at all here is that `i` can go up to values in the hundreds!

But good, it loops at least more than once; now we can do crimes.

First let's try this.

```rs
use std::task::Poll;
use std::task::Context;
use std::pin::Pin;
async fn foo() -> u32 {
    let mut x = 1;
    tokio::time::sleep(std::time::Duration::from_secs(0)).await;
    x+= 1;
    x
}

#[tokio::main]
async fn main() {

    println!("{}", BadFuture.await);
}

struct BadFuture;

impl Future for BadFuture {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut x = foo();

        {
            let mut a = unsafe { Pin::new_unchecked(&mut x) };
            let _ = a.as_mut().poll(cx);
        }
        
        let mut x2 = x;
        
        let mut a = unsafe { Pin::new_unchecked(&mut x2) };
        let _ = a.as_mut().poll(cx);
        
        let mut i = 0;
        loop {
            i += 1;
            dbg!(i);
            if let Poll::Ready(a) = a.as_mut().poll(cx) {
                return Poll::Ready(a);
            }
        }
    }
}
```

For me, this caused an infinite loop. FUN!

The cause is UB. We moved the future, which contained a self-reference, which might now point to uninitialized memory, so now who knows what crazy things might happen.

Another case mentioned a lot in the `pin` docs is mem swapping; let's try that.


```rs
use std::mem;
use std::task::Poll;
use std::task::Context;
use std::pin::Pin;
async fn foo() -> u32 {
    let mut x = 1;
    tokio::time::sleep(std::time::Duration::from_secs(0)).await;
    x+= 1;
    x
}

#[tokio::main]
async fn main() {

    println!("{}", BadFuture.await);
}

struct BadFuture;

impl Future for BadFuture {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut x1 = foo();
        
        {
            let mut a = unsafe { Pin::new_unchecked(&mut x1) };
            let _ = a.as_mut().poll(cx);
        }
        
        let mut x2 = foo();
        
        mem::swap(&mut x1, &mut x2);
        
        let mut a = unsafe { Pin::new_unchecked(&mut x2) };
        let _ = a.as_mut().poll(cx);
        
        let mut i = 0;
        loop {
            i += 1;
            dbg!(i);
            if let Poll::Ready(a) = a.as_mut().poll(cx) {
                return Poll::Ready(a);
            }
        }
    }
}
```

Ah, now a much more expected `SIGSEV`. This means Rust is probably, actually, dereferencing uninitialized memory. But again, this is UB; you might see a different thing.

Now, let's try one more slightly different thing.

```rs
use std::mem;
use std::task::Poll;
use std::task::Context;
use std::pin::Pin;
async fn foo() -> u32 {
    let mut x = 1;
    tokio::time::sleep(std::time::Duration::from_secs(0)).await;
    x+= 1;
    x
}

#[tokio::main]
async fn main() {

    println!("{}", BadFuture.await);
}

struct BadFuture;

impl Future for BadFuture {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut x1 = foo();
        let mut x2 = foo();

        {
            let mut a = unsafe { Pin::new_unchecked(&mut x1) };
            let _ = a.as_mut().poll(cx);
        }
        
        mem::swap(&mut x1, &mut x2);
        
        let mut a = unsafe { Pin::new_unchecked(&mut x2) };
        let _ = a.as_mut().poll(cx);
        
        let mut i = 0;
        loop {
            i += 1;
            dbg!(i);
            if let Poll::Ready(a) = a.as_mut().poll(cx) {
                return Poll::Ready(a);
            }
        }
    }
}
```

Aha! Now we failed an assertion in Tokio. Specifically, this:

```
thread 'tokio-runtime-worker' panicked at /playground/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-1.45.1/src/runtime/time/wheel/mod.rs:233:17:
assertion `left == right` failed
  left: 106587145424240
 right: 1
```

[This is the line](https://github.com/tokio-rs/tokio/blob/tokio-1.45.1/tokio/src/runtime/time/wheel/mod.rs#L233) where it fails. It's the internals of how sleep is implemented, probably; it might have its own state that's address-dependent, so we're probably messing with that.

I'd be happy if someone knows the specifics. But it seems like it's not just our internal `x` that's the problem; it's also the future in `tokio::time::sleep`.

So, yeah, it's good to keep this in mind next time you're suffering with `Pin`.
