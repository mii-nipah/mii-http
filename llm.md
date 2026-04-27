# mii-http specs — LLM guide

mii-http: `.http` spec → real HTTP server. Each endpoint → shell command. This doc teaches the syntax.

## File layout

Two sections, in order:

1. **Setup** — global directives, top of file.
2. **Endpoints** — one or more endpoint blocks.

`#` starts comment to end of line. Blank lines fine.

## Setup directives

All optional. One per line, uppercase keywords.

| Directive | Purpose | Example |
|---|---|---|
| `VERSION N` | Mounts endpoints under `/vN`. | `VERSION 1` |
| `BASE /path` | Path prefix before version. | `BASE /api` |
| `AUTH Bearer [HEADER H]` | Require bearer token in header `H`. | `AUTH Bearer [HEADER API_TOKEN]` |
| `JWT_VERIFIER [ENV X]` | Bearer is JWT; verifier from env `X`. | |
| `TOKEN_SECRET [ENV X]` | Plain bearer secret from env `X`. | |
| `MAX_BODY_SIZE` | Body cap. | `MAX_BODY_SIZE 1mb` |
| `MAX_QUERY_PARAM_SIZE` | Per-query-param cap. | `MAX_QUERY_PARAM_SIZE 100` |
| `MAX_HEADER_SIZE` | Per-header cap. | `MAX_HEADER_SIZE 100` |
| `TIMEOUT` | Per-request timeout. | `TIMEOUT 30s` |

Final URL = `BASE` + `/vVERSION` + endpoint path.

## Endpoint block

Starts with `METHOD /path`. `METHOD` ∈ {`GET`, `POST`, `PUT`, `DELETE`, `PATCH`}. Then directive lines. Ends at next method line or EOF.

```http
GET /status
Response-Type text/plain
Exec: echo ok
```

Required: method line, `Response-Type`, `Exec:`.

### Path params

`:name:type` in path:

```http
GET /users/:user_id:uuid
```

### Inputs

| Directive | Form | Notes |
|---|---|---|
| `QUERY name: TYPE` | `name?: TYPE` optional | Query string |
| `HEADER Name: TYPE` | `Name?: TYPE` optional | Request header (besides AUTH) |
| `BODY TYPE` | scalar body | `string`, `json`, `binary` |
| `BODY form { ... }` | structured form | typed fields |
| `BODY json { ... }` | structured JSON | typed fields |
| `VAR name [ENV X]` | bind env var | usable in `Exec` |

Form/JSON field syntax inside `{ ... }`:

```
field: TYPE
optional?: TYPE
list?: [TYPE]
```

### Types

- `int`, `float`, `boolean`, `uuid`
- `int(1..10)`, `float(0.0..1.0)` — inclusive ranges
- `a|b|c` — union of literal strings
- `/regex/` — full-match regex (avoid `/.*/`-style)
- `string` — stdin-only body
- `json` — stdin-only body
- typed JSON — via `BODY json { ... }`
- `binary` — `BODY` only; passed as temp file path (or stdin)
- `form` — `BODY` only

**Argv-safety rule (enforced by checker):** `string` and untyped `json` may be *declared* on any input, but they may **only reach the command via stdin**, never as an argv token. Use `[ ]` interpolation only with constrained types (int/float/uuid/range/union/regex). For untyped JSON bodies, declare a schema or pipe as stdin (`$ | cmd`).

## Exec line

`Exec:` declares shell command. Rest of line = command template.

### Reference sigils

| Sigil | Source |
|---|---|
| `%name` | query param |
| `:name` | path param |
| `^Name` | header |
| `@name` | `VAR` variable |
| `$` | full body |
| `$.field` | field of structured body (json/form) |

### Two interpolation contexts

1. **Inside double-quoted strings**: `{sigilname}` interpolates.
   ```http
   Exec: echo "Hello, {%name}!"
   ```
   Bare `{name}` outside a string is invalid.

2. **As shell words/groups**: wrap sigil-bearing word in `[ ]`.
   ```http
   Exec: some_cmd [--flag %name] [@greeting]
   ```
   - All references in `[ ]` present → group emitted.
   - Any optional missing → whole group dropped.
   - Bare `%name` / `:id` / `^H` / `@v` / `$.x` outside `[ ]` and outside a string is **literal text** (checker warns; escape with `\` to silence).

3. **Stdin shortcut**: leading bare sigil piped → read as stdin.
   ```http
   Exec: $ | xargs echo
   ```

Missing optionals inside `"{...}"` → empty string. Missing optionals in `[ ]` → group dropped.

## Minimal complete example

```http
VERSION 1
BASE /named
TIMEOUT 5s

GET /status
Response-Type text/plain
Exec: echo ok

GET /greet
Response-Type text/plain
QUERY name: /[a-zA-Z0-9_]+/
QUERY guest?: /[a-zA-Z0-9_]+/
Exec: echo Hello, [%name] [%guest]

POST /echo
Response-Type text/plain
BODY string
Exec: $ | xargs echo

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

GET /echovar
Response-Type text/plain
VAR greeting [ENV GREETING]
QUERY name?: /[a-zA-Z0-9_]+/
Exec: echo [%name] [@greeting]
```

## Rules of thumb

- Narrow types > `string` for argv-bound values.
- `string`/`json` raw body → stdin only, never argv.
- Declare every header you read. AUTH header excepted.
- `[ ]` for shell words with sigils. `"{...}"` only inside quoted strings.
- First `Exec:` token (program name) must be literal. No interpolation.
- Validate: `mii-http --check <file>`. Preview commands: `mii-http --dry-run <file>`.

## Common mistakes checker flags

| Mistake | Outcome |
|---|---|
| Bare `%name`/`:id`/`^H`/`@v`/`$.x` outside `[ ]` and `"{...}"` | Warning: literal text. Wrap in `[ ]` or escape with `\`. |
| `string`/untyped `json` field used as argv | Error. Move to stdin or constrain type. |
| Untyped `BODY json` used as `[$.field]` | Error. Add schema or use `$` via stdin. |
| `binary` declared anywhere except `BODY` | Error. |
| `VAR` from `[HEADER ...]` used as argv | Error. Use typed `HEADER`, or pipe via stdin. |
| Reference to undeclared name (e.g. `[%foo]` no `QUERY foo`) | Error. |
| Interpolated program name (`Exec: [@cmd] arg`) | Error. Must be literal. |
| Permissive regex `/.*/`, `/.+/`, or containing `.*`/`.+` | Warning. Tighten character class. |
| Two endpoints, same method + same normalized path | Warning: later overrides. |
| `AUTH Bearer` without `JWT_VERIFIER` or `TOKEN_SECRET` | Warning: any token accepted. |
| `GET` with `BODY` | Warning. |

## Worked example: wrap `ffmpeg` as thumbnail service

Goal: HTTP service. Input = video upload + timestamp. Output = JPEG frame.

```http
VERSION 1
BASE /media
AUTH Bearer [HEADER X-API-Key]
TOKEN_SECRET [ENV THUMBS_API_TOKEN]
MAX_BODY_SIZE 50mb
TIMEOUT 30s

POST /thumbnail
Response-Type image/jpeg
QUERY at: /[0-9]+(\.[0-9]+)?/      # seconds, e.g. 12 or 12.5
QUERY width?: int(16..3840)
BODY binary
Exec: ffmpeg -ss [%at] -i $ -frames:v 1 [-vf scale=%width:-1] -f mjpeg pipe:1
```

Notes:

- `BODY binary` → `$` is temp file path (or stdin in pipeline form).
- `[%at]` constrained shell arg; regex blocks injection.
- `[-vf scale=%width:-1]` = one shell word, one optional sigil. `width` omitted → whole `-vf …` group dropped.
- Bearer token from `X-API-Key`, validated against `THUMBS_API_TOKEN` env.

## Authoring workflow

1. Setup block: version, base, auth, limits, timeout.
2. Per endpoint: method+path → list inputs (`QUERY`/`HEADER`/path/`BODY`/`VAR`) → write `Exec:`.
3. Tightest type that fits. Reaching for `string`? Try stdin instead.
4. `[ ]` for shell words with sigils. `"{...}"` for messages.
5. `mii-http --check <file>`. Fix all errors, review every warning.
6. `mii-http --dry-run <file>`, hit each endpoint with curl. Verify printed commands match intent.
7. Then run real server.

## CLI

- `mii-http <file>` — run server.
- `mii-http --addr 0.0.0.0:8080 <file>` — bind address.
- `mii-http --check <file>` — validate, including security lints.
- `mii-http --dry-run <file>` — log commands, don't execute.
- `mii-http --quiet <file>` — suppress request/error logs.
