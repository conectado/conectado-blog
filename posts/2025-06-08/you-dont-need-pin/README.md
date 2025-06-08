---
slug: poll-style-futures
title: You don't need `async`
authors: [conectado]
tags: [async, rust, futures, polling, concurrency]
date: 2025-06-08
---

## When and why we use `async`

In Rust, we normally use `async` functions to express concurrency. A function that's marked as `async` is a function which can be paused and resumed at a later time when an event signals that's ready to continue.

There's a lot written on how this works[^1][^2], so I'll not dive into this.

[^1]: https://doc.rust-lang.org/stable/book/ch17-00-async-await.html
[^2]: https://rust-lang.github.io/async-book/

However, most runtimes provide functions on how to compose these futures in a way that they can be executed in parallel, in other threads.

A lot of the time this isn't needed, especially when I/O is involved there's little to gain in parallel execution, what you want is to await an event and continue working on other futures. But this still requires using primitives such as Mutexes due to async runtimes being designed for potential parallelism.

So here I'll lay out what I call the `Poll`-style future which can save you a lot of pain when dealing with single-threaded concurrent execution.

## An `async` problem

Let's say you want to await 2 different events that update shared state, in turn those events also depend on shared state.

For example, the events could be sleep, each time that it finishes it updates a counter, and the sleep time is the number of the counter in seconds.

```rs
async fn event_a(counter: &mut u64) {
    tokio::time::sleep(std::time::Duration::from_secs(*counter)).await;
    *counter += 1;
}

async fn event_b(counter: &mut u64) {
    tokio::time::sleep(std::time::Duration::from_secs(*counter)).await;
    *counter += 1;
}
```
We want the counter to be shared between both events.

This is an incredibly artificial problem, but the sleep can be any async operation and counter can be any shared-state.

Now with typical async we could write something like:

```rs
#[tokio::main]
async fn main() {
    // Records the number of times event A or event B happens.
    let mut counter = 0;

    loop {
        // We wait for event A to occur.
        event_a(&mut counter).await;
        // Sequentially we wait for event B to finish.
        // This is not the behaviour we want as it waits for the previous event to finish.
        event_b(&mut counter).await;
        
        // Giving some kind of stop point to the program so it eventually finishes.
        if counter > 5 {
            break
        }
    }
}
```

So this is wrong, this forces event B to always execute after event A, there's no concurrency between events.

So alternatively we can use `future::select` from the `futures` lib. Note that we use `future::select` from the futures crate rather than tokio's `select!` macro to avoid additional complexity but it's another option.

```rs

#[tokio::main]
async fn main() {
    // Records the number of times event A or event B happens.
    let mut counter = 0;
    loop {
        futures::future::select(event_a(&mut counter), event_b(&mut counter)).await;
        
        if counter > 5 {
            break
        }
    }
}
```

If you simply do this you get some lengthy `Pin` error that can be easily solved like this:

```rs
#[tokio::main]
async fn main() {
    // Records the number of times event A or event B happens.
    let mut counter = 0;
    loop {
        let ev_a = std::pin::pin!(event_a(&mut counter));
        // Second mutable borrow as we haven't dropped `ev_a` yet.
        let ev_b = std::pin::pin!(event_b(&mut counter));
        // Wait for both futures concurrently.
        futures::future::select(ev_a, ev_b).await;
        
        if counter > 5 {
            break
        }
    }
}
```

It's very plain to see that there are 2 mutable borrows at the same time, so to fix this we need to introduce a `Mutex` here.


```rs
use std::sync::Mutex;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Records the number of times event A or event B happens.
    let counter = Arc::new(Mutex::new(0));
    loop {
        let ev_a = std::pin::pin!(event_a(counter.clone()));
        let ev_b = std::pin::pin!(event_b(counter.clone()));
        futures::future::select(ev_a, ev_b).await;
        
        if *counter.lock().unwrap() > 5 {
            break
        }
    }
}

async fn event_a(counter: Arc<Mutex<u64>>) {
    let c = *counter.lock().unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(c)).await;
    *counter.lock().unwrap() += 1;
}

async fn event_b(counter: Arc<Mutex<u64>>) {
    let c = *counter.lock().unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(c)).await;
    *counter.lock().unwrap() += 1;
}
```

This of course adds a lot of complexity, when locking you need to be careful to not force an event to wait for the other, if the state was more complex you need to prevent deadlocks and most importantly, there's this question of:

After a loop, will both events increase the counter? Can it ever happen?

You might know the answer (no) but it's not obvious here so what can we do?

### `Poll`-style saves the day

As a quick refresher the `Future` trait is

```rs
pub trait Future {
    type Output;

    // Required method
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}
```

The async runtime calls `poll` on the future when it's awaited, and if it returns `Ready` you get the value within `Ready`. If it's `Pending` the runtime puts it in the backburner, until an event signals to the runtime it can continue, so it calls `poll` again in the hopes to make more progress. 

So with the correct future we could write something like this.

```rs
struct FutureThatCountsEvents { ... }
impl Future for FutureThatCountsEvents { ... }

#[tokio::main]
async fn main() {
  FutureThatCountsEvents {}.await
}
```


Now how would that look in this case?

```rs
use std::pin::Pin;
use std::task::{Context, Poll};
use std::future::Future;
use tokio::time::{Duration, Interval};

// This future manages the state and both events
// We use `Interval` instead of `sleep` to simplify the code so we don't have to recreate the sleep each iteration
// but this can be easily swapped back.
struct FutureThatCountsEvents {
    // The state
    counter: u64,

    // The events generating state and their internal state
    event_a: Interval,
    event_b: Interval,
}

impl FutureThatCountsEvents {
    fn new() -> Self {
        // Some trivial initialization.
        let event_a = tokio::time::interval(Duration::from_secs(1));
        let event_b = tokio::time::interval(Duration::from_secs(1));
                
        Self {
            counter: 0,
            event_a,
            event_b,
        }
    }
}

impl Future for FutureThatCountsEvents {
    type Output = ();
    
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Get mutable access through Pin
        let this = self.get_mut();
        
        loop {
            // Is event A ready?
            if let Poll::Ready(_) = this.event_a.poll_tick(cx) {
              // If so update state
              this.counter += 1;
              continue;
            }
    
            // Is event B ready?
            if let Poll::Ready(_) = this.event_b.poll_tick(cx) {
              // If so update state
              this.counter += 1;
              continue;
            }
    
            // If the counter reached the desired target finish execution.
            if this.counter > 5 {
              return Poll::Ready(());
            }

            // We haven't finished our work but we have registered the waker with event a and event b so either of those will make the runtime poll again.
            return Poll::Pending;
        }
    }
}

#[tokio::main]
async fn main() {
    FutureThatCountsEvents::new().await;
}

```

This is way clearer! On one side, no `Mutex`. And it's also very clear when the counter actually increases, what happens when one future finishes and the other doesn't. And this also gives us control if we wanted to fine-tune that behavior.

Here we use `Interval` instead of `Sleep` to simplify the code - since sleep futures complete after one use, we'd need to recreate them each iteration. The core polling pattern remains identical.

There's still room for bugs, for example if you forget to `loop`, you won't register the waker within `cx` again to be woken up at a later time. And the future will forever sleep 😔. So be careful with that, always make sure that if you return `Pending` you've registered some waker; In most cases it just means always calling at least one other `poll` function that also returns pending.

But this can also be made simpler, for starters, the `Pin` does look weird...

## Futures do actually need `Pin`

So in the example for `FutureThatCountsEvents` there's that first part: `let this = self.get_mut()` that requires to deal with `Pin`.

This is simple in this case but once you get more state it can become a bit verbose. There are plenty of excellent resources explaining why futures need `Pin`[^3][^4][^5][^6][^7].
But the good news is that you can avoid thinking about it altogether when writing your future's business logic. At least as long as you are only dealing with `'unpin` values, and normally you can get away with fully `'unpin` state.

[^3]: https://fasterthanli.me/articles/pin-and-suffering
[^4]: https://blog.cloudflare.com/pin-and-unpin-in-rust/
[^5]: https://doc.rust-lang.org/std/pin/index.html#address-sensitive-values-aka-when-we-need-pinning
[^6]: https://doc.rust-lang.org/book/ch17-05-traits-for-async.html?highlight=pin#the-pin-and-unpin-traits
[^7]: https://rust-lang.github.io/async-book/04_pinning/01_chapter.html


### How to avoid `Pin`

What's more, you don't even need to write a `Future` as in a struct that implements `Future`. There's an even cleaner way in this case, which can be applied in multiple contexts. With `std::future::poll_fn` you can get a context without a future and it returns an `await`able future.

So now it can look like this.

```rs
use tokio::time::Interval;
use std::task::{Context, Poll};
use tokio::time::{Duration};

// Only the state of the events you want to await.
struct Events {
    a: Interval,
    b: Interval,
}

impl Events {
    fn new() -> Events {
        Events {
            a: tokio::time::interval(Duration::from_secs(1)),
            b: tokio::time::interval(Duration::from_secs(1)),
        }
    }
}

// This is basically the same but now it's a free-standing function
fn event_counter(counter: &mut u64, events: &mut Events, cx: &mut Context<'_>) -> Poll<()> {
    loop {
        if let Poll::Ready(_) = events.a.poll_tick(cx) {
          *counter += 1;
          continue;
        }

        if let Poll::Ready(_) = events.b.poll_tick(cx) {
          *counter += 1;
          continue;
        }
        
        if *counter > 5 {
          return Poll::Ready(());
        }
        
        return Poll::Pending;
    }
}

#[tokio::main]
async fn main() {
  let mut counter = 0;
  let mut events = Events::new();
  // `poll_fn` provides us with a context and converts our function into a future that can be `await`ed
  std::future::poll_fn(move |cx| event_counter(&mut counter, &mut events, cx)).await;
}
``` 

Save for the `poll_fn` which is outside the main logic this looks pretty much like single-threaded code.

No need for `Mutex` or understanding how `select` works, and definitely not the hellish landscape that's the `select!` macro if there were more events to deal with.

Now, there's no `Pin` just mutable reference to state, easy to reason about, add debug statements and compiler breakpoints.

## Conclusion

This blog post was heavily inspired by [Firezone's sans-IO article](https://www.firezone.dev/blog/sans-io). Go read that! It's an amazing application of this pattern in a useful context.

It shows how it can be used for managing state in complex applications without dealing with contentions when you need single-thread concurrency.

For single-threaded concurrent operations, with shared state, this might be a better approach. Reduced overhead, easy-to-reason-about, generally cleaner. But when you need parallel execution or there's no need for shared state, this might not be the best approach, there are plenty of pitfalls when writing the polling loops that one needs to be careful about. 

But it's another weapon in your arsenal which should be considered. So next time you are reaching for `Arc<Mutex<T>>` in async code, consider if it could instead be written in `Poll`-style.
