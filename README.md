# piscina

A simple generic pool that supports both sync and async.

[![crate_version](https://img.shields.io/crates/v/piscina.svg?style=flat)](https://crates.io/crates/piscina)
[![docs_version](https://img.shields.io/badge/docs-latest-blue.svg?style=flat)](https://docs.rs/fe2o3-amqp/latest/piscina/)

This crate provides a simple generic pool that uses channels instead of locks and supports both sync and async
without dependence on any runtime.

## Features

```toml
default = []
```

The features below are related to logging errors, which should be unreachable.

- `log`: Enables logging using the `log` crate.
- `tracing`: Enables logging using the `tracing` crate.

## Examples

Non-async example:

```rust
use piscina::Pool;

let mut pool = Pool::new();
pool.put(1);
pool.put(2);

let item1 = pool.try_get();
assert!(item1.is_some());

let item2 = pool.blocking_get();

let none = pool.try_get();
assert!(none.is_none());

drop(item1);
let item3 = pool.try_get();
assert!(item3.is_some());
```

Async example:

```rust
use piscina::Pool;

futures::executor::block_on(async {
    let mut pool = Pool::new();
    pool.put(1);
    pool.put(2);

    let item1 = pool.get().await;
    let item2 = pool.get().await;

    let none = pool.try_get();
    assert!(none.is_none());

    drop(item1);
    let item3 = pool.get().await;
});
```

## Safety

Instead of using an `Option`, this crate uses unsafe code `ManuallyDrop` in `PooledItem`.
And the only use of unsafe code `ManuallyDrop::take()` occurs when `PooledItem` is dropped.

## TODO:

- [ ] Allow removal of items with `pop()` and `try_pop()` once an item becomes available

License: MIT/Apache-2.0
