//! `fraud-mock`: serves `GET /score?customer=NAME` -> `{"score": N}`.
//! `BIND` (default 127.0.0.1:3100) selects the listen address.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3100".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("fraud-mock listening on {bind}");
    axum::serve(listener, storefront::fraud::router()).await?;
    Ok(())
}
