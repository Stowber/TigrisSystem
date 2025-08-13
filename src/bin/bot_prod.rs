use dotenvy::from_filename;
use tigrus_bot::run;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    from_filename(".env.prod").ok(); // ładuje plik .env.prod
    run().await
}