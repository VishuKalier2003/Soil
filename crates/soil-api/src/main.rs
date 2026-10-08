//! `soil-api` — the Soil control plane server.
//!
//! Usage: `soil-api [--bind 127.0.0.1:7700] [--no-seed]`

use soil_api::{router, seeded_store, AppState, Store};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bind = args
        .iter()
        .position(|a| a == "--bind")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("SOIL_API_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1:7700".into());
    let store = if args.iter().any(|a| a == "--no-seed") {
        Store::default()
    } else {
        seeded_store()
    };
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("soil-api: cannot bind {bind}: {e}");
            std::process::exit(1);
        }
    };
    println!("soil-api listening on http://{bind}  (projects: GET /v1/projects)");
    if let Err(e) = axum::serve(listener, router(AppState::new(store))).await {
        eprintln!("soil-api: {e}");
        std::process::exit(1);
    }
}
