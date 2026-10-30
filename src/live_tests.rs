//! Checks against the real databazeknih.cz, so a change to the site's pages shows up as a failing
//! test rather than as a user's empty search. They are ignored by `cargo test`; run them with
//! `cargo test live -- --ignored --test-threads=1`. The scheduled `Live` workflow runs them daily.
//!
//! They check facts about well-known books that should not change (title, author, series, page
//! count, ISBN), never ratings or wording, so an edited blurb or a new vote is not a failure.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::databazeknih::{Client, SITE};
use crate::server::routes;
use crate::{Config, VERSION};

/// The provider, serving from the real site with the same pacing as in production.
async fn provider() -> String {
    let cfg = Config {
        auth_token: None,
        deadline: Duration::from_secs(20),
    };
    let user_agent = format!("bookorbit-databazeknih-provider/{VERSION} (live check)");
    let client = Client::new(SITE, &user_agent, 2, Duration::from_millis(400));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = routes(Arc::new(cfg), Arc::new(client));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn get(base: &str, path: &str) -> Value {
    let res = reqwest::get(format!("{base}{path}")).await.unwrap();
    let status = res.status().as_u16();
    let body: Value = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(Value::Null);
    assert_eq!(status, 200, "{path} answered {status}: {body}");
    body
}

fn matches(body: &Value) -> &Vec<Value> {
    body["matches"].as_array().expect("a matches list")
}

fn series(book: &Value) -> (String, String) {
    let first = &book["series"][0];
    (
        first["series"].as_str().unwrap_or_default().to_string(),
        first["sequence"].as_str().unwrap_or_default().to_string(),
    )
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_print_edition() {
    let base = provider().await;
    let book = get(&base, "/book/zaklinac-zaklinac-posledni-prani-30396").await;
    assert_eq!(book["providerId"], "zaklinac-zaklinac-posledni-prani-30396");
    assert_eq!(book["mediaType"], "book");
    assert_eq!(book["title"], "Poslední přání", "title (h1)");
    assert_eq!(book["authors"][0], "Andrzej Sapkowski", "authors (JSON-LD)");
    assert_eq!(book["publisher"], "Leonardo", "publisher");
    assert_eq!(
        book["publishedYear"], "1999",
        "year (line under the genres)"
    );
    assert_eq!(book["languageCode"], "cs", "language");
    assert_eq!(book["pages"], 280, "pages (Více info fragment)");
    assert_eq!(book["isbn10"], "8085951142", "ISBN");
    assert_eq!(
        series(&book),
        ("Zaklínač".to_string(), "1".to_string()),
        "series box"
    );
    assert!(
        book["genres"]
            .as_array()
            .is_some_and(|g| g.iter().any(|g| g == "Fantasy")),
        "genres: {}",
        book["genres"]
    );
    let rating = book["rating"].as_f64().expect("a rating");
    assert!((3.0..=5.0).contains(&rating), "rating {rating}");
    assert!(
        book["ratingCount"].as_i64().unwrap_or(0) > 1000,
        "rating count"
    );

    // The whole blurb, not the shortened copy in the structured data, and none of the page around it.
    let description = book["description"].as_str().unwrap_or_default();
    assert!(
        description.contains("Hlas rozumu 7"),
        "description is cut short: {description}"
    );
    assert!(
        !description.contains("celý text"),
        "description has the reveal link"
    );
    assert!(
        !description.contains("Co říkají"),
        "description has the comment summary"
    );

    // The large cover exists.
    let cover = book["cover"].as_str().expect("a cover");
    assert!(
        cover.contains("/big_"),
        "cover is not the large one: {cover}"
    );
    let res = reqwest::get(cover).await.unwrap();
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(res.status().as_u16(), 200, "cover {cover}");
    assert!(
        content_type.starts_with("image/"),
        "cover is {content_type}"
    );
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_audiobook_edition() {
    let base = provider().await;
    let book = get(&base, "/book/posledni-prani-537448").await;
    assert_eq!(book["mediaType"], "audiobook", "form (Více info fragment)");
    assert!(
        book["narrators"].as_array().is_some_and(|n| n.len() > 10),
        "narrators: {}",
        book["narrators"]
    );
    assert_eq!(book["duration"], 11 * 3600 + 56 * 60, "running time");
    assert_eq!(series(&book).0, "Zaklínač", "series box");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_isbn_search_lands_on_the_edition() {
    let base = provider().await;
    let body = get(&base, "/search?isbn=978-80-7477-079-1").await;
    assert_eq!(matches(&body).len(), 1, "{body}");
    let book = &matches(&body)[0];
    assert_eq!(book["providerId"], "posledni-prani-497285");
    assert_eq!(book["isbn13"], "9788074770791");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_title_search_ranks_the_author_first() {
    let base = provider().await;
    let body = get(
        &base,
        "/search?query=Krakatit&author=Karel%20%C4%8Capek&limit=2",
    )
    .await;
    assert!(body.get("partial").is_none(), "partial answer: {body}");
    let first = &matches(&body)[0];
    assert_eq!(first["title"], "Krakatit");
    assert_eq!(first["authors"][0], "Karel Čapek");
    assert!(first["pages"].as_i64().unwrap_or(0) > 0, "pages: {first}");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_lookup_by_an_old_slug_answers_with_the_asked_id() {
    let base = provider().await;
    // The site redirects any slug to the current one.
    let book = get(&base, "/book/stary-nazev-30396").await;
    assert_eq!(book["providerId"], "stary-nazev-30396");
    assert_eq!(book["title"], "Poslední přání");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_title_search_finds_slovak_editions() {
    let base = provider().await;
    let body = get(
        &base,
        "/search?query=Posledn%C3%A9%20%C5%BEelanie&author=Andrzej%20Sapkowski&limit=1",
    )
    .await;
    let first = &matches(&body)[0];
    assert_eq!(first["title"], "Posledné želanie");
    assert_eq!(first["languageCode"], "sk");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_a_search_with_no_results_says_so() {
    let base = provider().await;
    // An empty answer only when the site says it found nothing; otherwise this would be a 502.
    let body = get(&base, "/search?query=qxzwvqxzwv").await;
    assert!(matches(&body).is_empty(), "{body}");
}

#[tokio::test]
#[ignore = "hits databazeknih.cz"]
async fn live_unknown_id_is_not_found() {
    let base = provider().await;
    let res = reqwest::get(format!("{base}/book/999999999"))
        .await
        .unwrap();
    assert_eq!(
        res.status().as_u16(),
        404,
        "the site no longer redirects unknown books"
    );
}
