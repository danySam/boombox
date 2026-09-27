# Contributing

## Building

You need Rust 1.88 or newer. On Linux, streaming also needs the ALSA
development headers and `pkg-config`:

```console
sudo apt install libasound2-dev pkg-config         # Debian, Ubuntu
sudo dnf install alsa-lib-devel pkgconf-pkg-config # Fedora
```

```console
make build            # debug build, without streaming
make build-streaming  # debug build, with streaming and the visualisations
make install          # onto your PATH, with streaming
make uninstall        # remove it again; config and tokens are left alone
make check            # formatting, clippy and tests
```

Both feature sets build into the same `target/debug/boombox`, so a plain
`make build` replaces the streaming binary, and the other way round. A bare
`boombox` runs whatever is on your PATH, not what you last built here.

## Checks

CI runs these on Linux and macOS, and fails on any warning:

```console
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features streaming -- -D warnings
cargo test --workspace
cargo test --workspace --features streaming
```

Tests never touch the network or a Spotify account. `app.rs` and `keymap.rs`
hold no I/O, so the TUI is tested without a terminal: `App::update` takes an
`Action` and returns an optional `Command`, and both ends are ordinary values.

**Keep personal data out of tests and docs.** Use invented names, IDs and
device names rather than ones from your own library: a playlist ID from your
account leads to your profile, and a Daily Mix ID is made for you.

## Trying changes without disturbing your own setup

`BOOMBOX_CONFIG_DIR` and `BOOMBOX_STATE_DIR` point boombox at a separate configuration,
token store and daemon socket, so an experiment cannot touch the daemon you
use day to day:

```console
export BOOMBOX_CONFIG_DIR=/tmp/boombox-dev/config BOOMBOX_STATE_DIR=/tmp/boombox-dev/state
boombox setup
```

## Commit messages

Say why, not only what: what was wrong, how it was found, and what was
checked.

## macOS code signing (optional)

This only matters if you keep tokens in the keychain. `make setup-codesign`
signs dev builds with a self-signed certificate of their own, which gives every
build the same signing requirement. In practice the keychain still asked after
each rebuild, most likely because a self-signed certificate carries no Apple
Team ID, so it does not stop the prompts; a certificate with a Team ID would.
`make unsign-teardown` removes the signing setup. Without it, builds are
ad-hoc signed and everything else works the same.

## Dependency licences

Dependencies must use a licence allowed in `deny.toml`, which CI checks:

```console
cargo deny check licenses
```

Binary releases have to ship the licence texts of everything they include.
`cargo about` generates them, using `about.toml`. All features, so the
streaming dependencies are included:

```console
cargo about generate --all-features about.hbs > THIRD_PARTY_LICENSES.html
```

## Licence

Contributions are accepted under the MIT licence, the same as the project.
