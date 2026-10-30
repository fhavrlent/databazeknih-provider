//! Failures against the real databazeknih.cz. The real site cannot be made to fail on demand, so
//! these put a small proxy in front of it: every page is the site's own, fetched live, and only
//! the one failure a test names is injected (a redesigned page, a slow or failing request, a
//! redirect to a maintenance page, a 429). They are ignored by `cargo test`; run them with the
//! live tests, `cargo test live -- --ignored --test-threads=1`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::Value;

use crate::databazeknih::{Client, SITE};
use crate::server::routes;
use crate::{Config, VERSION};

/// One injected failure, for requests whose path and query contain `pattern`.
#[derive(Clone)]
enum Fault {
    /// Waits this long, then passes the real answer on.
    Delay(Duration),
    /// Answers this status instead of asking the site.
    Status(u16),
    /// Answers 429 with this `Retry-After`.
    RateLimit(u64),
    /// Answers a redirect to this path of the site instead of asking it.
    Redirect(&'static str),
    /// Passes the real page on with every `from` replaced by `to`.
    Rewrite(&'static str, &'static str),
}

struct Proxy {
    base: String,
    /// Every path and query the provider asked for, to show the site was asked.
    seen: Arc<Mutex<Vec<String>>>,
}

/// A proxy that forwards to the real site and injects `faults`. Redirects are passed on, not
/// followed, so the provider follows them back through the proxy.
async fn proxy(faults: Vec<(&'static str, Fault)>) -> Proxy {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!(
            "bookorbit-databazeknih-provider/{VERSION} (fault test)"
        ))
        .build()
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().fallback(move |req: Request| {
        let (http, faults, log) = (http.clone(), faults.clone(), log.clone());
        async move {
            let wanted = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_default();
            log.lock().unwrap().push(wanted.clone());
            let fault = faults
                .iter()
                .find(|(pattern, _)| wanted.contains(pattern))
                .map(|(_, fault)| fault.clone());
            match fault {
                Some(Fault::Status(code)) => {
                    return StatusCode::from_u16(code).unwrap().into_response()
                }
                Some(Fault::RateLimit(seconds)) => {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, seconds.to_string())],
                    )
                        .into_response()
                }
                Some(Fault::Redirect(to)) => {
                    return (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
                }
                Some(Fault::Delay(by)) => tokio::time::sleep(by).await,
                _ => {}
            }
            let res = http.get(format!("{SITE}{wanted}")).send().await.unwrap();
            let status = res.status();
            let location = res
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim_start_matches(SITE).to_string());
            let content_type = res.headers().get(header::CONTENT_TYPE).cloned();
            let mut body = res.bytes().await.unwrap().to_vec();
            if let Some(Fault::Rewrite(from, to)) = fault {
                body = String::from_utf8_lossy(&body)
                    .replace(from, to)
                    .into_bytes();
            }
            let mut out = Response::new(Body::from(body));
            *out.status_mut() = status;
            if let Some(location) = location {
                out.headers_mut()
                    .insert(header::LOCATION, HeaderValue::from_str(&location).unwrap());
            }
            if let Some(content_type) = content_type {
                out.headers_mut().insert(header::CONTENT_TYPE, content_type);
            }
            out
        }
    });
    Proxy {
        base: serve(app).await,
        seen,
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The provider, fetching from `site` with production pacing.
async fn provider(site: &str, deadline: Duration, token: Option<&str>) -> String {
    let cfg = Config {
        auth_token: token.map(str::to_string),
        deadline,
    };
    let client = Client::new(site, "test", 2, Duration::from_millis(400));
    serve(routes(Arc::new(cfg), Arc::new(client))).await
}

async fn get(base: &str, path: &str, auth: Option<&str>) -> (u16, Value, Option<String>) {
    let mut req = reqwest::Client::new().get(format!("{base}{path}"));
    if let Some(auth) = auth {
        req = req.header("Authorization", auth);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(Value::Null);
    (status, body, retry_after)
}

fn matches(body: &Value) -> &Vec<Value> {
    body["matches"].as_array().expect("a matches list")
}

const SECONDS_8: Duration = Duration::from_secs(8);

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_a_redesigned_results_page_is_a_failure_not_an_empty_answer() {
    // The real results page, with the result links marked up differently.
    let site = proxy(vec![(
        "/vyhledavani/knihy",
        Fault::Rewrite("type='book'", "type='kniha'"),
    )])
    .await;
    let base = provider(&site.base, SECONDS_8, None).await;
    let (status, body, _) = get(
        &base,
        "/search?query=Krakatit&author=Karel%20%C4%8Capek",
        None,
    )
    .await;
    assert_eq!(status, 502, "{body}");
    assert!(site
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.contains("q=Krakatit")));

    // The site's real "nothing found" page still answers empty through the same proxy.
    let (status, body, _) = get(&base, "/search?query=qxzwvqxzwv", None).await;
    assert_eq!((status, matches(&body).len()), (200, 0), "{body}");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_a_reworded_nothing_found_page_is_a_failure() {
    // The real empty results page, with its "nothing found" sentence reworded.
    let site = proxy(vec![(
        "/vyhledavani/knihy",
        Fault::Rewrite("jsme bohužel nenašli", "jsme nic nenalezli"),
    )])
    .await;
    let base = provider(&site.base, SECONDS_8, None).await;
    assert_eq!(get(&base, "/search?query=qxzwvqxzwv", None).await.0, 502);
    // An unknown ISBN shows the same page.
    assert_eq!(get(&base, "/search?isbn=9788025706305", None).await.0, 502);
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_an_isbn_search_that_lands_on_a_slow_book_keeps_the_deadline() {
    // The site's own ISBN redirect lands on the book; its details answer after 20 s.
    let site = proxy(vec![(
        "/book-detail-more-info/",
        Fault::Delay(Duration::from_secs(20)),
    )])
    .await;
    let base = provider(&site.base, Duration::from_secs(4), None).await;
    let started = Instant::now();
    let (status, body, _) = get(&base, "/search?isbn=9788074770791", None).await;
    let took = started.elapsed();
    assert_eq!(status, 502, "{body}");
    assert!(took < Duration::from_secs(6), "took {took:?}");
    assert!(site
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.starts_with("/prehled-knihy/posledni-prani-497285")));
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_a_slow_book_is_left_out_and_marked_partial() {
    // The comic edition of Krakatit answers after 20 s; the others come back in time.
    let site = proxy(vec![(
        "/prehled-knihy/krakatit-komiks-24657",
        Fault::Delay(Duration::from_secs(20)),
    )])
    .await;
    let base = provider(&site.base, Duration::from_secs(6), None).await;
    let started = Instant::now();
    let (status, body, _) = get(&base, "/search?query=Krakatit&limit=3", None).await;
    let took = started.elapsed();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["partial"], true, "{body}");
    assert!(took < Duration::from_secs(8), "took {took:?}");
    let ids: Vec<&str> = matches(&body)
        .iter()
        .filter_map(|b| b["providerId"].as_str())
        .collect();
    assert!(ids.contains(&"krakatit-688"), "{ids:?}");
    assert!(!ids.contains(&"krakatit-komiks-24657"), "{ids:?}");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_a_failing_book_is_left_out_and_marked_partial() {
    let site = proxy(vec![(
        "/prehled-knihy/krakatit-komiks-24657",
        Fault::Status(503),
    )])
    .await;
    let base = provider(&site.base, SECONDS_8, None).await;
    let (status, body, _) = get(&base, "/search?query=Krakatit&limit=3", None).await;
    assert_eq!(
        (status, body["partial"].as_bool()),
        (200, Some(true)),
        "{body}"
    );
    assert_eq!(matches(&body).len(), 2, "{body}");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_a_failing_search_page_is_a_bad_gateway() {
    let site = proxy(vec![("/vyhledavani/knihy", Fault::Status(503))]).await;
    let base = provider(&site.base, SECONDS_8, None).await;
    assert_eq!(get(&base, "/search?query=Krakatit", None).await.0, 502);
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_a_lookup_redirected_to_a_maintenance_page_is_a_failure() {
    // The real home page instead of the book, as a maintenance redirect would give.
    let site = proxy(vec![("/prehled-knihy/kniha-30396", Fault::Redirect("/"))]).await;
    let base = provider(&site.base, SECONDS_8, None).await;
    let (status, body, _) = get(&base, "/book/30396", None).await;
    assert_eq!(status, 502, "{body}");
    assert!(site.seen.lock().unwrap().iter().any(|p| p == "/"));

    // The site's own redirect of an unknown book to its catalogue is still a 404.
    let (status, _, _) = get(&base, "/book/999999999", None).await;
    assert_eq!(status, 404);
    assert!(site.seen.lock().unwrap().iter().any(|p| p == "/knihy"));
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_a_rate_limit_is_passed_on_and_held() {
    let site = proxy(vec![("/vyhledavani/knihy", Fault::RateLimit(42))]).await;
    let base = provider(&site.base, SECONDS_8, None).await;
    let (status, _, retry_after) = get(&base, "/search?query=Krakatit", None).await;
    assert_eq!((status, retry_after.as_deref()), (429, Some("42")));
    let asked = site.seen.lock().unwrap().len();
    // The pause holds: a lookup is refused without asking the site.
    let (status, _, retry_after) = get(&base, "/book/krakatit-688", None).await;
    assert_eq!(status, 429);
    let left: u64 = retry_after.unwrap().parse().unwrap();
    assert!((40..=42).contains(&left), "{left}");
    assert_eq!(site.seen.lock().unwrap().len(), asked);
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_fault_an_unreachable_site_is_a_bad_gateway() {
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let base = provider(&closed, SECONDS_8, None).await;
    assert_eq!(get(&base, "/search?query=Krakatit", None).await.0, 502);
    assert_eq!(get(&base, "/book/krakatit-688", None).await.0, 502);
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_auth_token_is_required_when_set() {
    let base = provider(SITE, SECONDS_8, Some("Bearer s3cret")).await;
    assert_eq!(get(&base, "/book/krakatit-688", None).await.0, 401);
    assert_eq!(
        get(&base, "/book/krakatit-688", Some("Bearer nope"))
            .await
            .0,
        401
    );
    let (status, book, _) = get(&base, "/book/krakatit-688", Some("Bearer s3cret")).await;
    assert_eq!((status, book["title"].as_str()), (200, Some("Krakatit")));
}
