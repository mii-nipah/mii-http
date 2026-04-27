# mii-http-client

> Generate a typed, async Rust HTTP client straight from a [`mii-http`](../../README.md) `.http` spec — at compile time, with one macro.

`mii-http-client` is the Rust client side of [mii-http](../../README.md). Point the `client!` macro at a checked spec file, list the endpoints you actually want, and you get back a small async struct with one method per endpoint, fully-typed request structs, and response decoding handled for you.

```rust
mii_http_client::client! {
    pub struct SampleApi;
    spec = "examples/sample.http";

    GET /status              as status       => String;
    GET /greet               as greet        => String;
    GET /users/:user_id      as user         => String;
    POST /submit-json        as submit_json  => serde_json::Value;
    GET /headers             as headers      => mii_http_client::ByteStream;
}
```

```rust
let api = SampleApi::new("http://localhost:8080")?.bearer_token("…");

let hello   = api.greet(GreetRequest { name: "nipah".into(), guest: None }).await?;
let payload = api.submit_json(SubmitJsonRequest {
    body: SubmitJsonBody { title: "hello".into(), count: Some(3) },
}).await?;
```

The spec is parsed and validated by the macro at build time, so a typo in a route name, a missing parameter or a stray return type is a compile error — not a runtime surprise.

---

## Index

- [Why](#why)
- [Quick start](#quick-start)
- [The `client!` macro](#the-client-macro)
- [Return types](#return-types)
- [Request bodies](#request-bodies)
- [File uploads](#file-uploads)
- [Authentication](#authentication)
- [Errors](#errors)
- [Re-exports](#re-exports)
- [Contributing](#contributing)
- [Related](#related)

---

## Why

- **One source of truth.** The same `.http` spec drives the server *and* the client — they cannot drift.
- **Compile-time checked.** The macro runs `mii-http`'s parser and semantic checker on the spec; broken specs fail `cargo build`.
- **Only what you ask for.** You list the endpoints you want bound. Unmapped routes generate no code.
- **Typed inputs.** Path params, query params, headers, JSON fields and form fields all become real Rust fields with the right types (`Uuid`, `i64`, `f64`, `bool`, `String`, `Vec<T>`, …).
- **Small surface.** Built on `reqwest` with `rustls`. No frameworks, no codegen step, no build script.

## Quick start

Add the crate:

```toml
[dependencies]
mii-http-client = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
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

Generate and use the client:

```rust
mii_http_client::client! {
    pub struct HelloApi;
    spec = "hello.http";

    GET /hello as hello => String;
}

#[tokio::main]
async fn main() -> mii_http_client::Result<()> {
    let api = HelloApi::new("http://localhost:8080")?;
    let greeting = api.hello(HelloRequest { name: Some("mii".into()) }).await?;
    println!("{greeting}");
    Ok(())
}
```

That's the whole loop — edit the spec, rebuild, the client follows.

## The `client!` macro

```text
mii_http_client::client! {
    <visibility> struct <ClientName>;
    spec = "<path/to/spec.http>";

    <METHOD> <path> as <method_name> => <ReturnType>;
    …
}
```

- `spec` is resolved relative to your crate's `CARGO_MANIFEST_DIR`. The file is also `include_str!`-ed so edits trigger a rebuild.
- Each mapping line binds **exactly one** endpoint from the spec. The HTTP method and path must match a declared endpoint, but you can write the path with the parameter *names only* (e.g. `GET /users/:user_id`) — the macro looks up the typed segment for you.
- The generated method is `pub async fn <method_name>(&self, request: <MethodName>Request) -> Result<ReturnType>`. If the endpoint has no inputs at all, the `request` argument is omitted.
- Generated request and body structs are named `<MethodName>Request` and `<MethodName>Body`, and live alongside the client struct.

Mapping more than one client over the same spec is fine — only the endpoints you list get bound.

## Return types

The return type tells the macro how to decode the response body:

| You write…                       | Decoded as                                         |
| -------------------------------- | -------------------------------------------------- |
| `String`                         | `response.text().await`                            |
| `mii_http_client::Bytes` / `Vec<u8>` | full body as bytes                             |
| `mii_http_client::ByteStream`    | streaming body — required for `Response-Type stream …` |
| anything else                    | `response.json::<T>().await`                       |

`ByteStream` and stream endpoints are linked: a `stream` response *must* use `ByteStream`, and `ByteStream` *only* works with stream responses. Anything else is a compile error.

## Request bodies

Bodies follow the `BODY` directive in the spec:

| Spec body                | Generated `body` field type                       | Sent as              |
| ------------------------ | ------------------------------------------------- | -------------------- |
| `BODY string`            | `String`                                          | raw text             |
| `BODY binary`            | `Vec<u8>`                                         | raw bytes            |
| `BODY json` (no schema)  | `serde_json::Value`                               | JSON                 |
| `BODY json { … }`        | a generated `<Name>Body` struct                   | JSON                 |
| `BODY form { … }`        | a generated `<Name>Body` struct                   | URL-encoded form     |
| `BODY form { … binary }` | same, with `FilePart` fields                      | `multipart/form-data` |

Optional fields become `Option<T>` automatically.

## File uploads

When a `BODY form { … }` block contains a `binary` field, the generated body switches to multipart and the field type becomes `mii_http_client::FilePart`. `FilePart` can stream from disk or wrap in-memory bytes:

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

Path-backed parts are uploaded via `reqwest`'s streaming file support, so large files don't get loaded into memory.

## Authentication

If the spec declares `AUTH Bearer […]`, the generated client wires the header for you:

```rust
let api = SampleApi::new("http://localhost:8080")?
    .bearer_token("eyJhbGciOi…");

api.set_bearer_token("rotated-token");   // from `&mut self`
api.clear_bearer_token();
```

Endpoints that don't require auth simply ignore it.

## Errors

Every generated method returns `mii_http_client::Result<T>`, which is `Result<T, mii_http_client::Error>`:

```rust
pub enum Error {
    InvalidUrl(String),
    Io(std::io::Error),
    Http(reqwest::Error),
    UnexpectedStatus { status: reqwest::StatusCode, body: String },
}
```

Non-2xx responses become `UnexpectedStatus` with the body captured for debugging.

## Re-exports

For convenience, the crate re-exports what the generated code needs (and what you usually want when calling it):

- `bytes::Bytes`
- `reqwest`
- `serde`, `serde_json`
- `uuid`
- `Client`, `ByteStream`, `FilePart`, `Result`, `Error`

You don't normally need to depend on `reqwest` directly — go through `mii_http_client::reqwest` if you need to build a custom HTTP client, then pass it via `Api::with_http_client(base, http)`.

## Contributing

Issues and PRs are welcome in the [main `mii-http` repository](../../README.md#contributing). When changing the macro:

- Run `cargo test -p mii-http-client` — the tests under `tests/` exercise the full code-generation path against real specs.
- Run `cargo clippy --all-targets` and `cargo fmt`.
- If you add or change spec syntax, update the macro alongside the parser/checker so generated clients stay in sync.

## Related

- [mii-http](../../README.md) — the server, spec language and CLI this client targets.
- [mii-http-client-macros](../mii-http-client-macros) — the proc-macro crate that powers `client!`.
- [`reqwest`](https://github.com/seanmonstar/reqwest) — the underlying HTTP client.
