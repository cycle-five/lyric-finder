//! # `lyric_finder`
//!
//! This crate provides a [`Client`] struct for retrieving a song's lyric.
//!
//! It utilizes the [Genius](https://genius.com) website and its APIs to get lyric data.
//!
//! ## Example
//!
//! ```rust
//! # use anyhow::Result;
//! #
//! # async fn run() -> Result<()> {
//! let client = lyric_finder::Client::new();
//! let result = client.get_lyric("shape of you").await?;
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

use std::time::Duration;

const SEARCH_BASE_URL: &str = "https://genius.com/api/search";

/// Host we are willing to fetch lyric pages from (and follow redirects to), subdomains included.
const ALLOWED_HOST: &str = "genius.com";

/// Upper bound on a search API response body.
const MAX_SEARCH_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Upper bound on a lyric page body. Genius pages are typically well under 2 MiB.
const MAX_LYRIC_PAGE_BYTES: usize = 16 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;

pub struct Client {
    http: reqwest::Client,
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

impl Client {
    /// Construct a client with a hardened default HTTP client: HTTPS only,
    /// connect and request timeouts, and redirects restricted to `genius.com`.
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
        Self { http }
    }

    /// Construct a client reusing an existing http client.
    ///
    /// The caller is responsible for that client's timeouts and redirect policy.
    /// Lyric page URLs are still validated to be `https://genius.com/...` before fetching.
    #[must_use]
    pub fn from_http_client(http: &reqwest::Client) -> Self {
        Self { http: http.clone() }
    }

    /// Search songs satisfying a given `query`.
    pub async fn search_songs(&self, query: &str) -> anyhow::Result<Vec<search::Result>> {
        let query = improve_query(query);

        log::debug!("search songs: query={query}");

        let response = self
            .http
            .get(SEARCH_BASE_URL)
            .query(&[("q", query.as_str())])
            .send()
            .await?;
        let status = response.status();
        let bytes = read_body_capped(response, MAX_SEARCH_BODY_BYTES).await?;

        let body: search::Body = match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            Err(err) if !status.is_success() => {
                anyhow::bail!("search request failed with status code: {status} ({err})")
            }
            Err(err) => return Err(err.into()),
        };

        if body.meta.status != 200 {
            let message = body.meta.message.unwrap_or_else(|| {
                format!("request failed with status code: {}", body.meta.status)
            });
            anyhow::bail!(message);
        }

        Ok(body
            .response
            .map(|r| {
                r.hits
                    .into_iter()
                    .filter(|hit| hit.ty == "song")
                    .map(|hit| hit.result)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default())
    }

    /// Retrieve a song's lyric from a "genius.com" `url`.
    ///
    /// Returns an error if `url` is not an `https://genius.com/...` URL.
    pub async fn retrieve_lyric(&self, url: &str) -> anyhow::Result<String> {
        let parsed = reqwest::Url::parse(url)?;
        anyhow::ensure!(
            is_allowed_url(&parsed),
            "refusing to fetch lyric from non-genius.com URL: {url}"
        );

        let lyric = self.fetch_lyric_page(parsed).await?;
        Ok(lyric.trim().to_string())
    }

    /// Fetch and parse a lyric page. Callers must have validated `url` already.
    ///
    /// Anything but a 2xx is an error. This matters for 3xx: a redirect the policy refuses to
    /// follow, or any redirect at all from a caller-supplied client with `Policy::none()`, comes
    /// back as an `Ok` response that `error_for_status()` accepts, and would otherwise be parsed
    /// into an empty lyric.
    async fn fetch_lyric_page(&self, url: reqwest::Url) -> anyhow::Result<String> {
        let response = self.http.get(url.clone()).send().await?;
        let status = response.status();
        anyhow::ensure!(
            status.is_success(),
            "lyric page request to {url} returned {status}"
        );
        let bytes = read_body_capped(response, MAX_LYRIC_PAGE_BYTES).await?;
        let html = String::from_utf8_lossy(&bytes);
        log::trace!("retrieved lyric page from url={url} ({} bytes)", html.len());
        Ok(parse::parse(&html))
    }

    /// Process a lyric obtained by crawling the [Genius](https://genius.com) website.
    ///
    /// The lyric received this way may have weird newline spacings between sections (*).
    /// The below function tries an ad-hoc method to fix this issue.
    ///
    /// (*): A section often starts with `[`.
    fn process_lyric(lyric: &str) -> String {
        // the below code modifies the `lyric` to make the newline between sections consistent
        lyric.replace("\n\n[", "\n[").replace("\n[", "\n\n[")
    }

    /// Get the lyric of a song satisfying a given `query`.
    pub async fn get_lyric(&self, query: &str) -> anyhow::Result<LyricResult> {
        // Perform the search for songs
        let results = self.search_songs(query).await?;

        // Filter to find the first result where the artist names do not contain 'Genius'
        // (Genius-authored translation/romanization pages).
        let Some(result) = results
            .into_iter()
            .find(|result| !result.artist_names.contains("Genius"))
        else {
            return Ok(LyricResult::None);
        };

        // Retrieve the song lyrics from the URL of the result
        let lyric = self.retrieve_lyric(&result.url).await?;

        Ok(LyricResult::Some {
            track: result.title,
            artists: result.artist_names,
            lyric: Self::process_lyric(&lyric),
        })
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Follow redirects only to allowed URLs, at most [`MAX_REDIRECTS`] times.
///
/// A redirect anywhere else is an error rather than `attempt.stop()`: stopping hands the 3xx
/// back to the caller as a successful response.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if is_allowed_url(attempt.url()) {
            attempt.follow()
        } else {
            let msg = format!("blocked redirect to non-genius.com URL: {}", attempt.url());
            attempt.error(msg)
        }
    })
}

/// Only `https://genius.com/...` (or a subdomain of it) on the default port, no userinfo.
fn is_allowed_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| {
            host == ALLOWED_HOST
                || host
                    .strip_suffix(ALLOWED_HOST)
                    .is_some_and(|prefix| prefix.ends_with('.'))
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
/// (try yourself adding remastered 2011 to a song's name when searching in Genius!)
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

mod parse {
    use ego_tree::iter::Edge;
    use scraper::{ElementRef, Html, Node, Selector};

    const LYRIC_CONTAINER_ATTR: &str = "data-lyrics-container";
    /// Genius marks non-lyric chrome nested inside lyric containers
    /// (contributor counts, "Lyrics" headers, ...) with this attribute.
    const EXCLUDE_ATTR: &str = "data-exclude-from-selection";

    /// Parse the HTML content of a "genius.com" lyric page to retrieve the corresponding lyric.
    pub fn parse(html: &str) -> String {
        let document = Html::parse_document(html);
        let selector = Selector::parse("[data-lyrics-container]")
            .expect("static lyric container selector is valid");

        let mut lyric = String::new();
        for container in document.select(&selector) {
            // A container nested in another container is already covered by its ancestor.
            let nested = container
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(|el| el.value().attr(LYRIC_CONTAINER_ATTR).is_some());
            if !nested {
                extract_text(container, &mut lyric);
            }
        }
        lyric
    }

    /// Append the text of `container` to `out`, turning `<br>` into newlines and skipping
    /// excluded subtrees. Iterative, so deeply nested markup cannot overflow the stack.
    fn extract_text(container: ElementRef<'_>, out: &mut String) {
        let mut skip_depth = 0usize;
        for edge in container.traverse() {
            match edge {
                Edge::Open(node) => match node.value() {
                    Node::Element(_) if skip_depth > 0 => skip_depth += 1,
                    _ if skip_depth > 0 => {}
                    Node::Element(el) if el.attr(EXCLUDE_ATTR) == Some("true") => skip_depth = 1,
                    Node::Element(el) if el.name() == "br" => out.push('\n'),
                    Node::Text(text) => out.push_str(&text.text),
                    _ => {}
                },
                Edge::Close(node) => {
                    if skip_depth > 0 && node.value().is_element() {
                        skip_depth -= 1;
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::parse;

        #[test]
        fn extracts_text_and_line_breaks() {
            let html = r#"<html><body>
                <div data-lyrics-container="true">[Verse 1]<br>Line one<br><i>Line</i> two</div>
                <div data-lyrics-container="true"><br>[Chorus]<br>Hook</div>
            </body></html>"#;
            assert_eq!(parse(html), "[Verse 1]\nLine one\nLine two\n[Chorus]\nHook");
        }

        #[test]
        fn ignores_text_outside_containers() {
            let html =
                r#"<p>Ad text</p><div data-lyrics-container="true">Lyric</div><p>Footer</p>"#;
            assert_eq!(parse(html), "Lyric");
        }

        #[test]
        fn skips_excluded_header() {
            let html = r#"<div data-lyrics-container="true"><div data-exclude-from-selection="true"><span>12 Contributors</span><h2>Song Lyrics</h2></div>[Intro]<br>Hello</div>"#;
            assert_eq!(parse(html), "[Intro]\nHello");
        }

        #[test]
        fn nested_containers_are_not_duplicated() {
            let html = r#"<div data-lyrics-container="true">a<div data-lyrics-container="true">b</div>c</div>"#;
            assert_eq!(parse(html), "abc");
        }

        #[test]
        fn deeply_nested_markup_does_not_overflow() {
            let depth = 50_000;
            let html = format!(
                "<div data-lyrics-container=\"true\">{}deep{}</div>",
                "<span>".repeat(depth),
                "</span>".repeat(depth)
            );
            assert_eq!(parse(&html), "deep");
        }

        #[test]
        fn no_container_yields_empty() {
            assert_eq!(parse("<html><body>nothing</body></html>"), "");
        }
    }
}

pub mod search {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub(crate) struct Body {
        pub meta: Metadata,
        pub response: Option<Response>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Metadata {
        pub status: u16,
        pub message: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Response {
        pub hits: Vec<Hit>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Hit {
        #[serde(rename(deserialize = "type"))]
        pub ty: String,
        pub result: Result,
    }

    /// A song hit from the Genius search API.
    #[derive(Debug, Clone, Deserialize)]
    pub struct Result {
        pub url: String,
        pub title: String,
        pub artist_names: String,
    }
}

#[cfg(test)]
mod tests {
    use super::{Client, improve_query, is_allowed_url, redirect_policy, search};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve `response` verbatim to each of up to `max_conns` connections on a local port.
    fn serve(response: String, max_conns: usize) -> reqwest::Url {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(max_conns) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        reqwest::Url::parse(&format!("http://{addr}/lyrics")).unwrap()
    }

    fn redirect_to(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    #[tokio::test]
    async fn redirect_policy_errors_on_blocked_redirect() {
        // Plain-HTTP local server, so build a client with the same policy minus https_only.
        let http = reqwest::Client::builder()
            .redirect(redirect_policy())
            .build()
            .unwrap();
        let url = serve(redirect_to("https://evil.example/x"), 1);
        let err = http.get(url).send().await.unwrap_err();
        assert!(err.is_redirect(), "{err:?}");
        assert!(format!("{err:?}").contains("blocked redirect"), "{err:?}");
    }

    #[tokio::test]
    async fn lyric_fetch_rejects_unfollowed_3xx() {
        // A caller-supplied client that never follows redirects hands back the raw 302.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let client = Client::from_http_client(&http);
        let url = serve(redirect_to("https://genius.com/elsewhere"), 1);
        let err = client.fetch_lyric_page(url).await.unwrap_err();
        assert!(err.to_string().contains("302"), "{err}");
    }

    #[tokio::test]
    async fn lyric_fetch_parses_2xx() {
        let http = reqwest::Client::new();
        let client = Client::from_http_client(&http);
        let body = r#"<div data-lyrics-container="true">a<br>b</div>"#;
        let url = serve(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
            1,
        );
        assert_eq!(client.fetch_lyric_page(url).await.unwrap(), "a\nb");
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
    fn process_lyric_normalizes_section_spacing() {
        assert_eq!(
            Client::process_lyric("[Verse]\na\n[Chorus]\nb\n\n[Outro]\nc"),
            "[Verse]\na\n\n[Chorus]\nb\n\n[Outro]\nc"
        );
    }

    #[test]
    fn only_genius_https_urls_are_allowed() {
        let ok = |u: &str| is_allowed_url(&reqwest::Url::parse(u).unwrap());
        assert!(ok("https://genius.com/Ed-sheeran-shape-of-you-lyrics"));
        assert!(ok("https://www.genius.com/foo"));
        assert!(!ok("http://genius.com/foo"));
        assert!(!ok("https://genius.com.evil.example/foo"));
        assert!(!ok("https://evilgenius.com/foo"));
        assert!(!ok("https://genius.com:8443/foo"));
        assert!(!ok("https://user@genius.com/foo"));
        assert!(!ok("https://169.254.169.254/latest/meta-data"));
        assert!(!ok("file:///etc/passwd"));
    }

    #[tokio::test]
    async fn retrieve_lyric_rejects_foreign_urls_without_fetching() {
        let client = Client::new();
        for url in [
            "http://127.0.0.1:1/",
            "https://169.254.169.254/latest/meta-data",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(client.retrieve_lyric(url).await.is_err(), "{url}");
        }
    }

    #[test]
    fn search_body_deserializes() {
        let json = r#"{
            "meta": {"status": 200},
            "response": {"hits": [
                {"type": "song", "result": {"url": "https://genius.com/a", "title": "A", "artist_names": "X"}},
                {"type": "album", "result": {"url": "https://genius.com/b", "title": "B", "artist_names": "Y"}}
            ]}
        }"#;
        let body: search::Body = serde_json::from_str(json).unwrap();
        assert_eq!(body.meta.status, 200);
        let hits = body.response.unwrap().hits;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].ty, "song");
        assert_eq!(hits[0].result.title, "A");
    }
}
