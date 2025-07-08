+++
title = "Surprised about drop order behavior"
description = "Write up about a surprise I had about drop order"
date = 2025-06-30
draft = false
+++

I was experimenting with the lifetime constraints for tokio channels, I wanted to understand how it enforces sane lifetimes for the type of the value sended.

<!-- more -->

So, my initial program was:

```rs
#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    let x = 1;
    tokio::spawn(async move {
        let Some(x) = rx.recv().await else { return; };
        println!("{x}");
    });
    tx.send(&x).await;
}
```

The error was:
```
  Compiling playground v0.0.1 (/playground)
error[E0597]: `x` does not live long enough
  --> src/main.rs:9:13
   |
4  |       let x = 1;
   |           - binding `x` declared here
5  | /     tokio::spawn(async move {
6  | |         let Some(x) = rx.recv().await else { return; };
7  | |         println!("{x}");
8  | |     });
   | |______- argument requires that `x` is borrowed for `'static`
9  |       tx.send(&x).await;
   |               ^^ borrowed value does not live long enough
10 |   }
   |   - `x` dropped here while still borrowed
```
So far so good, creating `channel` creates a tuple `(Sender<T>, Receiver<T>)` so if `T` is a reference `&'a U` `'a` must be the same for both, and since we moved `rx` inside the spawned task `'a` must be static. Great, this explains how it enforces the receiver will always get a valid reference.

But I got curious, and just tried to send a reference without a receiver. this shouldn't have the same problem.

```rs
#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    let x = 1;
    
    tx.send(&x).await
}
```

And got this error, which struck a bit strange to me.

```
error[E0597]: `x` does not live long enough
 --> src/main.rs:6:13
  |
4 |     let x = 1;
  |         - binding `x` declared here
5 |     
6 |     tx.send(&x).await;
  |             ^^ borrowed value does not live long enough
7 | }
  | -
  | |
  | `x` dropped here while still borrowed
  | borrow might be used here, when `tx` is dropped and runs the destructor for type `tokio::sync::mpsc::Sender<&i32>`
  |
  = note: values in a scope are dropped in the opposite order they are defined
```

This normally happens when you drop a value while it's still borrowed at the end of the scope, but since there's no further use for the value after line 6, rust should be able to drop the `Sender` earlier and everything should work.

Otherwise this should fail (and of course, it doesn't).

```rs
fn main() {
  let mut foo = Vec::new();
  let x = 1;
  foo.push(&x);
}
```

The following code however causes a similar error, since we use `foo` again so we can't drop `foo` at the end of the scope where we create the value `let x = 1`.

```rs
fn main() {
  let mut foo = Vec::new();
  {
    let x = 1;
    foo.push(&x);
  } // Can't drop here!
  let x = 2;
  foo.push(&x)
}
```

But was more similar my case looked more like the first one!

Well, I was quite stumped, I thought it might have to do with the `await` though it didn't make sense, and of course this still errored.

```rs
#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    let x = 1;
    
    let _future = tx.send(&x);
}
```
But anyone who has been paying attention to the errors is probably yelling at their screens right now: "IT MENTIONED DROP". 

What the error really meant is that due to the `Drop` implementation the compiler can't drop `x` before `tx`, which is the proper drop order as `x` was created after `tx` and `tx` uses `x` in its drop implementation.

We can see that this code causes the same error:

```rs
use std::fmt::Debug;
#[tokio::main]
async fn main() {
    let mut f = Foo::new();
    let x = 1;
    
    let _f = f.set(&x);
}

struct Foo<T: Debug> {
    x: Option<T>,
}

impl<T: Debug> Drop for Foo<T> {
    fn drop(&mut self) {
        println!("{:?}", self.x);
    }
}

impl<T: Debug> Foo<T> {
    fn new() -> Foo<T> {
        Foo {
            x: None,
        }
    }
    
    async fn set(&mut self, x: T) {
        self.x = Some(x);
    }
}
```

```
error[E0597]: `x` does not live long enough
 --> src/main.rs:7:20
  |
5 |     let x = 1;
  |         - binding `x` declared here
6 |     
7 |     let _f = f.set(&x);
  |                    ^^ borrowed value does not live long enough
8 | }
  | -
  | |
  | `x` dropped here while still borrowed
  | borrow might be used here, when `f` is dropped and runs the `Drop` code for type `Foo`
  |
  = note: values in a scope are dropped in the opposite order they are defined
```

And this actually compiles.

```rs
#[tokio::main]
async fn main() {
    let x = 1;
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    
    tx.send(&x).await;
}
```

So what's really going on here? Originally, I thought that it depended on `Drop` utilizing a the reference, but this still fails to compile.

```rs

#[tokio::main]
async fn main() {
    let mut f = Foo::new();
    let x = 1;
    
    let _f = f.set(&x);
}

struct Foo<T> {
    x: Option<T>,
}

impl<T> Drop for Foo<T> {
    fn drop(&mut self) {}
}

impl<T> Foo<T> {
    fn new() -> Foo<T> {
        Foo {
            x: None,
        }
    }
    
    async fn set(&mut self, x: T) {
        self.x = Some(x);
    }
}
```

`Drop` doesn't really do anything, so it just depends on the implementation of `Drop` existing. But wait a minute — `Vec` implements `Drop`.

So, yeah, I picked a bit of an unfortunate example with `Vec`, it's an special case. Looking at `Vec`'s [`Drop`](https://github.com/rust-lang/rust/blob/master/library/alloc/src/vec/mod.rs#L3830) implementation it uses a feature called `#[may_dangle]` and it's explained [here](https://doc.rust-lang.org/nomicon/dropck.html#an-escape-hatch), in fact that whole section explains very well what's going on here.

To put it simply here, whenever you hold a `&'a` reference, the lifetime of `'a` needs to be dropped after the struct(in drop order) if there's a `Drop` implementation for that struct. `#[may_dangle]` is an unsafe feature that allows you to indicate to the compiler that the reference isn't used in the `Drop` implementation. Using this you can write a version of this example that compiles.

```rs
#![feature(dropck_eyepatch)]

use std::fmt::Debug;
#[tokio::main]
async fn main() {
    let mut f = Foo::new();
    let x = 1;
    
    let _f = f.set(&x);
}

struct Foo<T: Debug> {
    x: Option<T>,
}

unsafe impl<#[may_dangle]T: Debug> Drop for Foo<T> {
    fn drop(&mut self) {}
}

impl<T: Debug> Foo<T> {
    fn new() -> Foo<T> {
        Foo {
            x: None,
        }
    }
    
    async fn set(&mut self, x: T) {
        self.x = Some(x);
    }
}
```

I think `#[may_dangle]` introduces a bit of a weird effect, you can normally tell the lifetime constraints of your values using only the type information of what you're using within the scope of your function. You only need to know that the type implements `Drop` for this case but with `#[may_dangle]` you need to know implementation details about `Drop`. So yeah... 

Edit: Thanks u/oconnor663 for [cluing me in](https://www.reddit.com/r/rust/comments/1lmqjce/comment/n09gfi9/?utm_source=share&utm_medium=web3x&utm_name=web3xcss&utm_term=1&utm_content=share_button) what's really happening here 
