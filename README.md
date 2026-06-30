# inkwell

A small self-hosted RSS/Atom reader for the built-in browser on a
Kindle, packaged as a single Rust binary with an optional pairing
sidecar.

**User-facing documentation lives in the book:
<https://kendal.codeberg.page/inkwell/>**. Installation, configuration,
the admin UI, OPML import, the pairing sidecar, and every environment
variable are documented there. The rest of this file is for working on
the codebase.

## Workspace layout

```
.
├── Cargo.toml            # workspace root + main crate (inkwell)
├── src/                  # main reader binary
├── pair/                 # inkwell-pair sidecar workspace member
├── docs/                 # mdbook source (published to Codeberg Pages)
├── book.toml             # mdbook config
├── Dockerfile            # main reader image
├── config.example.yaml   # seed config
└── config.docker.yaml    # baked into the image; overridden via volume mount
```

The main crate and `pair/` share `Cargo.lock`. Docker builds for both
images use the repo root as the build context (`pair/Dockerfile` is
selected with `-f`).

## Build

```sh
cargo build --release                # both members of the workspace
cargo build --release -p inkwell     # main binary only
cargo build --release -p inkwell-pair
```

The main binary lands at `target/release/inkwell`. It takes a single
positional argument — the path to a YAML config (see
`config.example.yaml`).

## Run locally

```sh
cp config.example.yaml config.yaml   # edit before running
cargo run --release -- config.yaml
```

Listens on `0.0.0.0:5050` by default; override with `PORT`. The cache
DB lands at `./reader_cache.sqlite`; override with `CACHE_DB`.

## Test

```sh
cargo test --workspace               # both crates
cargo test --bin inkwell             # main reader unit + integration
cargo test -p inkwell-pair           # sidecar
```

The test suite hits real `tokio::net::TcpListener`s for the
header-on-the-wire and pair flow tests, so the host must have loopback
networking available.

Formatting:

```sh
cargo fmt --all -- --check
```

CI on Codeberg runs `cargo fmt --check` and `cargo test --workspace`
on every push to `main`; see `.forgejo/workflows/ci.yml`.

## Docs

The docs site is an mdbook in `docs/`. Build locally with:

```sh
mdbook build      # outputs to ./book (gitignored)
mdbook serve      # local preview at http://localhost:3000
```

Codeberg Pages publishes the built site automatically from `main`.

When changing user-visible behavior, update the matching page in
`docs/` in the same commit — the docs site is treated as part of the
feature surface, not a follow-up.

## License

MIT or Apache-2.0 — at your option. See [LICENSE](LICENSE).
