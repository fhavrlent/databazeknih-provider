use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinSet;

use crate::databazeknih::{
    build_book_url, build_more_info_url, build_search_url, extract_id, extract_path, is_book_page,
    is_unknown_book_redirect, path_of, Client, Page, UpstreamError,
};
use crate::mapping::{
    map_book, parse_book_page, parse_more_info, rank, same_isbn, says_nothing_found,
    search_results, BookMetadata, MoreInfo,
};
use crate::{Config, VERSION};

/// A book costs two requests, so five books is already eleven requests a search.
const MAX_LIMIT: usize = 5;
const MAX_ID_LENGTH: usize = 512;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    dk: Arc<Client>,
}

#[derive(Serialize, Deserialize)]
pub struct SearchResponse {
    pub matches: Vec<BookMetadata>,
    /// Proposed: some matches could not be fetched, so this answer must not be cached as complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

pub fn routes(cfg: Arc<Config>, dk: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{*id}", get(lookup))
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState { cfg, dk })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "Databáze knih",
        "version": VERSION,
        "avatarUrl": "https://www.databazeknih.cz/favicon.ico",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "databazeknih",
        "mediaTypes": ["book", "audiobook", "comic"],
        "lookup": true,
        "supportedFields": [
            "title", "description", "cover", "authors", "publisher", "isbn", "publishedYear",
            "language", "pageCount", "seriesName", "seriesIndex", "genres", "communityRating",
            "narrators", "duration"
        ],
        // A search for an ISBN the site knows lands on the book's page.
        "searchParams": ["isbn", "limit"],
        "idMaxLength": MAX_ID_LENGTH,
        "bookUrlTemplate": "https://www.databazeknih.cz/prehled-knihy/{id}",
        // No `coverShape`: print covers are portrait and audiobook covers square, and BookOrbit
        // drops covers that do not fit a declared shape.
        // The link colour of databazeknih.cz's stylesheet (`--link`).
        "color": "#a03a3b",
        // A search already keeps both upstream slots busy, so BookOrbit sends one call at a time
        // and each gets its whole timeout.
        "rateLimit": { "maxConcurrency": 1, "minIntervalMs": 500 },
    }))
    .into_response()
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let param = |key: &str| q.get(key).map(|v| v.trim()).unwrap_or_default();
    let phrase = param("query");
    let given_isbn = normalize_isbn(param("isbn"));
    let isbn = given_isbn.clone().filter(|isbn| !is_placeholder_isbn(isbn));
    if phrase.is_empty() && isbn.is_none() {
        // A placeholder ISBN is a book without one: nothing to search, not a bad request.
        if given_isbn.is_some() {
            return Json(SearchResponse {
                matches: vec![],
                partial: false,
            })
            .into_response();
        }
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    let limit = param("limit")
        .parse::<usize>()
        .map_or(MAX_LIMIT, |n| n.clamp(1, MAX_LIMIT));
    let deadline = tokio::time::Instant::now() + state.cfg.deadline;

    let result = search_books(
        &state,
        phrase,
        param("author"),
        isbn.as_deref(),
        limit,
        deadline,
    );
    match result.await {
        Ok(answer) => Json(answer).into_response(),
        Err(err) => upstream_error("search", err),
    }
}

async fn search_books(
    state: &AppState,
    phrase: &str,
    author: &str,
    isbn: Option<&str>,
    limit: usize,
    deadline: tokio::time::Instant,
) -> Result<SearchResponse, UpstreamError> {
    // The ISBN first: when the site knows it, its search answers with the book's own page, the
    // edition itself and not a title that merely matches.
    if let Some(isbn) = isbn {
        let page = search_page(state, isbn, deadline).await?;
        if is_book_page(&page.url) {
            let answer = landed_on_book(state, page, Some(isbn), deadline).await?;
            // The site's ISBN search is not exact: an unknown ISBN can land on a book whose
            // recorded ISBN is junk such as "000-000-000". Only an edition that lists the ISBN is
            // its match; anything else is an ISBN the site does not know.
            if answer.matches.iter().any(|book| lists_isbn(book, isbn)) {
                return Ok(answer);
            }
        } else if !says_nothing_found(&page.body) && search_results(&page.body).is_empty() {
            // Results for an ISBN that did not land on a book share its digits by chance, so
            // only a page that says it found nothing, or that lists books, is understood.
            return Err(not_understood(&page));
        }
        if phrase.is_empty() {
            return Ok(SearchResponse {
                matches: vec![],
                partial: false,
            });
        }
    }

    let page = search_page(state, phrase, deadline).await?;
    // A title that is also the whole name of one book can land on it straight away too.
    if is_book_page(&page.url) {
        return landed_on_book(state, page, None, deadline).await;
    }
    let mut hits = rank(search_results(&page.body), phrase, author);
    if hits.is_empty() {
        // BookOrbit remembers an empty answer, so it is given only when the site says so.
        return if says_nothing_found(&page.body) {
            Ok(SearchResponse {
                matches: vec![],
                partial: false,
            })
        } else {
            Err(not_understood(&page))
        };
    }
    hits.truncate(limit);

    let mut tasks = JoinSet::new();
    for (index, hit) in hits.iter().enumerate() {
        let dk = state.dk.clone();
        let (url, provider_id) = (hit.url.clone(), hit.provider_id.clone());
        tasks.spawn(async move { (index, fetch_book(&dk, &url, &provider_id).await) });
    }

    let mut found: Vec<Option<BookMetadata>> = (0..hits.len()).map(|_| None).collect();
    let mut partial = false;
    let mut failure = None;
    loop {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(None) => break,
            Ok(Some(Ok((index, result)))) => match result {
                Ok(Some(book)) => found[index] = Some(book),
                // A page that is gone or does not parse is skipped.
                Ok(None) | Err(UpstreamError::NotFound) => partial = true,
                Err(err) => {
                    partial = true;
                    failure.get_or_insert(err);
                }
            },
            Ok(Some(Err(joined))) => {
                partial = true;
                failure.get_or_insert(UpstreamError::Failed(joined.to_string()));
            }
            Err(_) => {
                tasks.abort_all();
                partial = true;
                failure.get_or_insert(UpstreamError::Failed("book pages timed out".into()));
                break;
            }
        }
    }

    let matches: Vec<BookMetadata> = found.into_iter().flatten().collect();
    if matches.is_empty() && partial {
        return Err(
            failure.unwrap_or_else(|| UpstreamError::Failed("no book page could be read".into()))
        );
    }
    Ok(SearchResponse { matches, partial })
}

/// The answer when a search lands straight on a book's page, within the search's deadline.
async fn landed_on_book(
    state: &AppState,
    page: Page,
    isbn: Option<&str>,
    deadline: tokio::time::Instant,
) -> Result<SearchResponse, UpstreamError> {
    let book = tokio::time::timeout_at(deadline, read_book(&state.dk, page, "", isbn))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("book details timed out".into())));
    match book {
        Ok(Some(book)) => Ok(SearchResponse {
            matches: vec![book],
            partial: false,
        }),
        // The search led to this book, so a page that does not read is a failure, not an absence.
        Ok(None) | Err(UpstreamError::NotFound) => Err(UpstreamError::Failed(
            "the book page a search led to could not be read".into(),
        )),
        Err(err) => Err(err),
    }
}

fn lists_isbn(book: &BookMetadata, isbn: &str) -> bool {
    same_isbn(&book.isbn13, isbn) || same_isbn(&book.isbn10, isbn)
}

fn not_understood(page: &Page) -> UpstreamError {
    UpstreamError::Failed(format!(
        "search page at {} lists no books and does not say it found none",
        path_of(&page.url)
    ))
}

async fn search_page(
    state: &AppState,
    phrase: &str,
    deadline: tokio::time::Instant,
) -> Result<Page, UpstreamError> {
    tokio::time::timeout_at(deadline, state.dk.page(&build_search_url(phrase)))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("search page timed out".into())))
        .map_err(|err| match err {
            UpstreamError::NotFound => UpstreamError::Failed("search page answered 404".into()),
            err => err,
        })
}

/// One book as an answer: its overview page, then its details. `None` when the page has no title.
async fn fetch_book(
    dk: &Client,
    url: &str,
    provider_id: &str,
) -> Result<Option<BookMetadata>, UpstreamError> {
    let page = dk.page(url).await?;
    read_book(dk, page, provider_id, None).await
}

async fn read_book(
    dk: &Client,
    page: Page,
    provider_id: &str,
    searched_isbn: Option<&str>,
) -> Result<Option<BookMetadata>, UpstreamError> {
    if !is_book_page(&page.url) {
        // The site sends an unknown book to its catalogue rather than answering 404. Any other
        // page, a maintenance page or a new address scheme, says nothing about the book.
        return Err(if is_unknown_book_redirect(&page.url) {
            UpstreamError::NotFound
        } else {
            UpstreamError::Failed(format!(
                "a book address led to {} instead of a book page",
                path_of(&page.url)
            ))
        });
    }
    let parsed = parse_book_page(&page.body, Some(&page.url));
    let Some(id) = extract_id(&parsed.provider_id).or_else(|| extract_id(&page.url)) else {
        return Err(UpstreamError::NotFound);
    };
    let more = match dk.page(&build_more_info_url(&id)).await {
        Ok(fragment) => parse_more_info(&fragment.body),
        Err(UpstreamError::NotFound) => MoreInfo::default(),
        Err(err) => return Err(err),
    };
    Ok(map_book(
        &parsed,
        &more,
        &page.url,
        provider_id,
        searched_isbn,
    ))
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let (url, requested) = match (build_book_url(&raw), extract_path(&raw)) {
        (Some(url), Some(requested)) if raw.len() <= MAX_ID_LENGTH => (url, requested),
        _ => return error(StatusCode::NOT_FOUND, "Not found"),
    };

    let result = tokio::time::timeout(state.cfg.deadline, fetch_book(&state.dk, &url, ""))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("book page timed out".into())));

    match result {
        // The id BookOrbit asked for, not the page's current slug: the site renames slugs when a
        // title changes, and BookOrbit files the book under the id it stored.
        Ok(Some(mut book)) => {
            book.provider_id = requested;
            Json(book).into_response()
        }
        Ok(None) => {
            eprintln!("[provider.lookup] [fail] error=\"book page did not parse\" - databazeknih page unreadable");
            error(
                StatusCode::BAD_GATEWAY,
                "databazeknih.cz page could not be read",
            )
        }
        Err(UpstreamError::NotFound) => error(StatusCode::NOT_FOUND, "Not found"),
        Err(err) => upstream_error("lookup", err),
    }
}

/// An ISBN of either length without hyphens or spaces, or nothing when it is not one.
fn normalize_isbn(raw: &str) -> Option<String> {
    let isbn: String = raw
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .collect::<String>()
        .to_uppercase();
    let valid = match isbn.len() {
        13 => isbn.bytes().all(|b| b.is_ascii_digit()),
        10 => {
            isbn[..9].bytes().all(|b| b.is_ascii_digit())
                && isbn[9..].bytes().all(|b| b.is_ascii_digit() || b == b'X')
        }
        _ => false,
    };
    valid.then_some(isbn)
}

/// An ISBN of zeros, such as `0000000000` or `9780000000002`, that badly tagged files carry for
/// "no ISBN". The site stores some books under such placeholders too, so searching one would
/// answer an unrelated book.
fn is_placeholder_isbn(isbn: &str) -> bool {
    let body = match isbn.len() {
        13 => &isbn[3..12],
        _ => &isbn[..9],
    };
    body.bytes().all(|b| b == b'0')
}

/// Status codes are part of the contract: a client backs off on 429 and treats 5xx as a failure,
/// never as "no results".
fn upstream_error(op: &str, err: UpstreamError) -> Response {
    match err {
        UpstreamError::RateLimited {
            retry_after_seconds,
        } => {
            let mut res = error(
                StatusCode::TOO_MANY_REQUESTS,
                "databazeknih.cz is rate limiting this provider",
            );
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            res
        }
        UpstreamError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        UpstreamError::Failed(message) => {
            eprintln!("[provider.{op}] [fail] error={message:?} - databazeknih request failed");
            error(StatusCode::BAD_GATEWAY, "databazeknih.cz is unavailable")
        }
    }
}

/// The refusal to send when a token is configured and the request does not carry it.
fn deny_unauthorized(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let token = state.cfg.auth_token.as_ref()?;
    let given = headers
        .get(header::AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    (!constant_time_eq(given, token.as_bytes()))
        .then(|| error(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn log_requests(req: Request, next: Next) -> Response {
    let (method, path, started) = (
        req.method().clone(),
        req.uri().path().to_string(),
        Instant::now(),
    );
    let res = next.run(req).await;
    eprintln!(
        "[request] [end] method={method} path={path} status={} durationMs={}",
        res.status().as_u16(),
        started.elapsed().as_millis()
    );
    res
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use axum::response::Redirect;

    use super::*;

    const SEARCH_TWO: &str = r#"
      <p class='new'>
        <a class='new' type='book' href='/prehled-knihy/john-sinclair-zaklinac-40113'>Zaklínač</a>
        <br /><span class='pozn'>1998, Jason Dark (p)</span>
      </p>
      <p class='new'>
        <a class='new' type='book' href='/prehled-knihy/zaklinac-posledni-prani-30396'>Zaklínač</a>
        <br /><span class='pozn'>1999, Andrzej Sapkowski</span>
      </p>
    "#;

    const BOOK_HTML: &str = r#"
      <div id="bookDetail">
        <img class="kniha_img coverOnDetail" src="https://www.databazeknih.cz/img/books/30_/30396/bmid_posledni-prani-30396.gif" />
        <h1 class="oddown_zero">Poslední přání</h1>
        <span class="author"><a href="/autori/andrzej-sapkowski-269">Andrzej Sapkowski</a></span>
        <div class="lora lineHeightMid"><a href='/zanry/fantasy-21'>Fantasy</a><br /> 1999 <span class='pozn'>,</span> <a href='/nakladatelstvi/leonardo-338'>Leonardo</a></div>
      </div>
      <div id="left"><p class='new2 odtop'>Zaklínač Geralt.</p></div>
      <script type="application/ld+json">
      { "@type": "Book", "url": "https://www.databazeknih.cz/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396", "inLanguage": "cs" }
      </script>
    "#;

    const BOOK_HTML_2: &str = r#"
      <div id="bookDetail"><h1 class="oddown_zero">Na stopě hrůzy</h1></div>
      <script type="application/ld+json">
      { "@type": "Book", "url": "https://www.databazeknih.cz/prehled-knihy/john-sinclair-zaklinac-40113" }
      </script>
    "#;

    const MORE_HTML: &str = r#"
      <div class='book-details__row'><dt>Počet stran</dt><dd><span>280</span></dd></div>
      <div class='book-details__row'><dt>ISBN</dt><dd>80-85951-14-2</dd></div>
      <div class='book-details__row'><dt>Forma</dt><dd>klasická kniha</dd></div>
    "#;

    const EMPTY_PAGE: &str = "<html><body></body></html>";
    const NOTHING_FOUND: &str = include_str!("../testdata/search-nothing.html");

    /// One canned answer per path, a delay per path, and a log of every request it saw. A
    /// `redirect:` body answers with a redirect to the rest of it, as the site does.
    #[derive(Default)]
    struct Fake {
        pages: Mutex<HashMap<String, (u16, String)>>,
        delays: Mutex<HashMap<String, Duration>>,
        requested: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(pages: &[(&str, u16, &str)]) -> Arc<Self> {
            let fake = Fake::default();
            for (path, status, body) in pages {
                fake.pages
                    .lock()
                    .unwrap()
                    .insert(path.to_string(), (*status, body.to_string()));
            }
            Arc::new(fake)
        }

        fn requests(&self) -> Vec<String> {
            self.requested.lock().unwrap().clone()
        }
    }

    async fn fake_site(fake: Arc<Fake>) -> String {
        let app = Router::new().fallback(move |req: Request| {
            let fake = fake.clone();
            async move {
                let path = req.uri().path().to_string();
                let logged = match req.uri().query() {
                    Some(query) => format!("{path}?{query}"),
                    None => path.clone(),
                };
                fake.requested.lock().unwrap().push(logged.clone());
                let delay = fake.delays.lock().unwrap().get(&path).copied();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                let page = {
                    let pages = fake.pages.lock().unwrap();
                    pages.get(&logged).or_else(|| pages.get(&path)).cloned()
                };
                match page {
                    Some((429, _)) => (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "42")],
                        "",
                    )
                        .into_response(),
                    Some((_, body)) if body.starts_with("redirect:") => {
                        Redirect::to(&body["redirect:".len()..]).into_response()
                    }
                    Some((status, body)) => {
                        (StatusCode::from_u16(status).unwrap(), body).into_response()
                    }
                    // The site sends unknown books to its catalogue.
                    None if path.starts_with("/prehled-knihy/") => {
                        Redirect::to("/knihy").into_response()
                    }
                    None if path.starts_with("/book-detail-more-info/") => {
                        StatusCode::NO_CONTENT.into_response()
                    }
                    None if path == "/knihy" => (StatusCode::OK, "<h1>Knihy</h1>").into_response(),
                    None => (StatusCode::NOT_FOUND, "<h1>404</h1>").into_response(),
                }
            }
        });
        serve(app).await
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn provider_at(upstream: &str, token: Option<&str>, deadline: Duration) -> String {
        let cfg = Config {
            auth_token: token.map(str::to_string),
            deadline,
        };
        let client = Client::new(upstream, "test", 2, Duration::ZERO);
        serve(routes(Arc::new(cfg), Arc::new(client))).await
    }

    async fn provider(fake: Arc<Fake>) -> String {
        provider_at(&fake_site(fake).await, None, Duration::from_secs(5)).await
    }

    async fn get(
        base: &str,
        path: &str,
        auth: Option<&str>,
    ) -> (u16, serde_json::Value, HeaderMap) {
        let mut req = reqwest::Client::new().get(format!("{base}{path}"));
        if let Some(auth) = auth {
            req = req.header("Authorization", auth);
        }
        let res = req.send().await.unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        let body =
            serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(serde_json::Value::Null);
        (status, body, headers)
    }

    fn matches(body: &serde_json::Value) -> &Vec<serde_json::Value> {
        body["matches"].as_array().expect("a matches list")
    }

    fn sapkowski_site() -> Arc<Fake> {
        Fake::with(&[
            ("/vyhledavani/knihy", 200, SEARCH_TWO),
            (
                "/prehled-knihy/zaklinac-posledni-prani-30396",
                200,
                BOOK_HTML,
            ),
            (
                "/prehled-knihy/john-sinclair-zaklinac-40113",
                200,
                BOOK_HTML_2,
            ),
            ("/book-detail-more-info/30396", 200, MORE_HTML),
        ])
    }

    #[tokio::test]
    async fn info_declares_the_contract() {
        let base = provider(Fake::with(&[])).await;
        let (status, info, _) = get(&base, "/provider/info", None).await;
        assert_eq!(status, 200);
        assert_eq!(
            (
                info["key"].as_str(),
                info["name"].as_str(),
                info["lookup"].as_bool(),
                info["authenticationRequired"].as_bool()
            ),
            (
                Some("databazeknih"),
                Some("Databáze knih"),
                Some(true),
                Some(false)
            )
        );
        assert_eq!(
            info["bookUrlTemplate"],
            "https://www.databazeknih.cz/prehled-knihy/{id}"
        );
        assert_eq!(info["searchParams"], json!(["isbn", "limit"]));
        assert_eq!(info["color"], "#a03a3b");
        // Print covers are portrait and audiobook covers square.
        assert!(info.get("coverShape").is_none());
    }

    #[tokio::test]
    async fn search_by_title_ranks_the_author_first_then_fetches_details() {
        let fake = sapkowski_site();
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(
            &base,
            "/search?query=Zakl%C3%ADna%C4%8D&author=Andrzej%20Sapkowski&mediaType=book",
            None,
        )
        .await;
        assert_eq!((status, matches(&body).len()), (200, 2));
        assert!(body.get("partial").is_none());
        let book = &matches(&body)[0];
        assert_eq!(book["providerId"], "zaklinac-zaklinac-posledni-prani-30396");
        assert_eq!(book["title"], "Poslední přání");
        assert_eq!(book["authors"], json!(["Andrzej Sapkowski"]));
        assert_eq!(book["publisher"], "Leonardo");
        assert_eq!(book["publishedYear"], "1999");
        assert_eq!(book["languageCode"], "cs");
        assert_eq!(book["pages"], 280);
        assert_eq!(book["isbn10"], "8085951142");
        assert!(book.get("genres").is_none());
        assert_eq!(
            book["cover"],
            "https://www.databazeknih.cz/img/books/30_/30396/big_posledni-prani-30396.gif"
        );
        assert_eq!(book["description"], "Zaklínač Geralt.");
        assert_eq!(matches(&body)[1]["title"], "Na stopě hrůzy");
        // The author is not sent: the site matches titles, and an author in the phrase finds less.
        assert_eq!(
            fake.requests()[0],
            "/vyhledavani/knihy?q=Zakl%C3%ADna%C4%8D&lang=czsk"
        );
    }

    #[tokio::test]
    async fn search_requires_a_query_or_an_isbn() {
        let fake = Fake::with(&[]);
        let base = provider(fake.clone()).await;
        assert_eq!(get(&base, "/search", None).await.0, 400);
        assert_eq!(get(&base, "/search?query=%20&author=x", None).await.0, 400);
        assert_eq!(get(&base, "/search?isbn=12", None).await.0, 400);
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn search_by_isbn_lands_on_the_book() {
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/vyhledavani/knihy?q=8085951142&lang=czsk".into(),
            (
                302,
                "redirect:/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396".into(),
            ),
        );
        fake.pages.lock().unwrap().insert(
            "/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396".into(),
            (200, BOOK_HTML.into()),
        );
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(
            &base,
            "/search?isbn=80-85951-14-2&query=Zakl%C3%ADna%C4%8D",
            None,
        )
        .await;
        assert_eq!((status, matches(&body).len()), (200, 1));
        assert_eq!(
            matches(&body)[0]["providerId"],
            "zaklinac-zaklinac-posledni-prani-30396"
        );
        assert_eq!(matches(&body)[0]["pages"], 280);
        // The title search is not needed.
        assert_eq!(
            fake.requests(),
            [
                "/vyhledavani/knihy?q=8085951142&lang=czsk",
                "/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396",
                "/book-detail-more-info/30396",
            ]
        );
    }

    #[tokio::test]
    async fn an_unknown_isbn_falls_back_to_the_title_or_answers_none() {
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/vyhledavani/knihy?q=9780000000002&lang=czsk".into(),
            (200, NOTHING_FOUND.into()),
        );
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(&base, "/search?isbn=9780000000002", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        let (status, body, _) = get(
            &base,
            "/search?isbn=9780000000002&query=Zakl%C3%ADna%C4%8D",
            None,
        )
        .await;
        assert_eq!((status, matches(&body).len()), (200, 2));
    }

    #[tokio::test]
    async fn search_respects_the_limit() {
        let fake = sapkowski_site();
        let base = provider(fake.clone()).await;
        let (_, body, _) = get(&base, "/search?query=x&author=Sapkowski&limit=1", None).await;
        assert_eq!(matches(&body).len(), 1);
        assert_eq!(matches(&body)[0]["title"], "Poslední přání");
        assert_eq!(fake.requests().len(), 3);
    }

    #[tokio::test]
    async fn real_search_fetches_five_books() {
        let fake = Fake::with(&[(
            "/vyhledavani/knihy",
            200,
            include_str!("../testdata/search-zaklinac.html"),
        )]);
        let hits = search_results(include_str!("../testdata/search-zaklinac.html"));
        for hit in &hits {
            fake.pages.lock().unwrap().insert(
                hit.url.trim_start_matches(crate::databazeknih::SITE).into(),
                (200, include_str!("../testdata/book-30396.html").into()),
            );
        }
        fake.pages.lock().unwrap().insert(
            "/book-detail-more-info/30396".into(),
            (200, include_str!("../testdata/more-30396.html").into()),
        );
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(&base, "/search?query=Zakl%C3%ADna%C4%8D&limit=10", None).await;
        assert_eq!((status, matches(&body).len()), (200, 5));
        assert_eq!(matches(&body)[0]["pages"], 280);
        assert_eq!(fake.requests().len(), 11);
    }

    #[tokio::test]
    async fn unreadable_book_pages_fail_the_search_instead_of_answering_nothing() {
        let fake = Fake::with(&[
            ("/vyhledavani/knihy", 200, SEARCH_TWO),
            (
                "/prehled-knihy/zaklinac-posledni-prani-30396",
                200,
                EMPTY_PAGE,
            ),
            (
                "/prehled-knihy/john-sinclair-zaklinac-40113",
                200,
                EMPTY_PAGE,
            ),
        ]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
    }

    #[tokio::test]
    async fn a_skipped_book_marks_the_answer_partial() {
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/prehled-knihy/john-sinclair-zaklinac-40113".into(),
            (503, String::new()),
        );
        let base = provider(fake).await;
        let (status, body, _) = get(&base, "/search?query=x", None).await;
        assert_eq!(
            (status, matches(&body).len(), body["partial"].as_bool()),
            (200, 1, Some(true))
        );
    }

    #[tokio::test]
    async fn no_results_is_an_empty_answer() {
        let fake = Fake::with(&[("/vyhledavani/knihy", 200, NOTHING_FOUND)]);
        let base = provider(fake).await;
        let (status, body, _) = get(&base, "/search?query=qxzwv", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        assert!(body.get("partial").is_none());
    }

    #[tokio::test]
    async fn a_search_page_that_is_not_understood_is_a_failure_not_an_empty_answer() {
        // A redesigned results page lists nothing the parser knows and does not say it found none.
        let fake = Fake::with(&[("/vyhledavani/knihy", 200, EMPTY_PAGE)]);
        let base = provider(fake.clone()).await;
        assert_eq!(get(&base, "/search?query=Krakatit", None).await.0, 502);
        assert_eq!(get(&base, "/search?isbn=9788086201559", None).await.0, 502);
        assert_eq!(
            get(&base, "/search?isbn=9788086201559&query=Krakatit", None)
                .await
                .0,
            502
        );
    }

    #[tokio::test]
    async fn a_search_that_lands_on_a_book_keeps_the_deadline() {
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/vyhledavani/knihy?q=Posledn%C3%AD+p%C5%99%C3%A1n%C3%AD&lang=czsk".into(),
            (
                302,
                "redirect:/prehled-knihy/zaklinac-posledni-prani-30396".into(),
            ),
        );
        fake.delays.lock().unwrap().insert(
            "/book-detail-more-info/30396".into(),
            Duration::from_secs(3),
        );
        let base = provider_at(&fake_site(fake).await, None, Duration::from_millis(700)).await;
        let started = Instant::now();
        let (status, _, _) = get(
            &base,
            "/search?query=Posledn%C3%AD%20p%C5%99%C3%A1n%C3%AD",
            None,
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(status, 502);
    }

    #[tokio::test]
    async fn an_isbn_search_that_lands_on_a_book_without_that_isbn_is_no_match() {
        // Seen on databazeknih.cz: 9780000000002 lands on a book recorded with ISBN 000-000-000.
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/vyhledavani/knihy?q=9780000000002&lang=czsk".into(),
            (
                302,
                "redirect:/prehled-knihy/pred-ocima-vesnice-370474".into(),
            ),
        );
        fake.pages.lock().unwrap().insert(
            "/prehled-knihy/pred-ocima-vesnice-370474".into(),
            (
                200,
                r#"<div id="bookDetail"><h1 class="oddown_zero">Před očima vesnice</h1></div>
                <script type="application/ld+json">{ "@type": "Book", "isbn": "000-000-000",
                "url": "https://www.databazeknih.cz/prehled-knihy/pred-ocima-vesnice-370474" }</script>"#
                    .into(),
            ),
        );
        fake.pages.lock().unwrap().insert(
            "/book-detail-more-info/370474".into(),
            (200, "<dt>ISBN</dt><dd>000-000-000</dd>".into()),
        );
        let base = provider(fake).await;
        let (status, body, _) = get(&base, "/search?isbn=9780000000002", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0), "{body}");
        // With a title, the title is searched instead.
        let (status, body, _) = get(
            &base,
            "/search?isbn=9780000000002&query=Zakl%C3%ADna%C4%8D&author=Sapkowski",
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(matches(&body)[0]["title"], "Poslední přání");
    }

    #[tokio::test]
    async fn an_isbn_13_search_matches_an_edition_that_lists_its_isbn_10() {
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/vyhledavani/knihy?q=9788085951141&lang=czsk".into(),
            (
                302,
                "redirect:/prehled-knihy/zaklinac-posledni-prani-30396".into(),
            ),
        );
        let base = provider(fake).await;
        let (status, body, _) = get(&base, "/search?isbn=978-80-85951-14-1", None).await;
        assert_eq!((status, matches(&body).len()), (200, 1), "{body}");
        assert_eq!(matches(&body)[0]["isbn10"], "8085951142");
    }

    #[tokio::test]
    async fn an_isbn_search_reports_the_searched_isbn_of_an_edition_with_two() {
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/vyhledavani/knihy?q=8085951142&lang=czsk".into(),
            (
                302,
                "redirect:/prehled-knihy/zaklinac-posledni-prani-30396".into(),
            ),
        );
        fake.pages.lock().unwrap().insert(
            "/book-detail-more-info/30396".into(),
            (
                200,
                "<dt>ISBN</dt><dd>978-80-7477-079-1, 80-85951-14-2</dd>".into(),
            ),
        );
        let base = provider(fake).await;
        let (_, body, _) = get(&base, "/search?isbn=8085951142", None).await;
        let book = &matches(&body)[0];
        assert_eq!(book["isbn10"], "8085951142");
        assert!(book.get("isbn13").is_none(), "{book}");
    }

    #[tokio::test]
    async fn search_page_failure_is_a_bad_gateway() {
        let fake = Fake::with(&[("/vyhledavani/knihy", 503, "")]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
    }

    #[tokio::test]
    async fn a_slow_book_is_left_out_when_the_deadline_passes() {
        let fake = sapkowski_site();
        fake.delays.lock().unwrap().insert(
            "/prehled-knihy/john-sinclair-zaklinac-40113".into(),
            Duration::from_secs(3),
        );
        let base = provider_at(&fake_site(fake).await, None, Duration::from_millis(700)).await;
        let started = Instant::now();
        let (status, body, _) = get(&base, "/search?query=x&author=Sapkowski", None).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            (status, matches(&body).len(), body["partial"].as_bool()),
            (200, 1, Some(true))
        );
        assert_eq!(matches(&body)[0]["title"], "Poslední přání");
    }

    #[tokio::test]
    async fn lookup_by_stored_ids() {
        let fake = sapkowski_site();
        fake.pages.lock().unwrap().insert(
            "/prehled-knihy/kniha-30396".into(),
            (
                302,
                "redirect:/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396".into(),
            ),
        );
        fake.pages.lock().unwrap().insert(
            "/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396".into(),
            (200, BOOK_HTML.into()),
        );
        let base = provider(fake.clone()).await;
        fake.pages.lock().unwrap().insert(
            "/prehled-knihy/stary-nazev-30396".into(),
            (
                302,
                "redirect:/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396".into(),
            ),
        );
        // Each answer carries the id that was asked for, whatever the page's current slug.
        for (id, answered) in [
            ("30396", "30396"),
            (
                "zaklinac-zaklinac-posledni-prani-30396",
                "zaklinac-zaklinac-posledni-prani-30396",
            ),
            ("stary-nazev-30396", "stary-nazev-30396"),
            (
                "https%3A%2F%2Fwww.databazeknih.cz%2Fprehled-knihy%2Fzaklinac-zaklinac-posledni-prani-30396",
                "zaklinac-zaklinac-posledni-prani-30396",
            ),
        ] {
            let (status, book, _) = get(&base, &format!("/book/{id}"), None).await;
            assert_eq!(
                (status, book["providerId"].as_str(), book["title"].as_str()),
                (200, Some(answered), Some("Poslední přání")),
                "{id}"
            );
        }
        assert_eq!(fake.requests()[0], "/prehled-knihy/kniha-30396");
    }

    #[tokio::test]
    async fn lookup_answers_404_for_unknown_and_invalid_ids() {
        let fake = Fake::with(&[]);
        let base = provider(fake.clone()).await;
        // The site redirects an unknown book to its catalogue.
        assert_eq!(get(&base, "/book/999999", None).await.0, 404);
        assert_eq!(fake.requests(), ["/prehled-knihy/kniha-999999", "/knihy"]);
        let long = format!("{}-1", "a".repeat(MAX_ID_LENGTH));
        for id in [
            "not-a-valid-id",
            "..",
            "conformance-unknown-id",
            long.as_str(),
        ] {
            assert_eq!(
                get(&base, &format!("/book/{id}"), None).await.0,
                404,
                "{id}"
            );
        }
        assert_eq!(fake.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_lookup_that_lands_anywhere_but_the_catalogue_is_a_failure() {
        // A maintenance page or a new address scheme says nothing about the book.
        let fake = Fake::with(&[
            ("/prehled-knihy/kniha-30396", 302, "redirect:/"),
            ("/", 200, "<h1>Údržba</h1>"),
        ]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/book/30396", None).await.0, 502);
    }

    #[tokio::test]
    async fn lookup_of_an_unreadable_page_is_a_bad_gateway() {
        let fake = Fake::with(&[
            ("/prehled-knihy/kniha-1", 200, EMPTY_PAGE),
            ("/prehled-knihy/kniha-2", 503, ""),
        ]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/book/1", None).await.0, 502);
        assert_eq!(get(&base, "/book/2", None).await.0, 502);
    }

    #[tokio::test]
    async fn network_failure_is_a_bad_gateway() {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let base = provider_at(&closed, None, Duration::from_secs(5)).await;
        assert_eq!(get(&base, "/book/30396", None).await.0, 502);
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
    }

    #[tokio::test]
    async fn upstream_rate_limit_is_passed_on_and_held() {
        let fake = Fake::with(&[("/prehled-knihy/kniha-30396", 429, "")]);
        let base = provider(fake.clone()).await;
        let (status, _, headers) = get(&base, "/book/30396", None).await;
        assert_eq!(
            (
                status,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
        // The pause holds: the next call is refused without asking databazeknih.cz again.
        let (status, _, headers) = get(&base, "/search?query=x", None).await;
        let retry_after: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert_eq!(status, 429);
        assert!((40..=42).contains(&retry_after), "{retry_after}");
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn auth_token() {
        let fake = sapkowski_site();
        let upstream = fake_site(fake).await;
        let base = provider_at(&upstream, Some("Bearer s3cret"), Duration::from_secs(5)).await;
        assert_eq!(
            get(&base, "/book/zaklinac-posledni-prani-30396", None)
                .await
                .0,
            401
        );
        assert_eq!(
            get(&base, "/search?query=x", Some("Bearer nope")).await.0,
            401
        );
        assert_eq!(
            get(
                &base,
                "/book/zaklinac-posledni-prani-30396",
                Some("Bearer s3cret")
            )
            .await
            .0,
            200
        );
        let (status, info, _) = get(&base, "/provider/info", None).await;
        assert_eq!(
            (status, info["authenticationRequired"].as_bool()),
            (200, Some(true))
        );
    }

    #[test]
    fn isbns_are_normalized() {
        assert_eq!(
            normalize_isbn("978-80-7477-079-1").as_deref(),
            Some("9788074770791")
        );
        assert_eq!(
            normalize_isbn("80-85951-14-x").as_deref(),
            Some("808595114X")
        );
        assert_eq!(normalize_isbn("abc"), None);
        assert_eq!(normalize_isbn(""), None);
    }

    #[test]
    fn placeholder_isbns_are_told_apart() {
        for placeholder in ["0000000000", "9780000000002", "9790000000001"] {
            assert!(is_placeholder_isbn(placeholder), "{placeholder}");
        }
        for real in ["8085951142", "9788074770791", "0261102214", "9780261102217"] {
            assert!(!is_placeholder_isbn(real), "{real}");
        }
    }

    #[tokio::test]
    async fn a_placeholder_isbn_is_not_searched() {
        let fake = sapkowski_site();
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(&base, "/search?isbn=0000000000", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
        // With a title, only the title is searched.
        let (status, body, _) = get(
            &base,
            "/search?isbn=978-0-00-000000-2&query=x&author=Sapkowski&limit=1",
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(matches(&body)[0]["title"], "Poslední přání");
        assert!(
            !fake.requests().iter().any(|r| r.contains("q=978")),
            "{:?}",
            fake.requests()
        );
    }
}
