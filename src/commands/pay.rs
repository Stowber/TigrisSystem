use anyhow::Result;
use chrono::Utc;
use serenity::all::CommandDataOptionValue;
use serenity::all::*;
use serenity::builder::CreateCommand;
use sqlx::{PgPool, Row};
use crate::utils::log_action;

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("pay")
        .description("Przelej TK innemu graczowi 💸")
        .add_option(
            CreateCommandOption::new(CommandOptionType::User, "cel", "Odbiorca")
                .required(true),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::Integer,
                "kwota",
                "Ile TK chcesz przelać?",
            )
            .required(true),
        );
    cmd
}

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    let sender = &cmd.user;
    let sender_id = sender.id.get();

    let (target_user, amount) = match parse_args(cmd) {
        Some(v) => v,
        None => return respond_error(ctx, cmd, "❌ Nieprawidłowe argumenty.").await,
    };

    if target_user.id.get() == sender_id {
        return respond_error(ctx, cmd, "❌ Nie możesz przelać TK samemu sobie!").await;
    }
    if amount <= 0 {
        return respond_error(ctx, cmd, "❌ Kwota musi być większa niż 0!").await;
    }

    // 🔁 Transakcja atomowa
    let mut tx = db.begin().await?;

    // Upewnij się, że istnieją rekordy dla obu użytkowników
    sqlx::query(
        "INSERT INTO users (id, balance) VALUES ($1,0), ($2,0) ON CONFLICT (id) DO NOTHING",
    )
    .bind(sender_id as i64)
    .bind(target_user.id.get() as i64)
    .execute(&mut *tx)
    .await?;

    // Zablokuj saldo nadawcy
    let sender_balance: i64 = sqlx::query("SELECT balance FROM users WHERE id = $1 FOR UPDATE")
        .bind(sender_id as i64)
        .fetch_one(&mut *tx)
        .await?
        .try_get("balance")?;

    if sender_balance < amount {
        tx.rollback().await?;
        return respond_error(ctx, cmd, "❌ Nie masz wystarczającej ilości TK.").await;
    }

    // Odejmij nadawcy
    sqlx::query("UPDATE users SET balance = balance - $1 WHERE id = $2")
        .bind(amount)
        .bind(sender_id as i64)
        .execute(&mut *tx)
        .await?;

    // Dodaj odbiorcy
    sqlx::query(
        "UPDATE users SET balance = balance + $1 WHERE id = $2",
    )
    .bind(amount)
    .bind(target_user.id.get() as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // 🧾 Log do DB (fire-and-forget OK, ale tu czekamy na wynik)
    log_action(
        db,
        sender_id,
        "pay",
        Some(target_user.id.get()),
        Some(amount),
        Some(&format!("Przelał {} TK do {}", amount, target_user.tag())),
    ).await?;

    // 📢 Log na kanał (jeśli ustawiony)
    let _ = send_log_to_channel(ctx, sender, target_user.clone(), amount).await;

    // 📤 Potwierdzenie dla nadawcy
    let embed = build_sender_embed(sender, &target_user, amount);
    respond_embed(ctx, cmd, embed).await?;

    Ok(())
}

fn parse_args(cmd: &CommandInteraction) -> Option<(User, i64)> {
    let mut target_user: Option<User> = None;
    let mut amount: Option<i64> = None;

    for opt in &cmd.data.options {
        match (&*opt.name, &opt.value) {
            ("cel", CommandDataOptionValue::User(uid)) => {
                target_user = cmd.data.resolved.users.get(uid).cloned();
            }
            ("kwota", CommandDataOptionValue::Integer(i)) => {
                amount = Some(*i);
            }
            _ => {}
        }
    }

    Some((target_user?, amount?))
}

fn build_sender_embed(_sender: &User, target: &User, amount: i64) -> CreateEmbed {
    CreateEmbed::new()
        .title("📤 Przelew wysłany!")
        .description(format!("💸 Przesłałeś środki do {}!", target.mention()))
        .field("Kwota", format!("**{} TK**", amount), true)
        .field("Do", target.tag(), true)
        .footer(CreateEmbedFooter::new("Dziękujemy za korzystanie z Tigrus Bank™ 💼"))
        .color(0x00AAFF)
        .timestamp(chrono::Utc::now())
}

async fn respond_error(ctx: &Context, cmd: &CommandInteraction, message: &str) -> Result<()> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .content(message),
        ),
    ).await?;
    Ok(())
}

async fn respond_embed(ctx: &Context, cmd: &CommandInteraction, embed: CreateEmbed) -> Result<()> {
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

async fn send_log_to_channel(ctx: &Context, sender: &User, target: User, amount: i64) -> Result<()> {
    let log_channel_id = std::env::var("LOG_CHANNEL_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&id| id != 0);

    if let Some(id) = log_channel_id {
        let channel = ChannelId::new(id);

        let embed = CreateEmbed::new()
    .title("📒 Log przelewu (/pay)")
    .description("Transakcja została wykonana pomyślnie 💸")
    .color(0xFFD700)
    .field(
        "👤 Nadawca",
        format!("{} (`{}`)\n{}", sender.tag(), sender.id.get(), sender.mention()),
        true,
    )
    .field(
        "🎯 Odbiorca",
        format!("{} (`{}`)\n{}", target.tag(), target.id.get(), target.mention()),
        true,
    )
    .field("💰 Kwota", format!("**{} TK**", amount), false)
    .footer(CreateEmbedFooter::new("Zalogowano przez Tigrus Bank™"))
    .timestamp(Utc::now());

channel
    .send_message(&ctx.http, CreateMessage::new().embed(embed))
    .await?;
    }

    Ok(())
}
