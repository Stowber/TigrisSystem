use dotenvy::from_filename;
use tigrus_bot::run;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    from_filename(".env.dev").ok(); // ładuje plik .env.dev
    run().await
}