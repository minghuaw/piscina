# piscina

Yet another implementation of pool of generic items.

## Features

- `log`: Enables logging using the `log` crate
- `tracing`: Enables logging using the `tracing` crate

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
