# lyric-finder
Lyric finder is a simple rust API that allows you to search for lyrics of a song by its title and artist. It uses the Genius API to fetch the lyrics. This project was originally part of [spotify-player](https://github.com/aome510/spotify-player), a terminal based spotify client written in rust. It was forked primarily for use in my discord bot [CrackTunes](https://github.com/cycle-five/cracktunes).

## Usage

See the [examples](examples/) directory for usage examples.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Releases

```bash
cargo bump
git commit -S -am "chore: release vX.X.X"
cargo release --sign-tag --sign-commit --execute
```