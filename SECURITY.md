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

## Where it connects

`api.spotify.com` and `accounts.spotify.com` for the Web API and sign-in;
`open.spotify.com` to name playlists the Web API will not describe; Spotify's
image servers for album art; and, with streaming, Spotify's own servers
through librespot. There is no telemetry.
