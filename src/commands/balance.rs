use anyhow::Result;
use serenity::all::*;
use serenity::all::CommandOptionType;
use serenity::builder::{CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedAuthor};
use sqlx::{PgPool, Row};
use num_format::{Locale, ToFormattedString};

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("balance")
        .description("Sprawdź saldo swoje lub innego gracza 💰")
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::User,
                "użytkownik",
                "Użytkownik, którego saldo chcesz sprawdzić",
            )
            .required(false),
        );
    cmd
}

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    let (user, user_id) = match cmd.data.options.get(0) {
        Some(opt) => match &opt.value {
            CommandDataOptionValue::User(uid) => {
                if let Some(u) = cmd.data.resolved.users.get(uid).cloned() {
                    (u.clone(), u.id.get())
                } else {
                    (cmd.user.clone(), cmd.user.id.get())
                }
            }
            _ => (cmd.user.clone(), cmd.user.id.get()),
        },
        None => (cmd.user.clone(), cmd.user.id.get()),
    };

    let balance: i64 = sqlx::query("SELECT balance FROM users WHERE id = $1")
        .bind(user_id as i64)
        .fetch_optional(db)
        .await?
        .and_then(|row| row.try_get("balance").ok())
        .unwrap_or(0);

    // Formatowanie z separatorami tysięcy (np. 1 234 567)
    let balance_str = balance.to_formatted_string(&Locale::pl);

    let embed = CreateEmbed::new()
        .title("💰 Saldo konta")
        .description(format!("{} posiada **{} TK**", user.mention(), balance_str))
        .color(0x00BFFF)
        .author(
            CreateEmbedAuthor::new(&user.name)
                .icon_url(user.avatar_url().unwrap_or_default()),
        );

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embed(embed),
        ),
    )
    .await?;

    Ok(())
}
