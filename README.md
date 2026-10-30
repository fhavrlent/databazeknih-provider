# Databáze knih metadata provider

BookOrbit metadata from [Databáze knih](https://www.databazeknih.cz), the largest Czech book
catalogue and reading community. It covers Czech books and the Czech translations of foreign ones:
Czech titles, descriptions, covers, publishers, series, genres, ratings, and for audiobooks their
narrators and running time. It runs beside BookOrbit as a small container.

Rust: axum, tokio, reqwest with rustls, serde, regex and scraper, the same stack as the published
[BookOrbit providers](https://github.com/bookorbit/bookorbit-metadata-providers). The image is a
static musl binary on `scratch`, about 3.4 MB.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  databazeknih:
    image: ghcr.io/fhavrlent/databazeknih-provider:0.1.3
    restart: unless-stopped
```

Then add `http://databazeknih:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. No account or key is needed. To build it yourself:
`docker build -t databazeknih-provider .`, or with a Rust toolchain, `cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `DATABAZEKNIH_SITE` | `https://www.databazeknih.cz` | Where pages are fetched from; links in answers always point at databazeknih.cz |
| `USER_AGENT` | `bookorbit-databazeknih-provider/0.1.3` | |
| `DEADLINE_MS` | `8000` | How long a search or lookup may take. A search that runs out answers with the books it has, marked partial. Raise it if you raise the provider's timeout in BookOrbit (10 seconds by default) |

databazeknih.cz is a community site, so the provider sends it at most two requests at a time, at
least 400 ms apart, whatever its clients ask for. When the site answers `429`, the provider sends
it nothing more until its `Retry-After` has passed, and answers `429` itself meanwhile.

What it sends to databazeknih.cz: the title or the ISBN of each book BookOrbit asks about, and the
ids it looks up. The author is used only to rank the results and is not sent. It logs request paths
and failures, never titles.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `databazeknih`, books, audiobooks and comics, lookup, ISBN search, one call at a time 500 ms apart, a URL template for book links, icon and colour. No cover shape: print covers are portrait, audiobook covers square or nearly (27 of 31 measured square, 4 slightly wider, none portrait), and BookOrbit ranks a declared shape that does not fit last |
| `GET /search?query=&author=&isbn=&mediaType=&limit=` | `limit` defaults to 5, at most 5 |
| `GET /book/{id}` | An id such as `zaklinac-posledni-prani-272481`, or the bare number `272481`, or a databazeknih.cz book address. The answer carries the id that was asked for, even when the site has renamed the book's slug |

A book costs two requests: its overview page and the "Více info..." fragment that holds its page
count, ISBN, form, narrators and running time. A title search is the search page and then each
result, up to eleven requests, about four seconds. An ISBN search is three requests.

Status codes: `400` neither a query nor a valid ISBN, `401` bad token, `404` unknown id, `429` with
`Retry-After` when databazeknih.cz throttles, `502` when it fails or a page cannot be read. A search
where some book pages could not be fetched or read still answers `200` with `"partial": true`; one
where none could answers `502`, never an empty list. When the book that ranks first fails to load,
the search answers `502` (or `429`) rather than a partial answer led by a weaker match, such as a
novel of the same title by another author; only a first book that is gone from the site is skipped.

BookOrbit remembers an empty search and a `404`, so the provider gives them only when the site says
so. A search answers empty only when its page says "jsme bohužel nenašli žádný výsledek"; a results
page it cannot read is a `502`. A lookup answers `404` only when the site redirects the book to its
catalogue (`/knihy`), which it does for unknown books; any other page, such as a maintenance page,
is a `502`.

## Mapping

- **ISBN search** comes first when BookOrbit sends an ISBN. The site's search sends an ISBN it
  knows straight to that edition's page. That page is the answer only when the edition lists the
  ISBN (an ISBN-10 and its ISBN-13 count as the same): the site's ISBN search is not exact, and
  sent `9780000000002` to a book recorded as `000-000-000`. Placeholder ISBNs of zeros, such as
  `0000000000`, which badly tagged files carry and under which the site stores some books, are not
  searched at all. About one edition in twenty lists more than one ISBN, often the print and e-book ones, comma-separated in its details (its page's structured data has only the first); the one searched is reported, and without an ISBN search the first. When
  the site does not know the ISBN, the title is searched instead, or, without a title, the answer
  is empty.
- **Title search** asks for the title alone: the site matches titles, and an author in the phrase
  finds fewer of the right books. Results stay in the site's order, except that books by the
  author BookOrbit asked for (by surname, ignoring diacritics and name order) come first, and among
  them an exact title before a partial one. "Zaklínač" by Sapkowski otherwise trails a pulp novel
  of the same name. It searches Czech books first, exactly as the site's default search does,
  and keeps those results whenever one has the title asked for. Only otherwise does it also
  search Slovak books, and it uses them when they match more closely: "Posledné želanie" is found
  only among Slovak books, while among Czech ones it brings up Sapkowski's Czech "Poslední přání".
  Czech and Slovak are never searched together, because a Slovak edition under the same title,
  such as Čapek's "Krakatit", would then tie with the Czech one.
- **Ids** are the slug and number of the book's address, such as `zaklinac-posledni-prani-272481`,
  taken from the page's canonical address. Every edition, including audiobooks, has its own.
  Lookups use the number; the site redirects any slug to the current one, and the answer keeps
  the id that was asked for.
- **Title, authors, series and genres** are in Czech as the site has them. The series box ("Zaklínač
  série, 1. díl") gives the series and its position.
- **Media type** is `audiobook` when the edition's form is "audiokniha", `comic` when a genre is
  "Komiksy", else `book`.
- **Language** is the page's `inLanguage`, else "Jazyk vydání" mapped to a code (`český` is `cs`,
  also `slovenský`, `anglický`, `německý`, `polský`, `francouzský`, `španělský`, `italský`,
  `ruský`, `maďarský`). Other languages are left out.
- **ISBN** is the edition's, without hyphens, as `isbn13` or `isbn10` by length.
- **Year** is the edition's year from the line under the genres.
- **Rating** is the site's 5-star average, with the vote count. Editions share their work's rating.
- **Description** is the whole blurb, including the part the page hides behind "... celý text",
  as plain text with the site's line breaks. The site's AI summary of reader comments is left out.
- **Cover** is the largest the site has: the `big_` file its cover page shows.
- **Audiobooks** add narrators ("Interpreti") and the running time ("Délka") in seconds.

Known gaps:

- No search by original title: "The Last Wish" does not find "Poslední přání". Search with the
  Czech title, or the ISBN.
- The original title and translators are on the page but BookOrbit has no field for them.
- The site's search mostly lists print editions, so a title search for an audiobook usually
  answers with the book. An audiobook edition is found by its ISBN when it has one, or by its id.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The tests run offline against a stand-in for the site; nothing in them contacts databazeknih.cz.
The fixtures in `testdata/` are synthetic pages in the site's markup, with invented books, people
and texts: a search results page, a print edition and an audiobook edition with their details
fragments, an edition listing three ISBNs, and an empty results page. They keep what the provider
depends on: the classes, the structured data and its escaping, the line breaks of the blurb and
its hidden "... celý text" part, the series box, the details rows, and the site's summary of reader
comments next to the description, which the tests check stays out of it.

The stand-in also plays the site's failures: a redesigned results page, a page that no longer says
it found nothing, a slow or failing book page, a redirect to a maintenance page, a `429`, and an
unreachable site. The `Publish` workflow runs every test before it builds an image, so a release
that fails them is never published.

Before a release, check it with BookOrbit's conformance checker:

```bash
git clone https://github.com/bookorbit/bookorbit-metadata-providers.git
```

```bash
cd bookorbit-metadata-providers/conformance && go run . -url http://localhost:8080 -query "Krakatit"
```
