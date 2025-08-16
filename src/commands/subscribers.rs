use anyhow::Result;
use chrono::{DateTime, Utc};
use serenity::all::*;
use serenity::builder::{
    CreateCommand, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage,
};
use sqlx::PgPool;

/// Rejestracja komendy `/subskrypcje`
pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("subskrypcje")
        .description("Lista aktywnych subskrypcji rangi Tigris Kalwaryjski na tym serwerze")
        .dm_permission(false)
        // ograniczamy do administracji (możesz zmienić na inne uprawnienie)
        .default_member_permissions(Permissions::MANAGE_GUILD);
    cmd
}

/// Obsługa komendy `/subskrypcje`
pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    let Some(gid) = cmd.guild_id else {
        cmd.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("❌ Tę komendę można użyć tylko na serwerze."),
            ),
        ).await?;
        return Ok(());
    };

    // dane z modułu shop_ui
    let role_id = crate::commands::shop_ui::role_id();

    // pobierz aktywne subskrypcje z DB
    let rows: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
        r#"
        SELECT user_id, expires_at
        FROM role_subscriptions
        WHERE guild_id = $1 AND role_id = $2 AND active = true
        ORDER BY expires_at ASC
        "#,
    )
    .bind(gid.get() as i64)
    .bind(role_id.get() as i64)
    .fetch_all(db)
    .await?;

    let total = rows.len();

    // pokaż do 30 pozycji w embedzie (żeby nie przekroczyć limitów)
    let mut lines = Vec::new();
    for (uid, exp) in rows.iter().take(30) {
        lines.push(format!(
            "• <@{}> — wygasa: **{}**",
            uid,
            crate::commands::shop_ui::fmt_dt_full(*exp)
        ));
    }

    if total > 30 {
        lines.push(format!("… i jeszcze **{}** kolejnych.", total - 30));
    }

    let desc = if lines.is_empty() {
        "Brak aktywnych subskrypcji na tym serwerze.".to_string()
    } else {
        lines.join("\n")
    };

    // spójna kolorystyka (pomarańcz)
    let embed = CreateEmbed::new()
        .title("📋 Aktywne subskrypcje — Tigris Kalwaryjski")
        .description(desc)
        .field("Łącznie", total.to_string(), true)
        .field("Rola", format!("<@&{}>", role_id.get()), true)
        .color(0xFF7A00)
        .timestamp(Utc::now());

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embed(embed),
        ),
    ).await?;

    Ok(())
}
