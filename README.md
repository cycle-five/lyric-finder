# lyric-finder
Lyric finder is a simple rust API that allows you to search for lyrics of a song by its title and artist. It uses the Genius API to fetch the lyrics. This project was originally part of [spotify-player](https://github.com/aome510/spotify-player), a terminal based spotify client written in rust. It was forked primarily for use in my discord bot [CrackTunes](https://github.com/cycle-five/cracktunes).

## Usage

See the [examples](examples/) directory for usage examples.

```bash
cargo run --example lyric-finder -- "shape of you ed sheeran"
```

## Features

- `rustls` (default): TLS via rustls. `rustls-tls` is kept as an alias for the old name.
- `native-tls`: TLS via the platform's native library.

Minimum supported Rust version: 1.88.

## Network behavior

`Client::new()` builds an HTTPS-only client with connect/request timeouts and only follows
redirects to `genius.com`. Lyric pages are only fetched from `https://genius.com/...` URLs,
and response bodies are size-capped. If you pass your own client via
`Client::from_http_client`, its timeouts and redirect policy are yours to configure.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Releases

```bash
cargo bump
git commit -S -am "chore: release vX.X.X"
cargo release --sign-tag --sign-commit --execute
```