+++
title = "The scary and surprisingly deep rabbit hole of Rust's temporaries"
description = "Way too many words about how Rust's temporaries work"
date = 2025-06-03
draft = false

[taxonomies]
tags = ["rust", "programming", "memory-management", "temporaries", "lifetime", "borrow-checker"]
categories = ["rust", "deep dives"]

[extra]
toc = true
+++

'twas a lovely Sunday afternoon when I decided to do some [fun rust quiz](https://dtolnay.github.io/rust-quiz/37),
but a subtle mistake in the explanation of the answer ended up in hours of research
and a PR.

<!-- more -->

After learning so much about the niche Rust topic of temporaries, I'm determined to also burden you with this unholy knowledge, if you're just willing to read a few thousand words on it.


> **Spoiler alert!**
>
> While I won't talk about the exact problem in [Quiz #37](https://dtolnay.github.io/rust-quiz/37)
> I'll get pretty close to it. So you might want to try the quiz for yourself beforehand.
>
> And if you want to know what used to be wrong about the explanation to that problem before [here's my PR](https://github.com/dtolnay/rust-quiz/pull/92)

# Temporaries

If you've been programming in Rust for a while you must have seen this term used in an error like:

```
error[E0716]: temporary value dropped while borrowed
```

So, what if I told you temporary values don't exist?

{{ responsive_image(src="/images/temporaries-rabbit-hole/memes/morpheus.png", alt="Morpheus what if I told you meme") }}

Got your attention? Good.

This is, of course, too clickbaity. It's just a mismatch between how the term is used by the compiler errors
and how it's defined in the [reference](https://doc.rust-lang.org/stable/reference/expressions.html#r-expr.temporary).

But don't worry about reading the reference just yet; we will untangle that definition in a moment, so let's dive into it.

## An incredibly deceptively "simple" example

 Take a look at this example.

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {
        print!("foo");
    }
}

fn main() {
        let x = Some(&Foo);
        x;
}
```

When we try to compile it, we get this error.

```
error[E0716]: temporary value dropped while borrowed
  --> src/main.rs:9:23
   |
9  |         let x = Some(&Foo);
   |                       ^^^ - temporary value is freed at the end of this statement
   |                       |
   |                       creates a temporary value which is freed while still in use
10 |         x;
   |         - borrow later used here
   |
help: consider using a `let` binding to create a longer lived value
   |
9  ~         let binding = Foo;
10 ~         let x = Some(&binding);
   |

For more information about this error, try `rustc --explain E0716`.
```

Somehow, when we declare `Foo` in `let x = Some(&Foo)` there's really no variable to hold it. So, it's created temporarily, living only until the end of the `let` statement. After that, it's dropped, so when we try to invoke `x` again, the compiler disallows it, because it'd contain a dangling reference.

Does this make sense? No? Kinda? Maybe we should just get the `Some` out of the way, as it doesn't seem to be relevant for this discussion, and it shouldn't change anything.

So now we can rewrite it like this.

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {
        print!("foo");
    }
}

fn main() {
        let x = &Foo;
        x;
}
```

Now that we have less noise, the error is a bit clearer— wait, this compiles!

{{ responsive_image(src="/images/temporaries-rabbit-hole/memes/what.gif", alt="huh??") }}

Okay, okay, let's go back to the previous version, as it seems like there's no temporary value in this one.

So we had something like this, right?

```rs
struct Foo;

fn main() {
    let x = Some(&Foo);
    x;
}
```

And the error was... wait, I forgot the `Drop` impl for `Foo`. Well, it shouldn't matter as the `Drop` only printed `"foo"`. The error was—

Wait this compiles...

{{ responsive_image(src="/images/temporaries-rabbit-hole/memes/go-on.png", alt="go-on") }}

Now I better tell you what's really going on here.

## What's going on

Let's start at the beginning. The reference manual defines temporaries as:

> When using a value expression in most place expression contexts, a temporary unnamed memory location is created and initialized to that value. The expression evaluates to that location instead, except if promoted to a static. The drop scope of the temporary is usually the end of the enclosing statement.

Okay, that definition is confusing, let's zoom in on the first part.

"When using a value expression in most place expression contexts, a temporary unnamed memory location is created and initialized to that value"

The first thing to notice is that, under some condition — which we will explore later — a temporary is created. The temporary is defined as a memory location and *not* a value, that's why I've been so insistent on calling them "temporaries" instead of "temporary values". That's the mismatch in the wording between the reference and the errors that can lead to (at least my) confusion.

So when an expression triggers the creation of a temporary, its value is placed there. What makes this memory location special is that it's unnamed, meaning that, in contrast to the memory location of a variable that's declared in a statement like `let x = ...`, there's no associated alias such as `x`.

{{ responsive_image(src="/images/temporaries-rabbit-hole/temporaries.png", alt="temporaries") }}

Now let's explore what are the conditions that trigger the creation of a temporary.

## Value expressions and place expressions context

In the definition, it's mentioned that a temporary is created when a **value expression** is put in a **place expression context**. To understand this, we must understand what value expressions, place expressions, and place expression contexts are. Luckily these are all defined in the [same section](https://doc.rust-lang.org/stable/reference/expressions.html#r-expr.place-value).

First, let's look at the definition of a place expression.

> A place expression is an expression that represents a memory location.
> These expressions are paths which refer to local variables, static variables, dereferences (*expr), array indexing expressions (expr[expr]), field references (expr.f) and parenthesized place expressions.

The key here is that a place expression represents a memory location. The simplest case given is a variable. For example, in the statement `let x = y;` if `y` is a local variable, `y` corresponds to a place expression. Another case is an expression involving the dereference operator (`*`), like `*x` in `if (*x + 2) > 0 {...}`. This makes sense since `*x` should represent a value somewhere in memory that's being dereferenced; One important thing to notice is that while `*x` is a place expression, `(*x + 2)` isn't.

And value expressions are everything else. Basically, expressions that represent an actual value, such as `1`, `x + 2`,  `"hello"`, `Vec::new()`, or `RangeFull`. Notice that a value expression can contain a place expression such as `*x + 2` but the expression itself still represents the value of the operation.

Finally, a "place expression context" is a context where one can expect a place expression. It's [explained here](https://doc.rust-lang.org/stable/reference/expressions.html#r-expr.place-value.place-context), and it presents an exhaustive list of place expression contexts. The most important for our examples are the operand of a reference operator (`&`), the left-hand side of an assignment like `x = 5`, and the initializer of a `let` statement. The latter being the right-hand side of such a statement, for example, `5` in `let x = 5;`. The definition lists quite a few other cases, such as the `x` in a `match x { ... }` or in `if let ... = x { ... }`, but just know the same rules about temporaries apply in those cases.

## Everything is temporary, except death and taxes

Phew, still with me? So just one last time, to drive it home.

Place expressions are expressions that represent a location in memory. Value expressions are every other expression, these represent values. Contexts where place expressions are expected are called "place expression contexts," which are, among others, the right-hand side of a `let` statement or the operand of a reference operator (`&`).

With this in mind, when value expressions are used in place expression contexts, a temporary memory location is created, and the value of the expression is placed there. The expression then represents the temporary memory location.

Let's see one of the simplest examples of temporaries.

```rs
struct Foo;

fn main() {
    let x = Foo;
}
```

Since `Foo` is a value expression, and it's in a place expression context, the right-hand side of a let statement, it creates a temporary memory location. `Foo` is then inserted in the temporary, and the value in there is returned.

Let's look at another simple example.

```rs
struct Foo;

fn main() {
    &Foo;
}
```

Again, since `Foo` is a value expression in a place expression context, a temporary memory location is created. In this case, the place expression context is the operand of a `&` operator. `Foo` is then placed in the temporary, and now `&Foo` represents a reference to that anonymous memory location.

Going back to the previous example, you can still do this and still have the program compile.

```rs
struct Foo;

fn main() {
    let x = Foo;
    x;
}
```

You can even do this and have the program still compile!

```rs
struct Foo;

fn main() {
    let x = &Foo;
    x;
}
```

So, the question is, how temporary is a temporary really? That's to say, we haven't really talked about how long they live.

## Temporaries' lifetimes

Now we have the neccesary definitions to talk about what makes temporaries temporary. Well, not exactly, temporaries are just temporaries because they are anonymous, meaning there's no alias associated with that memory location. It has nothing to do with their lifetime. But you could ask, since there's no associated alias, then what's their lifetime? And you would be asking all the right questions.

Let's go to the relevant part of the definition of a temporary.

> The drop scope of the temporary is usually the end of the enclosing statement.

So, this is pretty clear, the drop scope of a temporary is the end of enclosing statement to be more precise we can go to [this part](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.temporary) of the reference where it tells us.

> Apart from lifetime extension, the temporary scope of an expression is the smallest scope that contains the expression and is one of the following:
> * The entire function.
> * A statement.
> * The body of an if, while or loop expression.
> * The else block of an if expression.
> * The non-pattern matching condition expression of an if or while expression, or a match guard.
> * The body expression for a match arm.
> * Each operand of a lazy boolean expression.
> * The pattern-matching condition(s) and consequent body of if (destructors.scope.temporary.edition2024).
> * The entirety of the tail expression of a block (destructors.scope.temporary.edition2024).

We'll focus on "a statement," but keep in mind that its lifetime could also be any of these expressions.

As expected, a temporary is dropped when it goes out of its drop scope. So let's go back to the first example, where we saw the "temporary value dropped while still in use" error.

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x = Some(&Foo);
    x;
}
```

And the full error was 

```
error[E0716]: temporary value dropped while borrowed
 --> src/main.rs:7:19
  |
7 |     let x = Some(&Foo);
  |                   ^^^ - temporary value is freed at the end of this statement
  |                   |
  |                   creates a temporary value which is freed while still in use
8 |     x;
  |     - borrow later used here
  |
help: consider using a `let` binding to create a longer lived value
  |
7 ~     let binding = Foo;
8 ~     let x = Some(&binding);
  |
```

Now we can understand this! Since the operand to `&` is a place expression context, and `Foo` is a value expression, a temporary memory location is created where the value `Foo` lives. The expression `&Foo` then returns a reference to that temporary, which drop scope is the end of the `let` statement, as the error mentions. After which the temporary memory location is freed, dropping `Foo`; and since we still use `x` on line 8, the compiler complains, as otherwise it'd allow a dangling reference.

Good! But then why is this allowed?

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x = Foo;
    x;
}
```

Luckily, this case is not complicated. The initializer (the right-hand side) of a let statement is a place expression context, and since `Foo` is a value expression, a temporary memory location is created where the value is held. But values [can always be moved out from temporaries](https://doc.rust-lang.org/stable/reference/expressions.html#r-expr.move.movable-place)[^1], so `Foo` is moved into `x`, which is a variable with a scope of the whole main function; therefore, it's still alive for the next line, and this program is valid!

Okay, good, but then explain this! (It compiles)

```rs
struct Foo;

fn main() {
    let x = Some(&Foo);
    x;
}
```

### Constant Promotion

Very well, the definition of temporaries mentioned this:

> The expression evaluates to that location instead, except if promoted to a static.

So what does that mean? Again, going to [the reference](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.const-promotion)

> Promotion of a value expression to a 'static slot occurs when the expression could be written in a constant and borrowed, and that borrow could be dereferenced where the expression was originally written, without changing the runtime behavior. That is, the promoted expression can be evaluated at compile-time and the resulting value does not contain interior mutability or destructors (these properties are determined based on the value where possible, e.g. &None always has the type &'static Option<_>, as it contains nothing disallowed).

It's a bit wordy, so let's break it down. When a value expression is promoted, it means that instead of placing it into a temporary memory slot, it's stored in memory with a `'static` lifetime, like a `const` or `static` variable would be. So if something like `let x = Some(&Foo)` meets the const promotion condition, `&Foo` would return a reference to an `'static` memory location instead of a temporary memory location that would go out of scope at the end of the statement.

{{ responsive_image(src="/images/temporaries-rabbit-hole/constpromotion.png", alt="const promotion") }}

The condition for this to happen is that, at runtime, there's no change in behavior between returning the temporary memory location and borrowing and dereferencing the `'static` location in its place instead. Or to put it another way, we can replace the expression `<expr>` with the following program, without any change.

```
const P = <expr>;

*&<expr>
```

Notice how only runtime behavior is mentioned, compile-time behavior could be affected as promotion can make some programs valid that wouldn't be otherwise.

The reference tells us exactly when this condition is met, the following 3 things need to be true:

* The expression can be evaluated at compile time: Otherwise it'd be impossible to place in a constant.
* The expression has no destructors, basically no `Drop` implementation invoked for the destructor of the type of the expression: Otherwise calling the promoted version wouldn't call the destructor at the end of the statement, when the temporary version would.
* No interior mutability: If this were to be allowed, each time the expression is called, it could mutate the static memory location, giving a different result each time! [^2]

And this explains why this compiles:

```rs
struct Foo;

fn main() {
    let x = Some(&Foo);
    x;
}
```

`Foo` doesn't implement `Drop`, has no interior mutability, and the `Foo` expression can be evaluated at compile-time. Therefore, `Foo` is promoted to an `'static`, and `&Foo` returns a reference to a static value that lives until the end of the program. 

While this program doesn't.

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x = Some(&Foo);
    x;
}
```

Since we implement `Drop` for type `Foo`, there's no const promotion for the expression `Foo`. And so, `&Foo` is a reference to a memory location that only lives until the end of the `let` statement.

Note that this also has further implications for const promotions; the value of the returned expression can be used anywhere where an `&'static` reference is expected, making the following program valid.

```rs

struct Foo;

fn main() {
    let x = &Foo;
    foo(x);
}

fn foo(x: &'static Foo) {}
```

While this program isn't

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x = &Foo;
    foo(x);
}

fn foo(x: &'static Foo) {}
```

So, with this you understand lifetimes of temporaries—wait... this compiles though!

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x = &Foo;
    x;
}
```

Well... it's finally time we get into lifetime extension.

### Lifetime extension

First, let's check another example:

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x;
    x = &Foo;
    x;
}
```
Aaand—this doesn't compile (next time anyone tells you that `let x = <expr>` desugars to `let x; x = <expr>;` you're entitled to act smug about that being false).

This is the error for the program.

```
error[E0716]: temporary value dropped while borrowed
 --> src/main.rs:8:10
  |
8 |     x = &Foo;
  |          ^^^- temporary value is freed at the end of this statement
  |          |
  |          creates a temporary value which is freed while still in use
9 |     x;
  |     - borrow later used here
  |
help: consider using a `let` binding to create a longer lived value
  |
8 ~     let binding = Foo;
9 ~     x = &binding;
  |
  ```
  
  Exactly what we would expect with the previous `Some` example. Then what?! Is there something special about initializers in `let` statements? Well... YES!
  
If you paid attention to the definition of temporaries, you might have noticed that it says that: "The drop scope of the temporary is usually the end of the enclosing statement," emphasis on the usually. Or more explicitly in the definition of the drop scope of a temporary: "*Apart from lifetime extension*, the temporary scope of an expression is the smallest scope that contains the expression [...]".

So what's going on? [Lifetime extensions](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.lifetime-extension) are an exception for the drop scope of temporaries:

> The temporary scopes for expressions in let statements are sometimes extended to the scope of the block containing the let statement. This is done when the usual temporary scope would be too small, based on certain syntactic rules.

The first important thing to note is that lifetime extensions extend the lifetime of temporary memory locations to the whole scope of the block containing the `let` statement. Which most of the time will make the actual aliased value in the `let` expression have a matching lifetime to any temporary reference it might hold. At least, this is the intended purpose, but the definition doesn't depend on an alias existing, so something like `let _ = ...` could still cause a lifetime extension.

Note that lifetime extensions only happen to temporaries associated with `let` statements, that's why it doesn't happen in this example, since the temporary is created for an assignment operation expression and not a `let` statement.

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x;
    x = &Foo;
    x;
}
```

We haven't yet defined the specific conditions on when the lifetime extensions happen, but keep in mind that it [says that](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.lifetime-extension.sub-expressions):

> If a borrow, dereference, field, or tuple indexing expression has an extended temporary scope then so does its operand. If an indexing expression has an extended temporary scope then the indexed expression also has an extended temporary scope.

We will come back to this later. There are 2 cases for lifetime extension: [extensions based on patterns](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.lifetime-extension.patterns) and [extensions based on expressions](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.lifetime-extension.exprs). 

Until now all the examples have been cases of the latter, so let's explore that first.

#### Extensions based on expressions

The reference states:

> For a let statement with an initializer, an extending expression is an expression which is one of the following:
> * The initializer expression.
> * The operand of an extending borrow expression.
> * The operand(s) of an extending array, cast, braced struct, or tuple expression.
> * The final expression of any extending block expression.

This tells us when an expression is considered "extending," but it doesn't tell us what that really means. For that, we need to look at this paragraph:

> The operand of any extending borrow expression has its temporary scope extended.

There's some kind of a "recursive" behavior with this definition. The initializer expression itself (right-hand side of a let statement) is an extending expression; that expression can contain an extending expression if it meets any of the requisites. For example, if it's a borrow or a tuple, its operands are also extending themselves and so the same goes on for any of those operands.

For example, in `let x = Foo` the expression `Foo` is an extending expression, though no lifetime is extended because there's no borrow. In `let x = &Foo`, the expression `&Foo` is an extending expression, and `Foo`, being the operand of an extending borrow expression, has its lifetime extended, and it's also an extending expression.

And for a more complicated case: `let x = &((), ((), &Foo));`, the expression `&((), ((), &Foo))` itself is an initializer expression, so the operand `((), ((), &Foo))` has its lifetime extended. This also means that `((), ((), &Foo))` is an extending expression. Which, in turn, means that each of the operands of the outer tuple are also extending expressions, in particular, `((), &Foo)` is an extending expression. Therefore, its operands are also extending, so `&Foo` is an extending expression. Then, since `Foo` is the operand of the extending expression, its temporary has its lifetime extended.

Note also that `Some(<expr>)` is a [call expression](https://doc.rust-lang.org/stable/reference/expressions/call-expr.html?highlight=call%20exp#call-expressions), which I will not get into[^3]. And the operands to call expressions are not mentioned in the types of extending expressions. Meaning that, while `Some(&Foo)` in `let x = Some(&Foo);` is an extending expression, `&Foo` isn't. Therefore, in that statement, there's no extending expression with a borrow, so no lifetime extension happens.

So finally, FINALLY! We can understand our previous examples.

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x = &Foo;
    x;
}
```

The `&Foo` in `let x = &Foo` is an extending expression, as the initializer expression of a `let` statement. Since `Foo` is the operand of the borrow of an extending expression, the temporary allocated for it has its lifetime extended to the whole `main` block. Then, for the next line, `x;`, `x` is still alive!

In contrast, in this example:

```rs
struct Foo;
impl Drop for Foo {
    fn drop(&mut self) {}
}

fn main() {
    let x = Some(&Foo);
    x;
}
```

While `Some(&Foo)`, as the initializer of a `let` statement, is an extending expression, its operands are not. Therefore, `&Foo` isn't extending, and no lifetime extension takes place. Meaning that the temporary for `Foo` goes out of scope at the end of the `let` statement, and for the next line, if `x` was still alive, it'd point to uninitialized memory.

And we've come so far to actually leave it here, so accompany me to take a quick look at extensions based on patterns. I promise I'll keep it a bit simpler.

#### Extensions based on patterns

So you might be wondering what a [pattern is](https://doc.rust-lang.org/stable/reference/patterns.html). For expediency's sake, patterns are the places where values are matched and bound, like the branches of a `match`, or, most importantly for our case, the left-hand side of a `let` statement. For example, the `x` in `let x = Foo;` is a pattern, and so is `(_, x)` in `let (_, x) = (5, Some(Foo));`.

The reference [tells us](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.lifetime-extension.patterns.extending) that an extending pattern is either:

> * An identifier pattern that binds by reference or mutable reference.
> * A struct, tuple, tuple struct, or slice pattern where at least one of the direct subpatterns is an extending pattern.

Meaning, the `x` in `let x = Foo;` isn't an extending pattern, while `ref x` in `let ref x = Foo;` is.

The reference then tells us exactly when lifetime extension happens based on an extending pattern.

> If the pattern in a let statement is an extending pattern then the temporary scope of the initializer expression is extended.

Okay, simple enough.

Let's look at this program:

```rs
struct Foo(Bar);

#[derive(Clone, Copy)]
struct Bar;

impl Drop for Foo {
    fn drop(&mut self) {
        println!("foo dropped");
    }
}

fn main() {
    let x = Foo(Bar).0;
    println!("last line of the program");
}
```

It prints:

```
foo dropped
last line of the program
```

[Field access](https://doc.rust-lang.org/stable/reference/expressions/field-expr.html#r-expr.field) operands are place expression contexts, `Foo(Bar)` in `Foo(Bar).0` is that kind of operand. Since it's a value expression, it's placed in a temporary. `Foo(Bar).0` copies `Bar` itself into `x`, but `Foo` remains at the temporary memory location and is then dropped at the end of the `let` statement. Note that there's no lifetime extension due to an extending expression since there's no borrow. And no pattern-based extension, since `x` isn't an extending pattern.

However, in this example there's pattern-based extension.

```rs
struct Foo(Bar);

#[derive(Clone, Copy)]
struct Bar;

impl Drop for Foo {
    fn drop(&mut self) {
        println!("foo dropped");
    }
}

fn main() {
    let ref x = Foo(Bar).0;
    println!("last line of the program");
}
```

This prints:

```
last line of the program
foo dropped
```

Since `ref x` is an extending pattern, the expression `Foo(Bar).0` has its lifetime extended, and from a few paragraphs above, you might [remember that](https://doc.rust-lang.org/stable/reference/destructors.html#r-destructors.scope.lifetime-extension.sub-expressions) "If a borrow, dereference, field, or tuple indexing expression has an extended temporary scope, then so does its operand." This means that the temporary that holds `Foo` has its lifetime extended, causing it to drop at the end of the main block, after the last line of the program.

Note that, if no lifetime extension happened, this program would not compile.

```rs
struct Foo(Bar);

#[derive(Clone, Copy)]
struct Bar;

impl Drop for Foo {
    fn drop(&mut self) {
        println!("foo dropped");
    }
}

fn main() {
    let ref x = Foo(Bar).0;
    x;
    println!("last line of the program");
}
```

Since the line `let ref x = Foo(Bar).0;` doesn't copy `Bar`, but rather keeps a reference to the temporary memory location. Which, without lifetime extension, would be dropped at the end of that same line.

# Closing words

Rejoice! You've gone through great trials and tribulations, and now you understand the magic of Rust temporaries.

{{ responsive_image(src="/images/temporaries-rabbit-hole/memes/feelsgood.png", alt="Yorokobe Shounen") }}

Normally, you don't need to understand this. When the compiler complains, you can just create a new binding and move on and ignore all this otherwise. But I, for one, believe that it's good to understand the specific conditions for the errors given by the compiler.

I still think it's a bit frustrating having a mismatch between the terminology of the errors given by the compiler and the terms used in the reference. But I also understand having a clear error while using "temporary memory" might be impossible. 

Furthermore, the reference itself [sometimes refers](https://doc.rust-lang.org/stable/reference/expressions.html?highlight=temporary%20value#r-expr.mut.valid-places) to something called "temporary values." Although it links to the section about temporary memory. I guess it'd be trivial to define "temporary value" as the value contained in "temporary memory," but I'm not too sure if that's a good definition, as the value itself would be only temporarily temporary.

But anyways, now you've been burdened with this knowledge, so go on, spread your wings. And the next time you're fighting with the compiler that you really don't want to add a `let` statement because it'd make your method call chain look ugly, and you'd really like this to be a one-liner, why it is complaining about your code.

---

[^1]: Notice that this is always the case, since there's no way to create multiple references to a temporary memory location.
[^2]: This is also true for any `&mut` reference, and in fact it seems like promotion doesn't happen [in that case](https://play.rust-lang.org/?version=stable&mode=debug&edition=2024&gist=d96a90d732614dac7b24f970f5b3a734).
[^3]: But if you want to understand why it's, [here](https://doc.rust-lang.org/stable/reference/items/enumerations.html?highlight=enum#r-items.enum.tuple-expr), also [wanna see a cool trick?](https://play.rust-lang.org/?version=stable&mode=debug&edition=2024&gist=c296161a3e0021ad0d96a5704adce31f)
