use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::header::{ACCEPT, ACCEPT_LANGUAGE, RETRY_AFTER};
use axum::http::HeaderMap;
use regex::Regex;
use reqwest::Url;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Where every link in an answer points, whatever `DATABAZEKNIH_SITE` fetches from.
pub const SITE: &str = "https://www.databazeknih.cz";
const SEARCH_PATH: &str = "/vyhledavani/knihy";
const BOOK_PATH_PREFIX: &str = "/prehled-knihy/";
const MORE_INFO_PATH: &str = "/book-detail-more-info/";
const MAX_BODY_BYTES: usize = 4 << 20;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 60;

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

/// A fetched page: where the site's redirects ended, as a public databazeknih.cz address, and its
/// HTML. A search for an ISBN the site knows lands on the book's own page.
pub struct Page {
    pub url: String,
    pub body: String,
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    slots: Semaphore,
    interval: Duration,
    next: Mutex<Instant>,
    /// Set when databazeknih.cz answers 429; nothing is sent to it again before then.
    paused_until: Mutex<Option<Instant>>,
}

impl Client {
    pub fn new(base: &str, user_agent: &str, concurrency: usize, interval: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            slots: Semaphore::new(concurrency),
            interval,
            next: Mutex::new(Instant::now()),
            paused_until: Mutex::new(None),
        }
    }

    /// A page on databazeknih.cz, given by its public address. An answer with no content, which
    /// the details fragment gives for an unknown book, is `NotFound`.
    pub async fn page(&self, url: &str) -> Result<Page, UpstreamError> {
        let target = match url.strip_prefix(SITE) {
            Some(rest) => format!("{}{rest}", self.base),
            None => url.to_string(),
        };
        let _permit = self.acquire().await?;
        // Errors drop the URL: it carries the title someone searched for.
        let failed = |error: reqwest::Error| UpstreamError::Failed(error.without_url().to_string());
        let mut res = self
            .http
            .get(target)
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(ACCEPT_LANGUAGE, "cs-CZ,cs;q=0.9,en;q=0.8")
            .send()
            .await
            .map_err(failed)?;

        match res.status().as_u16() {
            204 | 404 | 410 => return Err(UpstreamError::NotFound),
            429 => {
                let retry_after_seconds = retry_after(res.headers());
                self.pause(Duration::from_secs(retry_after_seconds));
                return Err(UpstreamError::RateLimited {
                    retry_after_seconds,
                });
            }
            200..=299 => {}
            status => {
                return Err(UpstreamError::Failed(format!(
                    "databazeknih.cz answered {status}"
                )))
            }
        }

        let landed = self.public_url(res.url());
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(failed)? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(UpstreamError::Failed(format!(
                    "page over {MAX_BODY_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Page {
            url: landed,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    /// An address on the fetched site, rewritten to the public one.
    fn public_url(&self, url: &Url) -> String {
        let url = url.as_str();
        match url.strip_prefix(&self.base) {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '?']) => {
                format!("{SITE}{rest}")
            }
            _ => url.to_string(),
        }
    }

    /// At most `concurrency` requests in flight, each starting at least `interval` after the last.
    async fn acquire(&self) -> Result<SemaphorePermit<'_>, UpstreamError> {
        self.check_paused()?;
        let permit = self
            .slots
            .acquire()
            .await
            .map_err(|error| UpstreamError::Failed(error.to_string()))?;
        let start = {
            let mut next = self.next.lock().expect("limiter lock poisoned");
            let start = (*next).max(Instant::now());
            *next = start + self.interval;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        self.check_paused()?;
        Ok(permit)
    }

    fn pause(&self, by: Duration) {
        let until = Instant::now() + by;
        let mut paused = self.paused_until.lock().expect("pause lock poisoned");
        if paused.is_none_or(|current| current < until) {
            *paused = Some(until);
        }
    }

    fn check_paused(&self) -> Result<(), UpstreamError> {
        let paused = *self.paused_until.lock().expect("pause lock poisoned");
        match paused {
            Some(until) if until > Instant::now() => Err(UpstreamError::RateLimited {
                retry_after_seconds: (until - Instant::now()).as_secs_f64().ceil().max(1.0) as u64,
            }),
            _ => Ok(()),
        }
    }
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
}

/// A book link on any of the book's tabs: `/prehled-knihy/`, `/knihy/` (comments),
/// `/dalsi-vydani/` (editions) and so on all end in the same slug and number.
static BOOK_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"/(?:prehled-knihy|knihy|dalsi-vydani|obalka-knihy|hodnoceni-knihy)/((?:[a-z0-9]+-)*([0-9]+))(?:[/?#]|$)",
    )
    .expect("valid pattern")
});
static BARE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^((?:[a-z0-9]+-)*([0-9]+))$").expect("valid pattern"));

/// The search page for a phrase. The site searches Czech-language books unless told otherwise.
pub fn build_search_url(phrase: &str) -> String {
    let mut url = Url::parse(&format!("{SITE}{SEARCH_PATH}")).expect("valid search URL");
    url.query_pairs_mut().append_pair("q", phrase);
    url.to_string()
}

/// The overview page of a stored id. The site redirects any slug to the canonical one, but a
/// bare number to its home page, so a bare number gets a placeholder slug.
pub fn build_book_url(provider_id: &str) -> Option<String> {
    let path = extract_path(provider_id)?;
    let path = if path.bytes().all(|b| b.is_ascii_digit()) {
        format!("kniha-{path}")
    } else {
        path
    };
    Some(format!("{SITE}{BOOK_PATH_PREFIX}{path}"))
}

/// The fragment behind the page's "Více info..." link: pages, ISBN, translators, narrators,
/// running time and the edition's form.
pub fn build_more_info_url(numeric_id: &str) -> String {
    format!("{SITE}{MORE_INFO_PATH}{numeric_id}")
}

/// The id: the slug and number after `/prehled-knihy/`, such as `zaklinac-posledni-prani-30396`,
/// or a bare number.
pub fn extract_path(value: &str) -> Option<String> {
    if let Some(captures) = BOOK_LINK.captures(value) {
        return Some(captures[1].to_string());
    }
    let trimmed = value.trim().trim_matches('/');
    BARE_ID
        .captures(trimmed)
        .map(|captures| captures[1].to_string())
}

/// The book's number alone, which tells two links to the same book apart from two books.
pub fn extract_id(value: &str) -> Option<String> {
    if let Some(captures) = BOOK_LINK.captures(value) {
        return Some(captures[2].to_string());
    }
    let trimmed = value.trim().trim_matches('/');
    BARE_ID
        .captures(trimmed)
        .map(|captures| captures[2].to_string())
}

/// Whether an address is a book's overview page, as opposed to a search page or the catalogue
/// the site redirects unknown books to.
pub fn is_book_page(url: &str) -> bool {
    Url::parse(url).ok().is_some_and(|url| {
        url.path().starts_with(BOOK_PATH_PREFIX) && BOOK_LINK.is_match(url.path())
    })
}

pub fn resolve_url(value: &str) -> Option<String> {
    if let Some(rest) = value.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    Url::parse(&format!("{SITE}/"))
        .ok()?
        .join(value)
        .ok()
        .map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_encodes_the_phrase() {
        assert_eq!(
            build_search_url("Zaklínač: Poslední přání"),
            "https://www.databazeknih.cz/vyhledavani/knihy?q=Zakl%C3%ADna%C4%8D%3A+Posledn%C3%AD+p%C5%99%C3%A1n%C3%AD"
        );
    }

    #[test]
    fn extract_path_and_id() {
        for (input, path, id) in [
            (
                "https://www.databazeknih.cz/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396",
                Some("zaklinac-zaklinac-posledni-prani-30396"),
                Some("30396"),
            ),
            (
                "/knihy/zaklinac-zaklinac-posledni-prani-30396#cmts",
                Some("zaklinac-zaklinac-posledni-prani-30396"),
                Some("30396"),
            ),
            (
                "/prehled-knihy/harry-potter-harry-potter-a-kamen-mudrcu-10?x=1",
                Some("harry-potter-harry-potter-a-kamen-mudrcu-10"),
                Some("10"),
            ),
            (
                "zaklinac-posledni-prani-272481",
                Some("zaklinac-posledni-prani-272481"),
                Some("272481"),
            ),
            ("30396", Some("30396"), Some("30396")),
            ("/autori/andrzej-sapkowski-269", None, None),
            ("not-a-valid-id", None, None),
            ("..", None, None),
            ("Zaklínač-1", None, None),
        ] {
            assert_eq!(extract_path(input).as_deref(), path, "{input}");
            assert_eq!(extract_id(input).as_deref(), id, "{input}");
        }
    }

    #[test]
    fn book_url_for_stored_ids() {
        assert_eq!(
            build_book_url("zaklinac-posledni-prani-272481").as_deref(),
            Some("https://www.databazeknih.cz/prehled-knihy/zaklinac-posledni-prani-272481")
        );
        assert_eq!(
            build_book_url("30396").as_deref(),
            Some("https://www.databazeknih.cz/prehled-knihy/kniha-30396")
        );
        assert_eq!(build_book_url("../admin"), None);
    }

    #[test]
    fn book_pages_are_told_from_other_pages() {
        assert!(is_book_page(
            "https://www.databazeknih.cz/prehled-knihy/posledni-prani-497285"
        ));
        assert!(!is_book_page("https://www.databazeknih.cz/knihy"));
        assert!(!is_book_page(
            "https://www.databazeknih.cz/vyhledavani/knihy?q=9788074770791"
        ));
        assert!(!is_book_page("https://www.databazeknih.cz/"));
    }

    #[test]
    fn resolve_url_handles_relative_and_protocol_relative_links() {
        for (input, want) in [
            (
                "/prehled-knihy/a-1",
                "https://www.databazeknih.cz/prehled-knihy/a-1",
            ),
            (
                "img/content/x.gif",
                "https://www.databazeknih.cz/img/content/x.gif",
            ),
            ("//cdn.example.com/c.jpg", "https://cdn.example.com/c.jpg"),
        ] {
            assert_eq!(resolve_url(input).as_deref(), Some(want), "{input}");
        }
    }

    #[test]
    fn public_url_rewrites_the_fetched_site() {
        let client = Client::new("http://127.0.0.1:9/", "test", 1, Duration::ZERO);
        let fetched = Url::parse("http://127.0.0.1:9/prehled-knihy/a-1").unwrap();
        assert_eq!(
            client.public_url(&fetched),
            "https://www.databazeknih.cz/prehled-knihy/a-1"
        );
    }

    #[tokio::test]
    async fn limiter_spaces_request_starts() {
        let client = Client::new("http://127.0.0.1:9", "test", 2, Duration::from_millis(100));
        let started = Instant::now();
        for _ in 0..3 {
            drop(client.acquire().await.unwrap());
        }
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn a_pause_refuses_requests_until_it_ends() {
        let client = Client::new("http://127.0.0.1:9", "test", 2, Duration::ZERO);
        client.pause(Duration::from_secs(30));
        match client.acquire().await {
            Err(UpstreamError::RateLimited {
                retry_after_seconds,
            }) => assert!((29..=30).contains(&retry_after_seconds)),
            other => panic!("expected a pause, got {:?}", other.map(|_| ())),
        };
    }
}
