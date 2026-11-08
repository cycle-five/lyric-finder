# lyric-finder
Lyric finder is a simple rust API that allows you to search for lyrics of a song by its title and artist. It uses [LRCLIB](https://lrclib.net), an open lyrics database with a free, keyless API. (Versions before 0.3 scraped Genius, which now blocks scripted clients with a 403.) This project was originally part of [spotify-player](https://github.com/aome510/spotify-player), a terminal based spotify client written in rust. It was forked primarily for use in my discord bot [CrackTunes](https://github.com/cycle-five/cracktunes).

## Usage

See the [examples](examples/) directory for usage examples.

```bash
cargo run --example lyric-finder -- "shape of you ed sheeran"
```

## Features

- `rustls` (default): TLS via rustls. `rustls-tls` is kept as an alias for the old name.
- `native-tls`: TLS via the platform's native library.

Minimum supported Rust version: 1.88.

## LRCLIB

- No API key. Every request sends `User-Agent: lyric_finder vX.Y.Z (https://github.com/cycle-five/lyric-finder)`,
  as LRCLIB asks clients to identify themselves.
- LRCLIB rate-limits per IP (roughly 90 requests/minute for an identified client, per its docs).
  On a 429 the client returns `lyric_finder::RateLimited` and refuses further requests until
  `Retry-After` has passed, as LRCLIB requires. Downcast the `anyhow::Error` to handle it.
- Self-hosting: the [LRCLIB server](https://github.com/tranxuanthang/lrclib) is open source.
  Use `Client::new().with_base_url("https://your-instance")?`. For a plain-HTTP instance, pass
  your own client via `Client::from_http_client` first, since the default client is HTTPS-only.
- `search_songs` returns full records, including `synced_lyrics` (LRC) and `duration`;
  `get_lyric_by_id` fetches one record.

## Network behavior

`Client::new()` builds an HTTPS-only client with connect/request timeouts that only follows
same-origin redirects. Any non-2xx response, including an unfollowed redirect, is an error, and
response bodies are size-capped. If you pass your own client via `Client::from_http_client`,
its timeouts and redirect policy are yours to configure.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Releases

```bash
cargo bump
git commit -S -am "chore: release vX.X.X"
cargo release --sign-tag --sign-commit --execute
```