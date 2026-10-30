mod databazeknih;
mod mapping;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.3";

pub struct Config {
    pub auth_token: Option<String>,
    /// How long a search or lookup may take. A search that runs out of time answers with the books
    /// it has, marked partial, instead of letting BookOrbit's own timeout discard them all.
    pub deadline: Duration,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let site = env("DATABAZEKNIH_SITE", databazeknih::SITE);
    let user_agent = env(
        "USER_AGENT",
        &format!("bookorbit-databazeknih-provider/{VERSION}"),
    );
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
        deadline: Duration::from_millis(
            std::env::var("DEADLINE_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(8000),
        ),
    };
    // databazeknih.cz is a community site: two requests in flight and 400 ms between starts,
    // whatever the clients ask for. A book costs two requests (its page and its details), so a
    // search of five books is eleven requests and takes about four seconds.
    let client = databazeknih::Client::new(&site, &user_agent, 2, Duration::from_millis(400));
    let app = server::routes(Arc::new(cfg), Arc::new(client));

    let port_number = port.parse::<u16>().expect("PORT must be a port number");
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port_number))
        .await
        .expect("could not bind the port");
    eprintln!("[provider] [start] port={port} site={site} version={VERSION} - listening");
    axum::serve(listener, app).await.expect("server stopped");
}

fn env(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}
