# mii-http for VS Code

Language support for `mii-http` `.http` spec files.

## Features

- Syntax highlighting for setup directives, endpoints, body schemas and `Exec` references.
- Diagnostics powered by `mii-http --check --json`, including warnings and exact source ranges.
- Completions for setup directives, endpoint directives, body kinds, type expressions and in-scope `Exec` references.
- Hover docs for keywords and typed symbols, including examples for common type expressions.
- Go to Definition for `Exec` references like `%query`, `:path`, `^Header`, `@var` and `$.bodyField`.

## Setup

The extension expects a compatible `mii-http` binary. If `miiHttp.executable` is left as `mii-http`, it first tries `target/debug/mii-http` in the current workspace, then falls back to `mii-http` on `PATH`.

Build this repo before developing the extension:

```sh
cargo build
```

Then run the extension from VS Code's extension host, or package it with `vsce`.

## Install from VSIX

Package locally:

```sh
bun install --frozen-lockfile
bun run package
code --install-extension mii-http-0.1.0.vsix
```

CI also packages this extension without using the VS Code Marketplace. Bump this `package.json` version, commit and push to `main`; the workflow creates a `vX.Y.Z-vscode` GitHub Release with `mii-http-X.Y.Z.vsix` attached. The same VSIX is also available as a workflow artifact on every run.
