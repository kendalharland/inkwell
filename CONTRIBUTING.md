# Contributing

Small self-hosted project — patches, bug reports, and feature ideas
are welcome.

## Documentation

The published book at <https://kendal.codeberg.page/inkwell/> is the
canonical reference for inkwell's user-visible surface — installation,
configuration, the admin UI, the pairing sidecar, and every rendering
choice on the Kindle. Its source is the mdbook under [`docs/`](docs/);
see the [README](README.md#docs) for the local build command.

Read the book before proposing changes that touch user-visible
behaviour, so a patch doesn't contradict something the docs already
promise.

## Getting started

Build, run, and test commands live in the [README](README.md). CI runs
`cargo fmt --check` and `cargo test --workspace --locked` on every
push to `main`; both are worth running locally before opening a PR.

## Filing issues

Bug reports are most useful when they include:

- What you were doing.
- What happened.
- What you expected instead.
- The URL of any article or feed that reproduces the problem, when
  the bug is content-specific.

Feature ideas: open an issue before writing more than a small patch.
A short discussion up front is cheaper than reworking a merged PR.

## Pull requests

Keep commits focused. A single PR should do one thing.

- If the change fixes a bug, add a test that fails without the fix
  and passes with it.
- If the change is user-visible (a new admin field, a UI tweak, a
  config knob, a rendering change on the Kindle), update the matching
  page in [`docs/`](docs/) in the same commit — the published book at
  <https://kendal.codeberg.page/inkwell/> is treated as part of the
  feature surface, not a follow-up.
- Commit messages: a short subject line saying what changed, and a
  body explaining why. Match the style visible on the main branch.

Docs voice is neutral and factual — no marketing prose ("simply",
"easily"), no `TODO` or `(see #N)` parentheticals, no hedged
opinions. See existing pages for the shape.

## AI-assisted contributions

AI-assisted code, docs, and issue triage are welcome — this project
itself uses them. The only rule: **read what the AI produced and
verify it does what the PR claims**. Submit only patches and PRs you
would sign your own name to if you had written them by hand.

Practically that means:

- If the AI generated tests, run them and check they cover the change
  they claim to.
- If the AI wrote a docs section, read it and verify the claims match
  the current code.
- If the AI wrote a commit message, confirm it describes what you're
  actually shipping.
- If the AI dropped stray imports, half-finished branches, or
  comments that describe something else, clean them up before pushing.

A reviewer shouldn't be able to tell in five minutes that the PR
wasn't proofread. If they can, it's going back for another pass.

## License

By opening a PR you agree that your contribution is licensed under
the same MIT-or-Apache-2.0 dual license as the rest of the project.
