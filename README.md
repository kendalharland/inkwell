# inkwell

A self-hosted RSS/Atom reader for the built-in browser on a Kindle,
packaged as a single Rust binary. Ships with an optional pairing
sidecar (`inkwell-pair`) for signing new devices into an auth gateway.

**User docs live at <https://kendal.codeberg.page/inkwell/>** —
installation, configuration, the admin UI, OPML import, the pairing
flow, and every environment variable. The rest of this file is for
working on the codebase.

## Workspace

```
.
├── Cargo.toml                  # workspace root + main crate
├── src/                        # main reader binary
├── pair/                       # inkwell-pair sidecar workspace member
├── docs/                       # mdbook source
├── theme/                      # docs site CSS + JS
├── book.toml                   # mdbook config
├── Dockerfile                  # reader image
├── config.example.yaml         # seed config for local runs
└── config.docker.yaml          # baked into the reader image
```

Both crates share `Cargo.lock`. Docker builds run with the repo root
as the build context; `pair/Dockerfile` is selected via `-f`.

## Build

```sh
cargo build --release                    # both crates
cargo build --release -p inkwell         # main reader only
cargo build --release -p inkwell-pair    # sidecar only
```

The reader binary lands at `target/release/inkwell`; the sidecar at
`target/release/inkwell-pair`.

## Run

```sh
cp config.example.yaml config.yaml       # edit before first run
cargo run --release -- config.yaml
```

Listens on `0.0.0.0:5050` by default (override: `PORT`). The cache DB
lands at `./reader_cache.sqlite` (override: `CACHE_DB`).

## Test

```sh
cargo test --workspace                   # both crates
cargo test --bin inkwell                 # reader only
cargo test -p inkwell-pair               # sidecar only
```

Some tests bind a real `tokio::net::TcpListener` for header-on-the-wire
and pair-flow assertions, so the host must have loopback networking
available.

Format check:

```sh
cargo fmt --all -- --check
```

## CI

`.forgejo/workflows/ci.yml` runs `cargo fmt --check` + `cargo test
--workspace --locked` on every push to `main`.
`.forgejo/workflows/docs.yml` rebuilds the mdbook and force-pushes
the output to the `pages` branch when `docs/`, `theme/`, or
`book.toml` changes; Codeberg Pages serves that branch.

## Docs

```sh
mdbook build          # writes ./book (gitignored)
mdbook serve          # preview at http://localhost:3000
```

User-visible behaviour changes and their docs edit land in the same
commit — the docs site is part of the feature surface, not a
follow-up.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to file issues, open
PRs, and the project's stance on AI-assisted contributions.

<a href="https://www.buymeacoffee.com/kendalharland" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-yellow.png" alt="Buy Me a Coffee" style="height: 15px !important;width: 40px !important;" ></a>

## License

MIT or Apache-2.0, at your option. See [LICENSE](LICENSE).
