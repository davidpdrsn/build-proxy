# build-proxy

Keep development web servers running and have them restarted, on demand, if any
files have changed. Rebuilding servers on every file change is a waste of
energy.

## Install

1. [Install Rust](https://www.rust-lang.org/learn/get-started)
2. Run `cargo install --git https://github.com/davidpdrsn/build-proxy`

## Usage

For example run the proxy with

```
$ build-proxy --port 8080 -- go run ./cmd/web/main.go
```

This'll run the proxy at port 8080 and start the child server with `go run
./cmd/web/main.go`.

Calling `localhost:8080` will hit the proxy which will route traffic to the
child server. If any file in the current directory is changed it'll kill the
child server and restart it when a new request arrives.

The child server must have its port configurable via the `PORT` environment
variable.
