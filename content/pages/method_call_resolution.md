+++
title = "Method call resolution for type parameters"
description = "Small writeup for method call resolution for call parameters"
date = 2025-07-08
draft = false
+++

The way method resolution works in Rust is specified in [this section](https://doc.rust-lang.org/stable/reference/expressions/method-call-expr.html) of the reference.

For a given type `T` where a method is being called, it follows these steps:

1. Start with a candidate list containing only `T` (`candidates := [T]`)
1. Try to dereference (`*`) the last element of the list(`last`)
1. If `*last` is valid, add it to the list (`candidates := candidates : [last]`) and go to the previous step. Else continue.
1. AAttempt [unsized coercion](https://doc.rust-lang.org/stable/reference/type-coercions.html#unsized-coercions), meaning vaguely, converting a sized type to its unsized variant (e.g. `[T; N]` to `[T]`)
1. For each element `t` on `candidates` add `&t` and `&mut t`
1. For each `t` in `candidates`:
    1.  Check if `t` inherently implements  (as in `impl t { .. }` w/o `<trait> for`) the method. If so, call it and finish.
    1.  If `t` is a [type parameter,](https://doc.rust-lang.org/stable/reference/types/parameters.html) check all methods on its bounded traits; if there's only one, call it and finish. If there are multiple, error out and finish; if there's none, continue.
    1.  Check all methods on `t` visible trait implementations; if there's one, call it and finish. If there are multiple error out; if there are none, move on to the next step.
1. Error out since there's no method.

Roughly, those are the steps. In the reference it already talks about a potentially unexpected result; all methods on `&T` are searched before methods on `&mut T`, so if a trait is implemented on a type `&T` and `&mut T` has an inherent impl (see 6.i), it'll call the trait impl on `&T`.

There are multiple other writings I saw about how reference and dereference interact in this definition.

But I've never seen anything on what 6.ii being before 6.iii means. To put it in other words, the compiler searches traits constrained directly by the bounds on a type parameter before traits *implied* by that bound.

So in something like this.

```rs
struct Foo {}

trait Bar {
  fn bar(&self); // <--- This
}

trait Baz {
  fn bar(&self); // <--- And this have the same name
}

impl Bar for Foo {
  fn bar(&self) {
    println!("bar")
  }
}

impl<T: Bar> Baz for T {
  fn bar(&self) {
    println!("baz")
  }
}

fn g<T: Bar>(f: T) {
    f.bar();
}

fn main() {
  let f = Foo{};
  g(f);
}
```

`bar` is printed. Because `Bar` is bounded directly on `T`. While, of course, this compiles without a problem.

```rs
struct Foo {}

trait Bar {
  fn bar(&self); // <--- Now here the name is different 
}

trait Baz {
  fn baz(&self); // <--- From here
}

impl Bar for Foo {
  fn bar(&self) {
    println!("bar")
  }
}

impl<T: Bar> Baz for T {
  fn baz(&self) {
    println!("baz")
  }
}

fn g<T: Bar>(f: T) {
    f.baz();
}

fn main() {
  let f = Foo{};
  g(f);
}
```

Printing `baz`. Because `Baz` is implied by `Bar`. This leads to a small "quirk," if you want to call it so, wherein this next example doesn't compile, in contrast to the first one.

```rs
struct Foo {}

trait Bar {
  fn bar(&self); // <--- Here the name is the same
}

trait Baz {
  fn bar(&self); // <--- As the name here
}

impl Bar for Foo {
  fn bar(&self) {
    println!("bar")
  }
}

impl<T: Bar> Baz for T {
  fn bar(&self) {
    println!("baz")
  }
}

fn main() {
  let f = Foo{};
  f.bar();
}
```

Since now, both `Bar` and `Baz` are checked in the same step (6.iii) so they conflict with each other.
