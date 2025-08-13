use anyhow::Result;
use serenity::all::ChannelId;
use sqlx::PgPool;

pub async fn log_action(
    db: &PgPool,
    user_id: u64,
    action: &str,
    target_id: Option<u64>,
    amount: Option<i64>,
    description: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO logs (user_id, action, target_id, amount, description, created_at)
        VALUES ($1, $2, $3, $4, $5, NOW())
        "#,
    )
    .bind(user_id as i64)
    .bind(action)
    .bind(target_id.map(|id| id as i64))
    .bind(amount)
    .bind(description)
    .execute(db)
    .await?;

    Ok(())
}

/// Pobiera identyfikator kanału logów z ENV.
/// Zwraca `None`, jeśli zmienna nie istnieje, jest pusta lub równa 0.
pub fn get_log_channel_id() -> Option<ChannelId> {
    std::env::var("LOG_CHANNEL_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&id| id != 0)
        .map(ChannelId::new)
}
