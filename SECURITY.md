# Security

## Reporting a vulnerability

Please report security problems privately, with the **Report a
vulnerability** button on the repository's Security tab, rather than in a
public issue. Only the latest `main` is supported.

## What boombox keeps, and where

- **Web API tokens** in `tokens.json`, readable only by your user (`0600`), in
  a state directory only you can open (`0700`; `~/.local/state/boombox` by
  default). The OS keychain is used instead only if you set
  `[auth] keyring = true`.
- **librespot's reusable streaming login**, in builds with streaming, in the
  same directory and also readable only by you.
- **The daemon's control socket**, restricted to your user. Anything that can
  reach it can control your playback.
- **Your client ID**, in `config.toml`. It is not a secret: sign-in uses PKCE,
  so there is no client secret anywhere.

boombox re-tightens those permissions every time it starts.

## Known advisories in dependencies

Streaming builds embed [librespot], which brings its own dependency tree.
That tree currently carries nine published advisories: a timing side
channel in `rsa`, two denial-of-service issues in `quick-xml`, several
certificate-validation issues in the `rustls 0.22` generation reached
through `hyper-proxy2`, and `rustls-pemfile` being unmaintained.

All nine are upstream. None are reachable through boombox's own
dependencies, and none can be configured away: `hyper-proxy2` is required
by every TLS option librespot offers. librespot 0.8.0 is its latest
release, so there is nothing to upgrade to. They will clear when librespot
moves to the current `rustls` generation.

Builds without `--features streaming` do not include any of this.

CI reports advisories on every run but does not fail on them, for the
reason above. That becomes a blocking check once the tree is clear.

[librespot]: https://github.com/librespot-org/librespot

## Where it connects

`api.spotify.com` and `accounts.spotify.com` for the Web API and sign-in;
`open.spotify.com` to name playlists the Web API will not describe; Spotify's
image servers for album art; and, with streaming, Spotify's own servers
through librespot. There is no telemetry.
