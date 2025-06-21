---
slug: sharing-io-resources
title: Ways of sharing IO resources in async code
authors: [conectado]
tags: [async, rust, futures, polling, concurrency, IO]
date: 2025-06-20
draft: true
---

## Introduction

Correctly mutablly sharing state in concurrenct code is a pain, this is true regardless of the language.

In Rust, it can be even more frustrating, in an effort to prevent you from shooting yourself in the foot it enforces XOR-aliasing rules that can be hard to follow, even for experts.

It does prevent a lot of potential mistakes but you can still be left in a very poor situation, due to deadlocks, extra copies or allocations that can't all be captured by the borrow-checker.

This is not to say that Rust's approach is bad or even misguided, on the contrary it's my favored approach. But these last few years has led me to believe that the key to untangle the potential mess of managing state in these situations is intelligently modelling the I/O resources of your application.

The last sentence suggest I'll be focusing on I/O-bound applications, and that's in fact the case, I'll not focus on CPU-bound task that need to execute parallely.

In this post I introduce a simple example that we will evolve with different modeling techniques for I/O concurrent code, that will give us some context to discuss the benefits and drawbacks of each of these.

## Motivation

More often than not, when writing async code in Rust you are dealing with IO-bound tasks.

This means a set of tasks which spend most of its execution time waiting for I/O instead of running in the CPU. 

![IO-bound task](io-bound-task.png)

In fact, that's the idea behind the `await` syntax; It signals the point where you want to suspend execution until the event happen. 

The goal here is that a suspended task can yield the execution thread to another task that's ready for work.

IO-bound apps don't benefit as much from having more CPU-time, as they spend most of the time suspended, therefore a single-thread of execution is often enough.

The way that a runtime like tokio models this is: the executor can either be single threaded or multi-threaded, it shouldn't matter at all for whoever is implementing the buisness logic for computation.

<!-- Diagram here on how tasks share execution-->

The implementation will spawn tasks, as units of work that will be scheduled into whatever available thread, regardless if there's one or more, and it will free up the thread for other tasks whenever it's suspended waiting for an event(normally IO).

This is great, even if your tasks are mostly IO-bound they can still benefit from paralellism, if the io events come fast enough you might have a lot of task competing for execution time and being stalled.

Yet, it has a big downside, it makes both mutablly sharing state and specially IO resources way harder, even when using single-threaded mode.

In the following example my idea is to show you the different ways you can model your application to show the compiler that this is indeed running on a single thread, and you can have access to shared mutable without a Mutex..

## The problem

This is a small and simplified yet realistic scenario. The simplifications though artificial are there to save us some error handling and boilerplate.

We have a "Router", each client connects to the router and is returned an id, this id can be shared with any peer through a side-channel. After that each peer can send message to the other using this protocol:

<!-- Diagram of the problem -->
 
