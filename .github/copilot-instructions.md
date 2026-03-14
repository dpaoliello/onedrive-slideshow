# Updating Dependencies

## 1. Non-breaking crate updates

```sh
cargo update -v
```

Run validation:

```sh
cargo build
cargo clippy
cargo test
```

Fix any errors before proceeding.

## 2. Breaking crate updates

Run `cargo update -v` and look for lines like:

```
Unchanged rand v0.9.2 (available: v0.10.0)
```

For each crate that appears **directly in Cargo.toml** (skip transitive-only dependencies):

1. Update the version in `Cargo.toml`.
2. Run `cargo update -p <crate>` to update the lockfile.
3. Run validation (`cargo build`, `cargo clippy`, `cargo test`).
4. Fix any API breakages (check compiler suggestions — they're usually accurate).
5. Commit before moving to the next crate.

### Known issues

- **reqwest**: The default TLS backend (`rustls` via `aws-lc-rs`) fails to build on Windows ARM64. Use `native-tls` instead:
  ```toml
  reqwest = { version = "0.13", default-features = false, features = ["charset", "form", "gzip", "http2", "json", "native-tls"] }
  ```
- **rand 0.9 → 0.10**: `random_range` moved from `Rng` to `RngExt` trait. Change `use rand::Rng` to `use rand::RngExt`.

## 3. Rust version

1. Update `channel` in `rust-toolchain.toml`.
2. Run validation (`cargo build`, `cargo clippy`, `cargo test`).
3. Fix any errors.
