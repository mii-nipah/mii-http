# mii-http

> Turn a `.http` specs file into a real HTTP server — backed by the shell commands you already have.

`mii-http` reads a small declarative file describing endpoints, parameters, body schemas and the shell command that should run for each route, and brings up a typed, validated, sandboxed HTTP server around it. It's the fastest way to put a clean HTTP face on top of CLI tools, scripts and one-off utilities — without writing a server.

```http
GET /status
Response-Type text/plain
Exec: echo ok

GET /greet
Response-Type text/plain
QUERY name: /[a-zA-Z0-9_]+/
Exec: echo Hello, [%name]
```

```sh
$ mii-http examples/sample.http
$ curl localhost:8080/named/v1/greet?name=world
Hello, world
```

---

## Index

- [Why mii-http](#why-mii-http)
- [Quick start](#quick-start)
- [The `.http` format](#the-http-format)
- [CLI](#cli)
- [Rust client generation](#rust-client-generation)
- [Editor support](#editor-support)
- [Architecture](#architecture)
- [Security model](#security-model)
- [Contributing](#contributing)
- [Related](#related)

---

## Why mii-http

- **Declarative.** Endpoints, types, headers, query and body schemas live in a single readable file.
- **Typed.** Every input is validated against a real type (`int`, `uuid`, ranges, unions, regexes, typed JSON, forms, …) before your command ever runs.
- **Safe by default.** Request values are type-checked and shell-quoted before execution. The `--check` command flags risky patterns.
- **Tiny.** One binary. No runtime, no framework to learn — write a spec, point `mii-http` at it.

## Quick start

Install the tool using cargo:

```sh
cargo install --locked mii-http
```

or, alternatively...

Build the binary:

```sh
cargo build --release
```

Write a spec — `hello.http`:

```http
VERSION 1
BASE /api

GET /hello
Response-Type text/plain
QUERY name?: /[a-zA-Z0-9_]+/
Exec: echo Hello, [%name]
```

Validate it, then run it:

```sh
mii-http --check hello.http
mii-http --addr 127.0.0.1:8080 hello.http
```

Try it out:

```sh
$ curl 'localhost:8080/api/v1/hello?name=mii'
Hello, mii
```

A larger, multi-endpoint example lives in [examples/sample.http](examples/sample.http).

## The `.http` format

A spec file has two parts: a **setup** block with global options, followed by one or more **endpoint** blocks. Comments start with `#`.

```http
VERSION 1                           # mounts everything under /v1
BASE /named                         # …prefixed with /named
AUTH Bearer [HEADER API_TOKEN]      # bearer-token auth from a header
MAX_BODY_SIZE 1mb
TIMEOUT 30s

GET /users/:user_id:uuid
Response-Type text/plain
Exec: echo user [:user_id]

POST /submit-form
Response-Type text/plain
BODY form {
  username: /[a-zA-Z0-9_]+/
  age?: int(0..150)
}
Exec: echo username=[$.username] age=[$.age]
```

### Inputs

| Source        | Reference in `Exec`                      |
| ------------- | ---------------------------------------- |
| Query param   | `[%name]` / `"Hello, {%name}"`           |
| Path param    | `[:name]` / `"user {:name}"`             |
| Header        | `[^Name]` / `"header {^Name}"`           |
| Body field    | `[$.field]` / `"field {$.field}"`        |
| Whole body    | `$` (as stdin)                           |
| User var      | `[@name]` / `"var {@name}"`              |

- `[ … ]` is **shell-piece interpolation** — use it for any shell word or shell-word group that contains request data, required or optional. Missing optional values drop the whole group.
- `{ … }` is **string interpolation** and is valid only inside quoted strings. Use it when the request data is part of a larger string. Missing optionals become empty strings.
- `$ | …` pipes the request body to the command's stdin.
- Bare references like `%name` in `Exec` are literal shell text, not interpolation. `--check` warns when they match declared inputs; escape them as `\%name` if you really want the literal text.

### Types

`int`, `float`, `boolean`, `uuid`, `int(1..10)`, `float(0.0..1.0)`, unions like `red|green|blue`, regexes `/.../`, `string`, `json`, typed JSON schemas, `form`, `binary`. See [specs.md](specs.md) for the authoritative reference and the rules around which types may flow into argv vs. stdin only.

`binary` is allowed both as a top-level `BODY` and as a field inside `BODY form { ... }` (uploaded via `multipart/form-data`). Outside of stdin, binary values are written to a temp file and the path is interpolated as a quoted shell word.

### Streaming responses

Prefix the response type with `stream` to stream the command's stdout to the client using HTTP chunked transfer instead of buffering the full output:

```http
GET /tail
Response-Type stream text/plain
Exec: tail -F /var/log/syslog
```

### Multi-line `Exec`

For longer scripts, use the `<<< ... >>>` form. Each non-empty line becomes its own pipeline statement; indentation is ignored:

```http
POST /complex
Response-Type text/plain
Exec: <<<
  echo "step one"
  echo "step two for {%name}"
  echo "done"
>>>
```

## CLI

```text
mii-http <path>                       run the server
mii-http --check <path>               validate the specs and exit
mii-http --check --json <path>        validate and print JSON diagnostics
mii-http --addr 0.0.0.0:8080 <path>   bind to a specific address
mii-http -q | --quiet <path>          suppress request/error logs
mii-http --dry-run <path>             log commands instead of running them
```

`--dry-run` is the recommended way to develop a spec: every request prints the exact command line that *would* have been executed, with all interpolations resolved.

`--check --json` emits the same validation result as machine-readable diagnostics for editor integrations.

## Rust client generation

The `mii-http-client` crate can generate a small async Rust client from a checked `.http` spec. The macro reads and validates the spec at compile time, then only generates the endpoints you explicitly map:

```rust
mii_http_client::client! {
    pub struct NamedApi;
    spec = "examples/sample.http";

    GET /status as status => String;
    GET /greet as greet => String;
    POST /submit-json as submit_json => serde_json::Value;
    GET /headers as headers => mii_http_client::ByteStream;
}
```

The explicit `METHOD /path as method => ReturnType;` form avoids path-name collisions and keeps response decoding in the Rust caller's hands. `String` responses are read as text, `mii_http_client::Bytes` and `Vec<u8>` as bytes, `mii_http_client::ByteStream` for `Response-Type stream ...`, and other return types are decoded as JSON.

Generated form bodies use normal URL-encoded forms until a `binary` field appears. Binary form fields use `mii_http_client::FilePart`, which can be path-backed for streaming uploads or bytes-backed for generated/in-memory content:

```rust
UploadBody {
    title: "cover".into(),
    file: mii_http_client::FilePart::path("cover.png"),
    preview: Some(
        mii_http_client::FilePart::bytes(vec![1, 2, 3])
            .with_file_name("preview.bin")
            .with_mime("application/octet-stream"),
    ),
}
```

## Editor support

A VS Code extension lives in [editors/vscode/mii-http](editors/vscode/mii-http). It adds syntax highlighting, `mii-http --check --json` diagnostics and basic completions for directives, types and `Exec` references.

Build the binary and the extension before launching the extension host or packaging a VSIX:

```sh
cargo build
cd editors/vscode/mii-http
bun install
bun run compile
bun run package
```

The CI workflow publishes downloadable VSIX files through GitHub, not the VS Code Marketplace. When `editors/vscode/mii-http/package.json` gets a new version on `main`, `.github/workflows/vscode-extension.yml` creates a `vX.Y.Z-vscode` release tag and attaches `mii-http-X.Y.Z.vsix` to the GitHub Release. Every workflow run also uploads the VSIX as an Actions artifact.

## Architecture

```
            ┌────────────┐    parse     ┌──────────────┐    check    ┌──────────────┐
   .http ──▶│  parse::   │─────────────▶│   spec AST   │────────────▶│  diagnostics │
            └────────────┘              └──────────────┘             └──────────────┘
                                               │
                                               ▼
                                        ┌──────────────┐    axum     ┌──────────────┐
                                        │   server::   │────────────▶│   exec::     │
                                        │  routes &    │             │  validate +  │
                                        │  validation  │             │  shell run   │
                                        └──────────────┘             └──────────────┘
```

Source layout:

- [src/parse/](src/parse/) — `chumsky`-based parser for the `.http` format.
- [src/spec.rs](src/spec.rs), [src/value.rs](src/value.rs) — typed AST and value model.
- [src/check.rs](src/check.rs), [src/diag.rs](src/diag.rs) — semantic checks and `ariadne` diagnostics.
- [src/server.rs](src/server.rs) — `axum` routing, request validation, header/body/query decoding.
- [src/exec.rs](src/exec.rs) — shell rendering, interpolation, temp-file materialization and process execution.

## Security model

- `Exec` runs through `/bin/sh`; shell syntax written in the spec is trusted shell syntax.
- Request values interpolated with `[ … ]` or quoted-string `{ … }` are shell-quoted before execution.
- Inputs are validated against their declared types before the command is invoked.
- `string` and free `json` types are restricted to **stdin only**, so unconstrained text cannot become argv.
- `binary` bodies and `binary` form fields are written to a temp file and the path is passed as argv (or streamed via stdin).
- `MAX_BODY_SIZE`, `MAX_QUERY_PARAM_SIZE`, `MAX_HEADER_SIZE` and `TIMEOUT` are enforced at the request boundary.
- `mii-http --check` highlights overly-permissive regexes (e.g. `/.*/`) and other risky patterns before they reach production.

## Contributing

Issues and PRs are welcome.

- Run `cargo test` before opening a PR — the suites under [tests/](tests/) cover the parser, checker, value model and end-to-end execution.
- Run `cargo clippy --all-targets` and `cargo fmt`.
- For new spec syntax, update both [specs.md](specs.md) and the parser tests; the spec file is the source of truth.
- Keep changes focused — small, reviewable PRs land faster.

## Related

- [`zmij`](https://crates.io/crates/zmij) — the structured-process helper used internally for safe command execution.
- [`axum`](https://github.com/tokio-rs/axum) — the HTTP framework powering the runtime.
- [`chumsky`](https://github.com/zesterer/chumsky) and [`ariadne`](https://github.com/zesterer/ariadne) — parser combinators and diagnostics.
