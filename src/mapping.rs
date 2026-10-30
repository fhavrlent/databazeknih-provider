use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use scraper::{CaseSensitivity, ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::databazeknih::{extract_id, extract_path, resolve_url};

/// The spec's schema. Fields after the blank line are proposed extensions that a client which
/// does not know them ignores.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: String,
    pub provider_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub narrators: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn13: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn10: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Series>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pages: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration: i64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub series: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sequence: String,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// One result on a search page, with the year and authors its caption gives.
#[derive(Debug, PartialEq)]
pub struct SearchHit {
    pub provider_id: String,
    pub url: String,
    pub title: String,
    pub authors: Vec<String>,
}

/// What a book's overview page says, before it is shaped into an answer.
#[derive(Debug, Default)]
pub struct BookPage {
    pub provider_id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub description: String,
    pub publisher: String,
    pub published_year: Option<i64>,
    pub language: String,
    pub isbn: String,
    pub series_name: String,
    pub series_index: String,
    pub genres: Vec<String>,
    pub cover_url: String,
    /// On a 0-5 scale.
    pub rating: Option<f64>,
    pub rating_count: Option<i64>,
}

/// What the "Více info..." fragment says about the edition.
#[derive(Debug, Default, PartialEq)]
pub struct MoreInfo {
    pub page_count: Option<i64>,
    pub isbn: String,
    pub language: String,
    /// "klasická kniha", "e-kniha", "audiokniha".
    pub form: String,
    pub narrators: Vec<String>,
    /// Seconds.
    pub duration: Option<i64>,
}

const MAX_GENRES: usize = 10;
const MIN_PUBLISHED_YEAR: i64 = 1000;
const MAX_PUBLISHED_YEAR: i64 = 2200;
const NOTHING_FOUND: &str = "jsme bohužel nenašli žádný výsledek";

/// Languages as the site names them in "Jazyk vydání".
const LANGUAGES: &[(&str, &str)] = &[
    ("český", "cs"),
    ("slovenský", "sk"),
    ("anglický", "en"),
    ("německý", "de"),
    ("polský", "pl"),
    ("francouzský", "fr"),
    ("španělský", "es"),
    ("italský", "it"),
    ("ruský", "ru"),
    ("maďarský", "hu"),
];

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("valid selector")
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

static RESULT_LINK: LazyLock<Selector> = LazyLock::new(|| selector(r#"a.new[type="book"]"#));
static TITLE: LazyLock<Selector> = LazyLock::new(|| selector("#bookDetail h1, h1.oddown_zero"));
static AUTHOR_LINK: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"span.author a[href*="/autori/"]"#));
static DESCRIPTION: LazyLock<Selector> = LazyLock::new(|| selector("#left p.new2.odtop"));
static PUBLISHER: LazyLock<Selector> = LazyLock::new(|| selector(r#"a[href*="/nakladatelstvi/"]"#));
static GENRES_AND_YEAR: LazyLock<Selector> =
    LazyLock::new(|| selector("#bookDetail .lora.lineHeightMid"));
static SERIES_LINK: LazyLock<Selector> =
    LazyLock::new(|| selector(r#".book_detail_serie_info a[href*="/serie/"]"#));
static SERIES_PART: LazyLock<Selector> =
    LazyLock::new(|| selector(".book_detail_serie_info span.odright_pet"));
static COVER: LazyLock<Selector> = LazyLock::new(|| selector("img.coverOnDetail"));
static OG_IMAGE: LazyLock<Selector> = LazyLock::new(|| selector(r#"meta[property="og:image"]"#));
static JSON_LD: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"script[type="application/ld+json"]"#));
static DT: LazyLock<Selector> = LazyLock::new(|| selector("dt"));
static LINK: LazyLock<Selector> = LazyLock::new(|| selector("a"));

static YEAR: LazyLock<Regex> = LazyLock::new(|| regex(r"(?:^|[^0-9])([0-9]{4})(?:[^0-9]|$)"));
static PART: LazyLock<Regex> = LazyLock::new(|| regex(r"^([0-9]+(?:[.,][0-9]+)?)\.?\s*díl"));
static FIRST_NUMBER: LazyLock<Regex> = LazyLock::new(|| regex(r"[0-9]+"));
static HOURS: LazyLock<Regex> = LazyLock::new(|| regex(r"([0-9]+)\s*hod"));
static MINUTES: LazyLock<Regex> = LazyLock::new(|| regex(r"([0-9]+)\s*min"));
static SECONDS: LazyLock<Regex> = LazyLock::new(|| regex(r"([0-9]+)\s*sek"));
static PSEUDONYM: LazyLock<Regex> = LazyLock::new(|| regex(r"\s*\([a-z]+\)$"));

/// Book results from a search page in the site's order, one per book.
pub fn search_results(html: &str) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for link in doc.select(&RESULT_LINK) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let (Some(id), Some(provider_id), Some(url)) =
            (extract_id(href), extract_path(href), resolve_url(href))
        else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        results.push(SearchHit {
            provider_id,
            url,
            title: element_text(link),
            authors: caption_authors(link),
        });
    }
    results
}

/// Whether a search page says it found nothing: "Na hledaný výraz ... jsme bohužel nenašli žádný
/// výsledek v kategorii Knihy." A page with no results that does not say so is a page the provider
/// no longer understands, not an empty answer.
pub fn says_nothing_found(html: &str) -> bool {
    html.contains(NOTHING_FOUND)
}

/// The caption after a result's title: "1998, Jason Dark (p)", the year then the authors, with
/// "(p)" marking a pen name.
fn caption_authors(link: ElementRef) -> Vec<String> {
    let caption = link
        .next_siblings()
        .filter_map(ElementRef::wrap)
        .take_while(|el| el.value().attr("type") != Some("book"))
        .find(|el| {
            el.value().name() == "span"
                && el.value().has_class("pozn", CaseSensitivity::CaseSensitive)
        })
        .map(element_text)
        .unwrap_or_default();
    caption
        .split(',')
        .map(|part| PSEUDONYM.replace(part.trim(), "").to_string())
        .filter(|part| !part.is_empty() && !part.bytes().all(|b| b.is_ascii_digit()))
        .collect()
}

/// The results best first: the site's own order, except that results by the author BookOrbit
/// asked for come before results by others, and among those an exact title before a partial one.
/// The site's search matches titles only, so "Zaklínač" by Sapkowski can trail a pulp novel that
/// shares the word.
pub fn rank(hits: Vec<SearchHit>, title: &str, author: &str) -> Vec<SearchHit> {
    let wanted = Wanted::new(title, author);
    let mut scored: Vec<(u8, usize, SearchHit)> = hits
        .into_iter()
        .enumerate()
        .map(|(index, hit)| (wanted.score(&hit), index, hit))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, hit)| hit).collect()
}

/// How closely the best of a search's results matches what was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Closeness {
    /// Only books that share a word with the title.
    None,
    /// A book by the author asked for, under another title.
    Author,
    /// A book with exactly the title asked for.
    Title,
}

pub fn closeness(hits: &[SearchHit], title: &str, author: &str) -> Closeness {
    let wanted = Wanted::new(title, author);
    hits.iter()
        .map(|hit| match wanted.score(hit) {
            0 => Closeness::None,
            2 => Closeness::Author,
            _ => Closeness::Title,
        })
        .max()
        .unwrap_or(Closeness::None)
}

struct Wanted {
    title: String,
    author: HashSet<String>,
}

impl Wanted {
    fn new(title: &str, author: &str) -> Self {
        Self {
            title: fold(title),
            author: fold(author)
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| word.chars().count() > 1)
                .map(str::to_string)
                .collect(),
        }
    }

    /// 2 for the author asked for, by surname, plus 1 for exactly the title asked for.
    fn score(&self, hit: &SearchHit) -> u8 {
        let by_author = !self.author.is_empty()
            && hit.authors.iter().any(|name| {
                fold(name)
                    .split(|c: char| !c.is_alphanumeric())
                    .rfind(|word| !word.is_empty())
                    .is_some_and(|surname| self.author.contains(surname))
            });
        let same_title = fold(&hit.title) == self.title;
        u8::from(by_author) * 2 + u8::from(same_title)
    }
}

pub fn parse_book_page(html: &str, source_url: Option<&str>) -> BookPage {
    let doc = Html::parse_document(html);
    let ld = parse_json_ld(&doc);
    let (series_name, series_index) = extract_series(&doc);
    let title = first_text(&doc, &TITLE);

    BookPage {
        provider_id: ld
            .url
            .as_deref()
            .and_then(extract_path)
            .or_else(|| source_url.and_then(extract_path))
            .unwrap_or_default(),
        title: if title.is_empty() {
            ld.name.clone()
        } else {
            title
        },
        authors: if ld.authors.is_empty() {
            unique_texts(doc.select(&AUTHOR_LINK).map(element_text))
        } else {
            ld.authors.clone()
        },
        description: extract_description(&doc, &ld.description),
        publisher: ld
            .publisher
            .clone()
            .unwrap_or_else(|| first_text(&doc, &PUBLISHER)),
        published_year: extract_year(&doc),
        language: ld.language.clone().unwrap_or_default(),
        isbn: ld.isbn.clone(),
        series_name,
        series_index,
        genres: ld.genres.iter().take(MAX_GENRES).cloned().collect(),
        cover_url: extract_cover(&doc, ld.image.as_deref()),
        rating: ld.rating,
        rating_count: ld.rating_count,
    }
}

/// The "Více info..." fragment: `dt` labels, each followed by its `dd`.
pub fn parse_more_info(html: &str) -> MoreInfo {
    let doc = Html::parse_fragment(html);
    let mut info = MoreInfo::default();
    for dt in doc.select(&DT) {
        let Some(dd) = dt
            .next_siblings()
            .find_map(ElementRef::wrap)
            .filter(|next| next.value().name() == "dd")
        else {
            continue;
        };
        let value = element_text(dd);
        match element_text(dt).as_str() {
            "Počet stran" => {
                info.page_count = FIRST_NUMBER
                    .find(&value)
                    .and_then(|n| n.as_str().parse().ok())
                    .filter(|pages| *pages > 0);
            }
            "ISBN" => info.isbn = value,
            "Jazyk vydání" => info.language = map_language(&value).unwrap_or_default(),
            "Forma" => info.form = value.to_lowercase(),
            "Interpreti" | "Interpret" => {
                info.narrators = unique_texts(dd.select(&LINK).map(element_text));
            }
            "Délka" => info.duration = parse_duration(&value),
            _ => {}
        }
    }
    info
}

/// The answer for a book, or nothing when its page has no title or no id to file it under. When the
/// book was found by an ISBN, that ISBN is the one reported if the edition lists it.
pub fn map_book(
    page: &BookPage,
    more: &MoreInfo,
    url: &str,
    fallback_id: &str,
    searched_isbn: Option<&str>,
) -> Option<BookMetadata> {
    let provider_id = if page.provider_id.is_empty() {
        extract_path(url).unwrap_or_else(|| fallback_id.to_string())
    } else {
        page.provider_id.clone()
    };
    if page.title.is_empty() || provider_id.is_empty() {
        return None;
    }
    let (isbn10, isbn13) = split_isbn(
        if more.isbn.is_empty() {
            &page.isbn
        } else {
            &more.isbn
        },
        searched_isbn,
    );
    let audiobook = more.form.contains("audio");
    let comic = page
        .genres
        .iter()
        .any(|genre| fold(genre).starts_with("komiks"));
    let mut book = BookMetadata {
        media_type: if audiobook {
            "audiobook"
        } else if comic {
            "comic"
        } else {
            "book"
        }
        .into(),
        provider_id,
        title: page.title.clone(),
        authors: page.authors.clone(),
        narrators: more.narrators.clone(),
        description: page.description.clone(),
        publisher: page.publisher.clone(),
        published_year: page
            .published_year
            .map(|year| year.to_string())
            .unwrap_or_default(),
        language_code: if page.language.is_empty() {
            more.language.clone()
        } else {
            page.language.clone()
        },
        pages: if audiobook {
            0
        } else {
            more.page_count.unwrap_or(0)
        },
        duration: more.duration.unwrap_or(0),
        isbn13,
        isbn10,
        genres: page.genres.clone(),
        cover: page.cover_url.clone(),
        url: url.to_string(),
        ..BookMetadata::default()
    };
    if !page.series_name.is_empty() {
        book.series.push(Series {
            series: page.series_name.clone(),
            sequence: page.series_index.clone(),
        });
    }
    if page.rating.is_some() {
        book.rating = page.rating;
        book.rating_count = page.rating_count;
    }
    Some(book)
}

fn element_text(el: ElementRef) -> String {
    clean_text(&el.text().collect::<String>())
}

fn first_text(doc: &Html, selector: &Selector) -> String {
    doc.select(selector)
        .next()
        .map(element_text)
        .unwrap_or_default()
}

fn first_attr(doc: &Html, selector: &Selector, attr: &str) -> String {
    doc.select(selector)
        .next()
        .and_then(|el| el.value().attr(attr))
        .unwrap_or_default()
        .to_string()
}

fn unique_texts(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

/// The blurb under "O knize". The page shows its start and hides the rest in a span that the
/// "... celý text" link reveals; both are in the HTML. The structured data carries a shortened
/// copy, used only when the page has none.
fn extract_description(doc: &Html, fallback: &str) -> String {
    let blurb = doc
        .select(&DESCRIPTION)
        .next()
        .map(|el| {
            let mut raw = String::new();
            block_text(el, &mut raw);
            paragraphs(&raw)
        })
        .unwrap_or_default();
    if blurb.is_empty() {
        paragraphs(fallback)
    } else {
        blurb
    }
}

/// Text with line breaks where the source or the markup breaks lines. The site writes its blurbs
/// as plain text with line breaks, not paragraphs.
fn block_text(el: ElementRef, out: &mut String) {
    for child in el.children() {
        if let Some(text) = child.value().as_text() {
            out.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        } else if let Some(element) = ElementRef::wrap(child) {
            match element.value().name() {
                "br" => out.push('\n'),
                "script" | "style" => {}
                "a" if element
                    .value()
                    .has_class("show_hide_more", CaseSensitivity::CaseSensitive) => {}
                "p" | "div" | "li" | "ul" | "ol" | "blockquote" => {
                    out.push('\n');
                    block_text(element, out);
                    out.push('\n');
                }
                _ => block_text(element, out),
            }
        }
    }
}

/// Lines with their spacing collapsed; a run of empty lines becomes one paragraph break.
fn paragraphs(raw: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in raw.replace("\r\n", "\n").split('\n') {
        let line = clean_text(line);
        if line.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        out.push_str(&line);
        blank = false;
    }
    out
}

/// The year of the edition: the bare number between the genres and the publisher, "Fantasy <br>
/// 1999, Leonardo". Only the line's own text is read, so a genre such as "19. století" is not
/// taken for it.
fn extract_year(doc: &Html) -> Option<i64> {
    let line = doc.select(&GENRES_AND_YEAR).next()?;
    line.children()
        .filter_map(|child| child.value().as_text().map(|text| text.to_string()))
        .find_map(|text| {
            YEAR.captures(&text)
                .and_then(|captures| captures[1].parse::<i64>().ok())
        })
        .filter(|year| (MIN_PUBLISHED_YEAR..=MAX_PUBLISHED_YEAR).contains(year))
}

/// "Zaklínač série, 1. díl".
fn extract_series(doc: &Html) -> (String, String) {
    let name = doc
        .select(&SERIES_LINK)
        .next()
        .map(|el| {
            let text = el.value().attr("title").map(clean_text).unwrap_or_default();
            if text.is_empty() {
                element_text(el)
            } else {
                text
            }
        })
        .unwrap_or_default();
    if name.is_empty() {
        return (String::new(), String::new());
    }
    let index = doc
        .select(&SERIES_PART)
        .map(element_text)
        .find_map(|text| PART.captures(&text).map(|c| c[1].replace(',', ".")))
        .unwrap_or_default();
    (name, index)
}

/// The largest cover. The page shows the medium one (`bmid_...`); its cover page links the large
/// one, the same file with the prefix `big_`. Pages without a cover show a placeholder outside
/// `/img/books/`, which is not a cover.
fn extract_cover(doc: &Html, ld_image: Option<&str>) -> String {
    let shown = first_attr(doc, &COVER, "src");
    let candidates = [
        shown.as_str(),
        ld_image.unwrap_or_default(),
        &first_attr(doc, &OG_IMAGE, "content"),
    ]
    .map(str::to_string);
    candidates
        .iter()
        .filter_map(|src| resolve_url(src.trim()))
        .find(|src| src.contains("/img/books/"))
        .map(|src| large_cover(&src))
        .unwrap_or_default()
}

fn large_cover(src: &str) -> String {
    let Some(slash) = src.rfind('/') else {
        return src.to_string();
    };
    let (dir, file) = src.split_at(slash + 1);
    let file = file
        .strip_prefix("bmid_")
        .or_else(|| file.strip_prefix("big_"))
        .unwrap_or(file);
    format!("{dir}big_{file}")
}

/// "11 hodin 56 minut", "1 hodina", "45 minut" as seconds.
fn parse_duration(raw: &str) -> Option<i64> {
    let value = |re: &Regex| {
        re.captures(raw)
            .and_then(|c| c[1].parse::<i64>().ok())
            .unwrap_or(0)
    };
    let seconds = value(&HOURS) * 3600 + value(&MINUTES) * 60 + value(&SECONDS);
    (seconds > 0).then_some(seconds)
}

/// A language the site names in Czech, as a code. Others are left out rather than passed on as
/// Czech words.
fn map_language(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_lowercase();
    LANGUAGES
        .iter()
        .find(|(name, _)| *name == normalized)
        .map(|(_, code)| code.to_string())
}

/// The first ISBN in a field, without hyphens, as (`isbn10`, `isbn13`).
fn split_isbn(raw: &str, searched: Option<&str>) -> (String, String) {
    let isbns: Vec<String> = raw
        .split([',', ';', '/'])
        .map(|part| {
            part.chars()
                .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
                .collect::<String>()
                .to_uppercase()
        })
        .filter(|isbn| matches!(isbn.len(), 10 | 13))
        .collect();
    let chosen = searched
        .and_then(|wanted| isbns.iter().find(|isbn| same_isbn(isbn, wanted)))
        .or(isbns.first());
    match chosen {
        Some(isbn) if isbn.len() == 13 => (String::new(), isbn.clone()),
        Some(isbn) => (isbn.clone(), String::new()),
        None => (String::new(), String::new()),
    }
}

/// Whether two ISBNs, without hyphens, name the same book: an ISBN-10 and the ISBN-13 made from
/// it (`978`, its first nine digits, a new check digit) are the same number.
pub fn same_isbn(a: &str, b: &str) -> bool {
    match (as_isbn13(a), as_isbn13(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn as_isbn13(isbn: &str) -> Option<String> {
    let isbn = isbn.to_uppercase();
    match isbn.len() {
        13 if isbn.bytes().all(|b| b.is_ascii_digit()) => Some(isbn),
        10 if isbn[..9].bytes().all(|b| b.is_ascii_digit()) => {
            let body = format!("978{}", &isbn[..9]);
            let sum: u32 = body
                .bytes()
                .enumerate()
                .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 0 { 1 } else { 3 })
                .sum();
            Some(format!("{body}{}", (10 - sum % 10) % 10))
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
struct JsonLd {
    name: String,
    url: Option<String>,
    isbn: String,
    authors: Vec<String>,
    image: Option<String>,
    description: String,
    publisher: Option<String>,
    language: Option<String>,
    genres: Vec<String>,
    rating: Option<f64>,
    rating_count: Option<i64>,
}

/// The page's schema.org `Book`.
fn parse_json_ld(doc: &Html) -> JsonLd {
    let Some(book) = doc
        .select(&JSON_LD)
        .filter_map(|script| serde_json::from_str::<Value>(&script.text().collect::<String>()).ok())
        .find(|value| value["@type"] == "Book")
    else {
        return JsonLd::default();
    };
    let text = |value: &Value| value.as_str().map(clean_text).filter(|s| !s.is_empty());
    let names = |value: &Value| -> Vec<String> {
        let items = match value {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        unique_texts(
            items
                .into_iter()
                .filter_map(|item| text(&item["name"]).or_else(|| text(item))),
        )
    };
    let number = |value: &Value| {
        value.as_f64().or_else(|| {
            value
                .as_str()
                .and_then(|s| s.replace(',', ".").parse().ok())
        })
    };

    let aggregate = &book["aggregateRating"];
    let rating_count = number(&aggregate["ratingCount"])
        .map(|count| count as i64)
        .filter(|count| *count > 0);
    let best = number(&aggregate["bestRating"])
        .filter(|best| *best > 0.0)
        .unwrap_or(5.0);
    let rating = rating_count
        .and(number(&aggregate["ratingValue"]))
        .map(|value| (value / best * 5.0 * 100.0).round() / 100.0)
        .filter(|value| (0.0..=5.0).contains(value));

    JsonLd {
        name: text(&book["name"]).unwrap_or_default(),
        url: text(&book["url"]),
        isbn: text(&book["isbn"]).unwrap_or_default(),
        authors: names(&book["author"]),
        image: text(&book["image"]),
        description: book["description"].as_str().unwrap_or_default().to_string(),
        publisher: names(&book["publisher"]).into_iter().next(),
        language: text(&book["inLanguage"]).map(|code| code.to_lowercase()),
        genres: names(&book["genre"]),
        rating,
        rating_count: rating.and(rating_count),
    }
}

fn clean_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lowercase without diacritics, for comparing names and titles: "Zaklínač" is "zaklinac".
pub fn fold(value: &str) -> String {
    clean_text(value)
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ą' | 'ã' | 'å' => 'a',
            'č' | 'ć' | 'ç' => 'c',
            'ď' => 'd',
            'é' | 'ě' | 'è' | 'ê' | 'ë' | 'ę' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ľ' | 'ĺ' | 'ł' => 'l',
            'ň' | 'ń' | 'ñ' => 'n',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ő' => 'o',
            'ř' | 'ŕ' => 'r',
            'š' | 'ś' => 's',
            'ť' => 't',
            'ú' | 'ů' | 'ù' | 'û' | 'ü' | 'ű' => 'u',
            'ý' | 'ÿ' => 'y',
            'ž' | 'ź' | 'ż' => 'z',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK: &str = include_str!("../testdata/book-30396.html");
    const MORE: &str = include_str!("../testdata/more-30396.html");
    const AUDIO: &str = include_str!("../testdata/book-537448.html");
    const AUDIO_MORE: &str = include_str!("../testdata/more-537448.html");
    const SEARCH: &str = include_str!("../testdata/search-zaklinac.html");

    const BOOK_URL: &str =
        "https://www.databazeknih.cz/prehled-knihy/zaklinac-zaklinac-posledni-prani-30396";

    #[test]
    fn real_search_page_lists_books_in_order_with_their_captions() {
        let hits = search_results(SEARCH);
        assert!(hits.len() >= 20, "{}", hits.len());
        assert_eq!(
            hits[0],
            SearchHit {
                provider_id: "john-sinclair-na-stope-hruzy-zaklinac-40113".into(),
                url: "https://www.databazeknih.cz/prehled-knihy/john-sinclair-na-stope-hruzy-zaklinac-40113".into(),
                title: "Zaklínač".into(),
                authors: vec!["Jason Dark".into()],
            }
        );
        let ids: HashSet<&str> = hits.iter().map(|h| h.provider_id.as_str()).collect();
        assert_eq!(ids.len(), hits.len());
    }

    #[test]
    fn ranking_puts_the_asked_author_first_and_keeps_the_site_order_otherwise() {
        let hits = search_results(SEARCH);
        let site_order: Vec<String> = hits.iter().map(|h| h.provider_id.clone()).collect();
        let ranked = rank(hits, "Zaklínač", "Andrzej Sapkowski");
        assert!(
            ranked[0].authors.iter().any(|a| a == "Andrzej Sapkowski"),
            "{:?}",
            ranked[0]
        );
        let unranked: Vec<String> = rank(search_results(SEARCH), "", "")
            .into_iter()
            .map(|h| h.provider_id)
            .collect();
        assert_eq!(unranked, site_order);
    }

    #[test]
    fn ranking_matches_surnames_without_diacritics_and_in_either_order() {
        let hit = |id: &str, title: &str, author: &str| SearchHit {
            provider_id: id.into(),
            url: String::new(),
            title: title.into(),
            authors: vec![author.into()],
        };
        let hits = vec![
            hit("a-1", "Krysař", "Viktor Dyk"),
            hit("b-2", "Krysař a jiné", "Karel Čapek"),
            hit("c-3", "Krysař", "Karel Čapek"),
        ];
        let ids: Vec<String> = rank(hits, "krysar", "Capek, Karel")
            .into_iter()
            .map(|h| h.provider_id)
            .collect();
        assert_eq!(ids, ["c-3", "b-2", "a-1"]);
    }

    #[test]
    fn real_book_page() {
        let page = parse_book_page(BOOK, Some(BOOK_URL));
        let more = parse_more_info(MORE);
        let book = map_book(&page, &more, BOOK_URL, "", None).unwrap();
        assert_eq!(book.provider_id, "zaklinac-zaklinac-posledni-prani-30396");
        assert_eq!(book.media_type, "book");
        assert_eq!(book.title, "Poslední přání");
        assert_eq!(book.authors, ["Andrzej Sapkowski"]);
        assert_eq!(book.publisher, "Leonardo");
        assert_eq!(book.published_year, "1999");
        assert_eq!(book.language_code, "cs");
        assert_eq!(book.pages, 280);
        assert_eq!(
            (book.isbn10.as_str(), book.isbn13.as_str()),
            ("8085951142", "")
        );
        assert_eq!(book.genres, ["Literatura světová", "Povídky", "Fantasy"]);
        assert_eq!(
            book.series,
            [Series {
                series: "Zaklínač".into(),
                sequence: "1".into()
            }]
        );
        assert_eq!((book.rating, book.rating_count), (Some(4.6), Some(7748)));
        assert_eq!(
            book.cover,
            "https://www.databazeknih.cz/img/books/30_/30396/big_posledni-prani-APM-30396.gif?v=1595550371"
        );
        assert_eq!(book.url, BOOK_URL);
        assert!(book.narrators.is_empty());
        assert_eq!(book.duration, 0);
    }

    #[test]
    fn real_description_is_whole_with_its_line_breaks() {
        let page = parse_book_page(BOOK, None);
        let d = &page.description;
        assert!(d.starts_with("Nově přeložené geraltovské povídky"), "{d}");
        assert!(
            d.contains("jak to všechno začalo.\n\nZaklínač, ošlehaný muž"),
            "{d}"
        );
        // The hidden rest of the text is there, the link that reveals it is not.
        assert!(
            d.contains("Povídky:\nCesta, z níž není návratu\nHlas rozumu 1"),
            "{d}"
        );
        assert!(d.ends_with("Hlas rozumu 7"), "{d}");
        assert!(!d.contains("celý text"), "{d}");
        assert!(!d.contains("Co říkají"), "{d}");
    }

    #[test]
    fn real_audiobook_edition() {
        let url = "https://www.databazeknih.cz/prehled-knihy/posledni-prani-537448";
        let page = parse_book_page(AUDIO, Some(url));
        let more = parse_more_info(AUDIO_MORE);
        let book = map_book(&page, &more, url, "", None).unwrap();
        assert_eq!(book.provider_id, "posledni-prani-537448");
        assert_eq!(book.media_type, "audiobook");
        assert_eq!(book.duration, 11 * 3600 + 56 * 60);
        assert_eq!(book.narrators.len(), 33);
        assert_eq!(book.narrators[0], "Petra Horváthová");
        assert_eq!(book.pages, 0);
        assert_eq!(book.series[0].series, "Zaklínač");
    }

    #[test]
    fn more_info_fields() {
        let more = parse_more_info(MORE);
        assert_eq!(
            more,
            MoreInfo {
                page_count: Some(280),
                isbn: "80-85951-14-2".into(),
                language: "cs".into(),
                form: "klasická kniha".into(),
                narrators: vec![],
                duration: None,
            }
        );
        assert_eq!(parse_more_info(""), MoreInfo::default());
    }

    #[test]
    fn durations() {
        for (raw, want) in [
            ("11 hodin 56 minut", Some(42960)),
            ("1 hodina", Some(3600)),
            ("2 hodiny 1 minuta", Some(7260)),
            ("45 minut", Some(2700)),
            ("neznámá", None),
        ] {
            assert_eq!(parse_duration(raw), want, "{raw}");
        }
    }

    #[test]
    fn isbns() {
        assert_eq!(
            split_isbn("978-80-7477-079-1", None),
            (String::new(), "9788074770791".into())
        );
        assert_eq!(
            split_isbn("80-85951-14-2", None),
            ("8085951142".into(), String::new())
        );
        assert_eq!(
            split_isbn("80-123, 978-80-7477-079-1", None),
            (String::new(), "9788074770791".into())
        );
        assert_eq!(split_isbn("123", None), (String::new(), String::new()));
    }

    #[test]
    fn a_real_edition_with_three_isbns() {
        let more = parse_more_info(include_str!("../testdata/more-470637.html"));
        assert_eq!(
            more.isbn,
            "978-80-7335-323-0, 978-80-7335-520-3, 978-80-87440-58-2"
        );
        assert_eq!(
            split_isbn(&more.isbn, Some("9788087440582")),
            (String::new(), "9788087440582".into())
        );
        assert_eq!(
            split_isbn(&more.isbn, None),
            (String::new(), "9788073353230".into())
        );
    }

    #[test]
    fn an_edition_with_two_isbns_reports_the_searched_one() {
        let both = "978-80-7477-079-1, 80-85951-14-2";
        assert_eq!(
            split_isbn(both, Some("8085951142")),
            ("8085951142".into(), String::new())
        );
        assert_eq!(
            split_isbn(both, Some("9788074770791")),
            (String::new(), "9788074770791".into())
        );
        // An ISBN the edition does not list leaves its first one.
        assert_eq!(
            split_isbn(both, Some("9780000000002")),
            (String::new(), "9788074770791".into())
        );
    }

    #[test]
    fn isbn_10_and_13_name_the_same_book() {
        // Pairs printed on the books themselves.
        assert!(same_isbn("0261102214", "9780261102217"));
        assert!(same_isbn("9780261102217", "0261102214"));
        assert!(same_isbn("8085951142", "9788085951141"));
        assert!(same_isbn("080442957X", "9780804429573"));
        assert!(same_isbn("080442957x", "9780804429573"));
        assert!(!same_isbn("8085951142", "9788074770791"));
        assert!(!same_isbn("9780000000002", "000000000"));
        assert!(!same_isbn("", ""));
        // The edition's ISBN-13 is reported when the search used its ISBN-10.
        assert_eq!(
            split_isbn("80-85951-14-2, 978-80-7477-079-1", Some("9788074770791")),
            (String::new(), "9788074770791".into())
        );
    }

    #[test]
    fn empty_search_pages_say_so() {
        assert!(says_nothing_found(include_str!(
            "../testdata/search-nothing.html"
        )));
        assert!(!says_nothing_found(SEARCH));
        assert!(!says_nothing_found("<html><body></body></html>"));
    }

    #[test]
    fn large_covers() {
        assert_eq!(
            large_cover("https://x/img/books/30_/30396/bmid_a-30396.gif?v=1"),
            "https://x/img/books/30_/30396/big_a-30396.gif?v=1"
        );
        assert_eq!(
            large_cover("https://x/img/books/53_/537448/a.jpg"),
            "https://x/img/books/53_/537448/big_a.jpg"
        );
        assert_eq!(
            large_cover("https://x/img/books/1/big_a.jpg"),
            "https://x/img/books/1/big_a.jpg"
        );
    }

    #[test]
    fn a_page_without_a_cover_has_none() {
        let html = r#"<div id="bookDetail"><img class="kniha_img coverOnDetail" src="/img/content/nfoto.png"><h1>X</h1></div>"#;
        assert_eq!(parse_book_page(html, None).cover_url, "");
    }

    #[test]
    fn a_page_without_a_title_is_no_book() {
        let page = parse_book_page("<html></html>", Some(BOOK_URL));
        assert!(map_book(&page, &MoreInfo::default(), BOOK_URL, "", None).is_none());
    }

    #[test]
    fn comics_are_told_by_their_genre() {
        let html = r#"<script type="application/ld+json">{"@type":"Book","name":"Maus","genre":["Komiksy"],"url":"https://www.databazeknih.cz/prehled-knihy/maus-1"}</script>"#;
        let page = parse_book_page(html, None);
        let book = map_book(&page, &MoreInfo::default(), "u", "", None).unwrap();
        assert_eq!(
            (book.media_type.as_str(), book.title.as_str()),
            ("comic", "Maus")
        );
        assert_eq!(book.provider_id, "maus-1");
    }

    #[test]
    fn languages() {
        assert_eq!(map_language(" Český ").as_deref(), Some("cs"));
        assert_eq!(map_language("slovenský").as_deref(), Some("sk"));
        assert_eq!(map_language("esperanto"), None);
    }

    #[test]
    fn folding() {
        assert_eq!(fold("  Žluťoučký   KŮŇ úpěl "), "zlutoucky kun upel");
    }
}
