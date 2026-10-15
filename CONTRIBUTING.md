# Contributing

## Commit messages

Commits follow [Conventional Commits](https://www.conventionalcommits.org/). The changelog and
the next version are derived from them.

```text
<type>(<optional scope>): <description>

<optional body>

<optional footer, e.g. BREAKING CHANGE: ...>
```

| Type | Use for | In the changelog |
|---|---|---|
| `feat` | a new feature | Added |
| `fix` | a bug fix | Fixed |
| `perf` | a performance improvement | Performance |
| `refactor` | a change that neither fixes a bug nor adds a feature | Changed |
| `docs` | documentation only | Documentation |
| `revert` | reverting an earlier commit | Reverted |
| `test`, `build`, `ci`, `chore`, `style` | everything else | not listed |

Scopes are optional and name the part of the crate, e.g. `kms`, `linuxfb`, `soft` (software
renderer), `glow`, `input`, `runner`, `options`, `examples` or `tools`. A breaking change is marked
with `!` after the type (`feat(input)!: ...`) or a `BREAKING CHANGE:` footer.

`main` is protected: changes go through pull requests, which are squash-merged. The pull
request's title becomes the commit message on `main`, so it has to follow this format; CI checks
it. The commits on the branch itself don't.

## Checks

CI runs these; run them locally before opening a pull request:

```sh
cargo fmt --check
cargo clippy --all-targets
cargo clippy --target aarch64-unknown-linux-gnu --all-targets
cargo clippy --target armv7-unknown-linux-gnueabihf --all-targets
cargo clippy --target armv7-unknown-linux-gnueabihf --no-default-features --features linuxfb,input-evdev --all-targets
cargo test
```

The board builds use [cross](https://github.com/cross-rs/cross) with the image in `docker/`
(Ubuntu 20.04, so the binaries run on glibc 2.31 and newer):

```sh
cross build --release --target aarch64-unknown-linux-gnu --example hello
cross build --release --target armv7-unknown-linux-gnueabihf --example hello
```

Changes to rendering or input should also be tried on a board. `tools/` has scripts to inject
touch, mouse and keyboard events over SSH and to turn a framebuffer dump into a PNG.

## Releases

Releases are made by [release-plz](https://release-plz.dev):

1. Every push to `main` opens or updates a release PR. It bumps the version in `Cargo.toml`
   based on the commits since the last release (checked with `cargo-semver-checks`) and adds
   them to `CHANGELOG.md`.
2. Merging the release PR publishes the crate to crates.io, tags the commit `vX.Y.Z` and
   creates a GitHub release with the changelog entry. The release also gets the `hello` example
   built for `aarch64` and `armv7`, ready to copy to a board.

While the version is `0.x`, breaking changes bump the minor version and everything else the
patch version.
