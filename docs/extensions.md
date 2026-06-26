# Extensions

**Status:** unimplemented. There is no extension surface yet.

This page is a placeholder so links to `/docs/extensions.md` from
elsewhere in the documentation don't 404. The extension model — what
a third party can plug in, how it loads, what API it sees — hasn't
been designed.

Open questions, in roughly the order they'd need to be answered:

- **Loading model.** WASM modules over a stable host API? Side-loaded
  Rust crates compiled into the binary? Out-of-process HTTP hooks
  similar to how Mastodon adapters work?
- **Surface.** Article extraction filters? New feed-discovery
  providers (the slot in `feed_search.rs` is intentional)? Custom
  view renderers? Outbound feed publishing (separate roadmap item)?
- **Trust model.** Inkwell is meant to run as a personal service
  behind an auth gateway; extensions don't need to be sandboxed from
  the operator, but they shouldn't be able to read arbitrary files or
  exfiltrate the article cache by default.

Tracked under [#7]. Comments / use-cases welcome on that issue —
they'll shape what the first iteration tries to do.

[#7]: https://codeberg.org/kendal/inkwell/issues/7
