//! # `lyric_finder`
//!
//! This crate provides a [`Client`] for retrieving a song's lyric from
//! [LRCLIB](https://lrclib.net), an open lyrics database with a free, keyless API.
//!
//! ## Example
//!
//! ```rust
//! # use anyhow::Result;
//! #
//! # async fn run() -> Result<()> {
//! let client = lyric_finder::Client::new();
//! let result = client.get_lyric("shape of you ed sheeran").await?;
//! match result {
//!     lyric_finder::LyricResult::Some {
//!         track,
//!         artists,
//!         lyric,
//!     } => {
//!         println!("{} by {}'s lyric:\n{}", track, artists, lyric);
//!     }
//!     lyric_finder::LyricResult::None => {
//!         println!("lyric not found!");
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Self-hosted instances
//!
//! The LRCLIB server is open source. Point the client at your own instance with
//! [`Client::with_base_url`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The public LRCLIB instance.
pub const DEFAULT_BASE_URL: &str = "https://lrclib.net/";

/// LRCLIB asks every client to identify itself with name, version and a project link.
const USER_AGENT: &str = concat!(
    "lyric_finder v",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/cycle-five/lyric-finder)"
);

/// Upper bound on a search response body (at most 20 records, lyrics included).
const MAX_SEARCH_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Upper bound on a single-record response body.
const MAX_RECORD_BODY_BYTES: usize = 1024 * 1024;
/// Upper bound on an error response body we bother reading.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Cool-down used when a 429 carries no usable `Retry-After`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);

pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
    /// Set from a 429's `Retry-After`; requests before this instant fail fast with
    /// [`RateLimited`] instead of hitting the server, as LRCLIB requires.
    retry_not_before: Arc<Mutex<Option<Instant>>>,
}

#[derive(Debug)]
pub enum LyricResult {
    Some {
        track: String,
        artists: String,
        lyric: String,
    },
    None,
}

/// Returned (inside the `anyhow::Error`) when LRCLIB rate-limits us, or when a call is made
/// before an earlier `Retry-After` has elapsed. Downcast with
/// `err.downcast_ref::<lyric_finder::RateLimited>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    /// How long to wait before the next request.
    pub retry_after: Duration,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rate limited by LRCLIB; retry after {}s",
            self.retry_after.as_secs_f32()
        )
    }
}

impl std::error::Error for RateLimited {}

impl Client {
    /// Construct a client for the public LRCLIB instance with a hardened HTTP client:
    /// HTTPS only, connect and request timeouts, and same-origin redirects only.
    ///
    /// # Panics
    ///
    /// Panics if the TLS backend cannot be initialized, the same condition under
    /// which `reqwest::Client::new()` panics.
    #[must_use]
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(redirect_policy())
            .build()
            .expect("failed to build HTTP client (TLS backend initialization failed)");
        Self::from_parts(http)
    }

    /// Construct a client reusing an existing http client.
    ///
    /// The caller is responsible for that client's timeouts and redirect policy. The
    /// `User-Agent` LRCLIB asks for is still sent on every request.
    #[must_use]
    pub fn from_http_client(http: &reqwest::Client) -> Self {
        Self::from_parts(http.clone())
    }

    fn from_parts(http: reqwest::Client) -> Self {
        Self {
            http,
            base: reqwest::Url::parse(DEFAULT_BASE_URL).expect("default base URL is valid"),
            retry_not_before: Arc::new(Mutex::new(None)),
        }
    }

    /// Use a different LRCLIB instance, e.g. a self-hosted one (`https://lyrics.example.com`).
    ///
    /// [`Client::new`]'s client is HTTPS-only; for a plain-HTTP instance on a private
    /// network, pass your own client via [`Client::from_http_client`] first.
    pub fn with_base_url(mut self, base_url: &str) -> anyhow::Result<Self> {
        let mut base = reqwest::Url::parse(base_url)?;
        anyhow::ensure!(
            matches!(base.scheme(), "http" | "https"),
            "LRCLIB base URL must be http(s): {base_url}"
        );
        anyhow::ensure!(
            base.username().is_empty() && base.password().is_none(),
            "LRCLIB base URL must not contain credentials"
        );
        anyhow::ensure!(
            base.query().is_none() && base.fragment().is_none(),
            "LRCLIB base URL must not have a query or fragment: {base_url}"
        );
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        self.base = base;
        Ok(self)
    }

    /// Search LRCLIB for records matching a free-text `query` (track and/or artist).
    ///
    /// Returns at most 20 records, best match first, lyrics included.
    pub async fn search_songs(&self, query: &str) -> anyhow::Result<Vec<search::Result>> {
        let query = improve_query(query);
        log::debug!("search songs: query={query}");

        let url = self.base.join("api/search")?;
        let request = self.http.get(url).query(&[("q", query.as_str())]);
        let bytes = self.send(request, MAX_SEARCH_BODY_BYTES).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Fetch one record by its LRCLIB `id`. Returns `Ok(None)` if there is no such record.
    pub async fn get_lyric_by_id(&self, id: u64) -> anyhow::Result<Option<search::Result>> {
        let url = self.base.join(&format!("api/get/{id}"))?;
        match self.send(self.http.get(url), MAX_RECORD_BODY_BYTES).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(err) if err.downcast_ref::<NotFound>().is_some() => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Get the plain lyric of the best match for `query`.
    ///
    /// Instrumental records and records without plain lyrics are skipped.
    pub async fn get_lyric(&self, query: &str) -> anyhow::Result<LyricResult> {
        let results = self.search_songs(query).await?;

        let Some((result, lyric)) = results.into_iter().find_map(|r| {
            let lyric = r.plain_lyrics.as_deref()?.trim().to_string();
            (!r.instrumental && !lyric.is_empty()).then_some((r, lyric))
        }) else {
            return Ok(LyricResult::None);
        };

        Ok(LyricResult::Some {
            track: result.track_name,
            artists: result.artist_name,
            lyric,
        })
    }

    /// Send `request` with our `User-Agent`, honoring any active `Retry-After`, and return
    /// the (size-capped) body of a 2xx response.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        limit: usize,
    ) -> anyhow::Result<Vec<u8>> {
        if let Some(wait) = self.remaining_cooldown() {
            return Err(RateLimited { retry_after: wait }.into());
        }

        let response = request
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await?;
        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = parse_retry_after(response.headers()).unwrap_or(DEFAULT_RETRY_AFTER);
            self.start_cooldown(retry_after);
            return Err(RateLimited { retry_after }.into());
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(NotFound.into());
        }
        if !status.is_success() {
            // Includes 3xx: a redirect the policy refused (or a caller's client that does not
            // follow redirects) must not be mistaken for an empty result.
            let body = read_body_capped(response, MAX_ERROR_BODY_BYTES)
                .await
                .unwrap_or_default();
            let detail = serde_json::from_slice::<ApiError>(&body)
                .map(|e| format!(": {} ({})", e.message, e.name))
                .unwrap_or_default();
            anyhow::bail!("LRCLIB request failed with status {status}{detail}");
        }

        read_body_capped(response, limit).await
    }

    fn remaining_cooldown(&self) -> Option<Duration> {
        let guard = self
            .retry_not_before
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.and_then(|until| until.checked_duration_since(Instant::now()))
    }

    fn start_cooldown(&self, wait: Duration) {
        let mut guard = self
            .retry_not_before
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(Instant::now() + wait);
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Internal marker for a 404, mapped to `Ok(None)` by [`Client::get_lyric_by_id`].
#[derive(Debug)]
struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not found on LRCLIB")
    }
}

impl std::error::Error for NotFound {}

/// LRCLIB's JSON error body.
#[derive(Debug, serde::Deserialize)]
struct ApiError {
    message: String,
    name: String,
}

/// `Retry-After` in delta-seconds form (what LRCLIB sends). HTTP-dates are not supported and
/// fall back to the default cool-down.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: f64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // Clamp to a sane range; a hostile or broken header must not wedge the client forever.
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs.min(3600.0)))
}

/// Follow redirects only within the origin of the original request, at most
/// [`MAX_REDIRECTS`] times. Anything else is an error rather than `attempt.stop()`, which
/// would hand the 3xx back to the caller as a successful response.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let same_origin = attempt
            .previous()
            .first()
            .is_some_and(|first| first.origin() == attempt.url().origin());
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if same_origin {
            attempt.follow()
        } else {
            let msg = format!("blocked cross-origin redirect to {}", attempt.url());
            attempt.error(msg)
        }
    })
}

/// Read a response body, failing if it exceeds `limit` bytes.
async fn read_body_capped(
    mut response: reqwest::Response,
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    if let Some(len) = response.content_length() {
        anyhow::ensure!(
            usize::try_from(len).is_ok_and(|len| len <= limit),
            "response body too large ({len} bytes, limit {limit})"
        );
    }
    let mut buf = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            buf.len() + chunk.len() <= limit,
            "response body exceeded {limit} bytes"
        );
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Largest index `<= idx` that is a char boundary of `s`.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Returns `query` without `remaster` & `remix` information from track/artist query.
/// Returned value is lowercase.
/// These caused wildly invalid lyrics to be found.
///
/// Must never panic: `query` usually comes from track metadata, i.e. untrusted input.
fn improve_query(query: &str) -> String {
    // flag for doing something wrong if the song name (after removing remix metadata) is too short.
    const SONG_MIN_LENGTH_WO_REMIX_METADATA: usize = 3;

    let is_filler = |c: char| c == '-' || c.is_whitespace();

    // Index just past the last non-filler (space, dash) char before `idx`.
    // Acts like a trim to remove undesired spaces and dashes.
    let trim_filler_before = |s: &str, idx: usize| {
        let idx = floor_char_boundary(s, idx);
        s[..idx].trim_end_matches(is_filler).len()
    };
    // used to handle longer variants of words: `remixed`, `remastered`, etc.
    let end_of_word = |s: &str, idx: usize| {
        let idx = floor_char_boundary(s, idx);
        s[idx..]
            .find(|c: char| !c.is_alphanumeric())
            .map_or(s.len(), |found| found + idx)
    };

    let mut query = query.to_lowercase();

    // remove "xxxx Remaster" from the query
    // For example, `{song} xxxx Remastered {artists}` becomes `{song} {artists}`.
    if let Some(remaster_start) = query.find("remaster") {
        let end = end_of_word(&query, remaster_start + "remaster".len());

        // If the word right before "remaster" is a year (e.g. "2011 remastered"), drop it too.
        let before = query[..remaster_start].trim_end_matches(is_filler);
        let prev_word_start = before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        let prev_word = &before[prev_word_start..];
        let start = if !prev_word.is_empty() && prev_word.chars().all(char::is_numeric) {
            prev_word_start
        } else {
            before.len()
        };

        let start = trim_filler_before(&query, start);
        query.drain(start..end);
    }

    // remove "- xxxx yyy remix" from the query
    // For example, `{song} - xxxx yyy remix {artists}` becomes `{song} {artists}`.
    if let Some(remix_start) = query.find("remix") {
        let end = end_of_word(&query, remix_start + "remix".len());

        // Only a dash *before* "remix" introduces the remix metadata.
        if let Some(metadata_start) = query[..remix_start].rfind('-')
            && metadata_start >= SONG_MIN_LENGTH_WO_REMIX_METADATA
        {
            let start = trim_filler_before(&query, metadata_start);
            query.drain(start..end);
        }
    }

    query
}

pub mod search {
    use serde::Deserialize;

    /// An LRCLIB lyrics record.
    #[derive(Debug, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Result {
        pub id: u64,
        pub track_name: String,
        pub artist_name: String,
        #[serde(default)]
        pub album_name: Option<String>,
        /// Track length in seconds.
        #[serde(default)]
        pub duration: Option<f64>,
        #[serde(default)]
        pub instrumental: bool,
        /// Plain lyrics; `None` for instrumental records.
        #[serde(default)]
        pub plain_lyrics: Option<String>,
        /// LRC-format time-synced lyrics; `None` for instrumental records.
        #[serde(default)]
        pub synced_lyrics: Option<String>,
    }
}

#[cfg(test)]
mod tests {
    use super::{Client, LyricResult, RateLimited, improve_query, redirect_policy, search};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Serve `responses` in order, one per connection, on a local port. Each raw request is
    /// sent back over the returned channel.
    fn serve(responses: Vec<String>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), rx)
    }

    fn http_response(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn local_client(base: &str) -> Client {
        Client::from_http_client(&reqwest::Client::new())
            .with_base_url(base)
            .unwrap()
    }

    const SEARCH_FIXTURE: &str = r#"[
        {"id": 1, "name": "Intro", "trackName": "Intro", "artistName": "Band", "albumName": null,
         "duration": 60, "instrumental": true, "plainLyrics": null, "syncedLyrics": null},
        {"id": 2, "name": "Song", "trackName": "Song", "artistName": "Band", "albumName": "Album",
         "duration": 233.5, "instrumental": false, "hasWordSync": false,
         "plainLyrics": "line one\nline two\n", "syncedLyrics": "[00:01.00] line one",
         "lyricsfile": "ignored: true"}
    ]"#;

    #[tokio::test]
    async fn get_lyric_skips_instrumental_and_sends_user_agent() {
        let (base, requests) = serve(vec![http_response("200 OK", "", SEARCH_FIXTURE)]);
        let client = local_client(&base);

        let LyricResult::Some {
            track,
            artists,
            lyric,
        } = client.get_lyric("Song & Band").await.unwrap()
        else {
            panic!("expected a lyric");
        };
        assert_eq!((track.as_str(), artists.as_str()), ("Song", "Band"));
        assert_eq!(lyric, "line one\nline two");

        let request = requests.recv().unwrap().to_lowercase();
        assert!(
            request.starts_with("get /api/search?q=song+%26+band "),
            "{request}"
        );
        assert!(request.contains("user-agent: lyric_finder v"), "{request}");
    }

    #[tokio::test]
    async fn search_songs_deserializes_records() {
        let (base, _rx) = serve(vec![http_response("200 OK", "", SEARCH_FIXTURE)]);
        let results = local_client(&base).search_songs("song").await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].instrumental && results[0].album_name.is_none());
        assert_eq!(results[1].album_name.as_deref(), Some("Album"));
        assert_eq!(
            results[1].synced_lyrics.as_deref(),
            Some("[00:01.00] line one")
        );
    }

    #[tokio::test]
    async fn get_lyric_none_when_no_results() {
        let (base, _rx) = serve(vec![http_response("200 OK", "", "[]")]);
        assert!(matches!(
            local_client(&base).get_lyric("nothing").await.unwrap(),
            LyricResult::None
        ));
    }

    #[tokio::test]
    async fn get_lyric_by_id_maps_404_to_none() {
        let body = r#"{"message":"Failed to find specified track","name":"TrackNotFound","statusCode":404}"#;
        let (base, requests) = serve(vec![http_response("404 Not Found", "", body)]);
        assert!(
            local_client(&base)
                .get_lyric_by_id(42)
                .await
                .unwrap()
                .is_none()
        );
        assert!(requests.recv().unwrap().starts_with("GET /api/get/42 "));
    }

    #[tokio::test]
    async fn error_body_is_surfaced() {
        let body =
            r#"{"message":"Server is overloaded","name":"ServerOverloaded","statusCode":503}"#;
        let (base, _rx) = serve(vec![http_response("503 Service Unavailable", "", body)]);
        let err = local_client(&base)
            .search_songs("x")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("503") && err.contains("ServerOverloaded"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn rate_limit_honors_retry_after_without_another_request() {
        let body =
            r#"{"message":"slow down","name":"RateLimitExceeded","statusCode":429,"scope":"ip"}"#;
        // Only one response is served: a second request reaching the server would fail to
        // connect rather than return RateLimited.
        let (base, _rx) = serve(vec![http_response(
            "429 Too Many Requests",
            "Retry-After: 30\r\n",
            body,
        )]);
        let client = local_client(&base);

        let err = client.search_songs("x").await.unwrap_err();
        let limited = err.downcast_ref::<RateLimited>().expect("RateLimited");
        assert_eq!(limited.retry_after, Duration::from_secs(30));

        let err = client.get_lyric("x").await.unwrap_err();
        let limited = err.downcast_ref::<RateLimited>().expect("fails fast");
        assert!(limited.retry_after <= Duration::from_secs(30));
        assert!(limited.retry_after > Duration::from_secs(25));
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_an_error() {
        let http = reqwest::Client::builder()
            .redirect(redirect_policy())
            .build()
            .unwrap();
        let (base, _rx) = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: https://evil.example/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        ]);
        let client = Client::from_http_client(&http)
            .with_base_url(&base)
            .unwrap();
        let err = client.search_songs("x").await.unwrap_err();
        assert!(
            format!("{err:?}").contains("blocked cross-origin redirect"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unfollowed_3xx_is_an_error_not_empty_success() {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let (base, _rx) = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        ]);
        let client = Client::from_http_client(&http)
            .with_base_url(&base)
            .unwrap();
        let err = client.search_songs("x").await.unwrap_err().to_string();
        assert!(err.contains("302"), "{err}");
    }

    #[test]
    fn base_url_validation() {
        let c = || Client::from_http_client(&reqwest::Client::new());
        assert!(c().with_base_url("https://lyrics.example.com").is_ok());
        assert!(c().with_base_url("http://10.0.0.5:3300/lrclib").is_ok());
        assert!(c().with_base_url("ftp://example.com").is_err());
        assert!(c().with_base_url("https://user:pw@example.com").is_err());
        assert!(c().with_base_url("https://example.com/?x=1").is_err());
        assert!(c().with_base_url("not a url").is_err());
    }

    #[tokio::test]
    async fn base_url_path_is_kept() {
        let (base, requests) = serve(vec![http_response("200 OK", "", "[]")]);
        let client = local_client(&format!("{base}/lrclib"));
        client.search_songs("x").await.unwrap();
        assert!(
            requests
                .recv()
                .unwrap()
                .starts_with("GET /lrclib/api/search?q=x ")
        );
    }

    #[tokio::test]
    async fn default_client_refuses_plain_http() {
        let client = Client::new().with_base_url("http://127.0.0.1:1").unwrap();
        assert!(client.search_songs("x").await.is_err());
    }

    #[test]
    fn improve_query_strips_remaster_with_year() {
        assert_eq!(
            improve_query("Here Comes The Sun - 2019 Remastered The Beatles"),
            "here comes the sun the beatles"
        );
        assert_eq!(
            improve_query("Song - Remastered 2009 Artist"),
            "song 2009 artist"
        );
        assert_eq!(improve_query("2011 Remaster"), "");
    }

    #[test]
    fn improve_query_strips_remix_metadata() {
        assert_eq!(
            improve_query("Song Name - Some DJ Remix Artist"),
            "song name artist"
        );
        assert_eq!(improve_query("Song - Extended Remixed"), "song");
        // Dash too early: the "song" would be empty, leave it alone.
        assert_eq!(improve_query("A - remix"), "a - remix");
    }

    #[test]
    fn improve_query_leaves_plain_queries() {
        assert_eq!(
            improve_query("Shape of You Ed Sheeran"),
            "shape of you ed sheeran"
        );
    }

    #[test]
    fn improve_query_never_panics_on_unicode_or_odd_input() {
        // Each of these panicked (non-char-boundary or start > end drain) before the fix.
        assert_eq!(improve_query("songéremaster"), "songé");
        assert_eq!(improve_query("abc remix foo - bar"), "abc remix foo - bar");

        let pieces = [
            "",
            " ",
            "-",
            " - ",
            "é",
            "日本",
            "🎵",
            "\u{3000}",
            "remaster",
            "remastered",
            "remix",
            "remixes",
            "2011",
            "١٢",
            "song",
            "x",
        ];
        for a in pieces {
            for b in pieces {
                for c in pieces {
                    for d in pieces {
                        let _ = improve_query(&format!("{a}{b}{c}{d}"));
                        let _ = improve_query(&format!("{a} {b}-{c} {d}"));
                    }
                }
            }
        }
    }

    #[test]
    fn retry_after_parsing() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let h = |v: &'static str| {
            let mut m = HeaderMap::new();
            m.insert(RETRY_AFTER, HeaderValue::from_static(v));
            super::parse_retry_after(&m)
        };
        assert_eq!(h("5"), Some(Duration::from_secs(5)));
        assert_eq!(h("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(h("999999"), Some(Duration::from_secs(3600)));
        assert_eq!(h("-1"), None);
        assert_eq!(h("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(super::parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn record_tolerates_missing_optional_fields() {
        let r: search::Result =
            serde_json::from_str(r#"{"id":7,"trackName":"T","artistName":"A"}"#).unwrap();
        assert_eq!(r.id, 7);
        assert!(!r.instrumental && r.plain_lyrics.is_none() && r.duration.is_none());
    }
}
