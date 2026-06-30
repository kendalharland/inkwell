# inkwell-pair

Standalone HTTP sidecar that mints one-time pairing codes for the
inkwell reader. Workspace member of the parent crate at the repo
root.

User-facing documentation — flow, configuration, Docker recipe,
pairing with authelia — lives in the book:
<https://kendal.codeberg.page/inkwell/sidecar.html>.

## Build

```sh
cargo build -p inkwell-pair --release
```

The release binary is written to `target/release/inkwell-pair`.

## Test

```sh
cargo test -p inkwell-pair
```

The test suite covers code generation and shape validation, the full
mint → consume flow, single-use semantics, expiry, sweep-on-mint of
expired entries, `Set-Cookie` attribute assembly, and env-var parsing.

## Crate layout

```
pair/
├── Cargo.toml      # workspace member
├── Dockerfile      # build context is the repo root
└── src/main.rs     # single-binary axum service
```

The Dockerfile is built from the repo root with `-f pair/Dockerfile`
so it has access to the workspace's `Cargo.toml` and `Cargo.lock`.
