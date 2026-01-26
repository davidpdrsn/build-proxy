//! A simple test server used by e2e tests.
//!
//! It reads `PORT` from the environment and exposes:
//! - `GET /` - returns "hello"
//! - `GET /instance-id` - returns a unique ID for this server instance (the PID)

use axum::{routing::get, Router};
use std::net::Ipv4Addr;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .expect("PORT env var must be set")
        .parse()
        .expect("PORT must be a valid u16");

    let instance_id = std::process::id().to_string();

    let app = Router::new()
        .route("/", get(|| async { "hello" }))
        .route(
            "/instance-id",
            get(move || {
                let id = instance_id.clone();
                async move { id }
            }),
        );

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();

    axum::serve(listener, app).await.unwrap();
}
