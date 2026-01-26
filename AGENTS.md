# build-proxy

A development proxy that watches for file changes and automatically rebuilds/restarts the child server on the next request.

## How it works

1. Starts a child server process, passing `PORT` via environment variable
2. Watches the working directory for file changes
3. When a file changes, kills the child process
4. On the next incoming request, spawns a new child process (lazy rebuild)

## Usage

```sh
build-proxy --port 3000 --pwd /path/to/project -- cargo run --bin my-server
```

Arguments after `--` are passed to the child process.

## Running tests

```sh
cargo test
```

The e2e tests (`tests/e2e.rs`) exercise the full flow: start proxy, send request, trigger file change, verify rebuild.
