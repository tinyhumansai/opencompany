# Contributing

## Local Checks

Run these before opening a pull request:

```sh
git submodule update --init --recursive
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo check --features tiny
```

## Pull Requests

Keep changes focused. Include a short summary, any API or behavior changes, and
the local verification commands you ran.
