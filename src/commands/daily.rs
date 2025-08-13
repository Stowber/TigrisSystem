use anyhow::{anyhow, Context as AnyCtx, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rand::Rng;
use serenity::all::*;
use serenity::builder::CreateCommand;
use sqlx::{PgPool, Row, Postgres, Transaction};
use serenity::builder::{CreateInteractionResponse, CreateInteractionResponseMessage};

use crate::utils::log_action;

const DAILY_COOLDOWN_HOURS: i64 = 24;
const COOLDOWN_SECS: i64 = DAILY_COOLDOWN_HOURS * 3600;

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("daily")
        .description("Odbierz codzienną nagrodę 💸");
    cmd
}

pub async fn ensure_daily_schema(db: &PgPool) -> anyhow::Result<()> {
    // Oddzielne zapytania – stabilniej na różnych setupach
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
          id BIGINT PRIMARY KEY,
          balance BIGINT NOT NULL DEFAULT 0
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        ALTER TABLE users
          ADD COLUMN IF NOT EXISTS last_daily TIMESTAMPTZ NULL;
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    // Schema best-effort (bez paniki jak się nie uda)
    let _ = ensure_daily_schema(db).await;

    // Defer z ephemeral, żeby nie złapać 3s timeoutu
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        ),
    ).await?;

    let user_id_u64 = cmd.user.id.get();
    let now = Utc::now();

    // RNG w krótkim scope
    let reward: i64 = {
        let mut rng = rand::rng();        // rand 0.9
        rng.random_range(250..=500)       // nowa metoda
    };

    match claim_daily(db, user_id_u64, reward, now).await? {
        ClaimOutcome::Claimed { balance_after } => {
            // Odpowiedź
            let embed = build_daily_reward_embed(reward, &cmd.user, balance_after);
            edit_embed(ctx, cmd, embed).await?;

            // Log do bazy (best effort)
            let _ = log_action(
                db,
                user_id_u64,
                "daily",
                None,
                Some(reward),
                Some(&format!("Odebrano daily: {} TK", reward)),
            ).await;

            // Log do kanału (opcjonalny)
            if let Some(ch) = log_channel() {
                let embed = CreateEmbed::new()
                    .title("🎁 Log: Codzienna nagroda (/daily)")
                    .description(format!(
                        "**{}** (`{}`) odebrał codzienną nagrodę **{} TK**.",
                        cmd.user.name, user_id_u64, reward
                    ))
                    .field(
                        "👤 Użytkownik",
                        format!("{}\n`{}`", cmd.user.mention(), user_id_u64),
                        true,
                    )
                    .field("💰 Zysk", format!("+{} TK", reward), true)
                    .color(0x33CC33)
                    .timestamp(Utc::now());

                let _ = ch
                    .send_message(&ctx.http, CreateMessage::new().embed(embed))
                    .await;
            }
        }
        ClaimOutcome::OnCooldown { remaining_secs } => {
            let embed = build_cooldown_embed(remaining_secs);
            edit_embed(ctx, cmd, embed).await?;
        }
    }

    Ok(())
}

/// Rezultat próby odebrania daily
enum ClaimOutcome {
    Claimed { balance_after: i64 },
    OnCooldown { remaining_secs: i64 },
}

/// Cała logika cooldownu w jednej transakcji z blokadą wiersza
async fn claim_daily(
    db: &PgPool,
    user_id_u64: u64,
    reward: i64,
    now: DateTime<Utc>,
) -> Result<ClaimOutcome> {
    let user_id = i64::try_from(user_id_u64).context("ID usera nie mieści się w i64")?;
    let mut tx: Transaction<'_, Postgres> = db.begin().await?;

    // Zablokuj rekord użytkownika jeśli istnieje
    let row_opt = sqlx::query(
        r#"SELECT balance, last_daily FROM users WHERE id = $1 FOR UPDATE"#,
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;

    // Helper: odczytaj last_daily niezależnie od typu kolumny
    fn read_last_daily(row: &sqlx::postgres::PgRow) -> Result<Option<DateTime<Utc>>> {
        // timestamptz
        if let Ok(v) = row.try_get::<Option<DateTime<Utc>>, _>("last_daily") {
            return Ok(v);
        }
        // timestamp (bez strefy)
        if let Ok(v) = row.try_get::<Option<NaiveDateTime>, _>("last_daily") {
            return Ok(v.map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)));
        }
        Err(anyhow!("Nieobsługiwany typ kolumny last_daily"))
    }

    let outcome = if let Some(row) = row_opt {
        let last_daily = read_last_daily(&row)?;

        if let Some(last) = last_daily {
            let elapsed = now.signed_duration_since(last).num_seconds();
            if elapsed < COOLDOWN_SECS {
                // Nadal cooldown
                let remaining = COOLDOWN_SECS - elapsed;
                tx.rollback().await.ok();
                ClaimOutcome::OnCooldown { remaining_secs: remaining }
            } else {
                // Można przyznać
                let new_balance: i64 = sqlx::query(
                    r#"
                        UPDATE users
                           SET balance = balance + $2,
                               last_daily = $3
                         WHERE id = $1
                     RETURNING balance
                    "#,
                )
                .bind(user_id)
                .bind(reward)
                .bind(now)
                .fetch_one(&mut *tx)
                .await?
                .try_get("balance")?;

                tx.commit().await?;
                ClaimOutcome::Claimed { balance_after: new_balance }
            }
        } else {
            // Pierwszy raz — ustaw last_daily teraz i dodaj nagrodę
            let new_balance: i64 = sqlx::query(
                r#"
                    UPDATE users
                       SET balance = balance + $2,
                           last_daily = $3
                     WHERE id = $1
                 RETURNING balance
                "#,
            )
            .bind(user_id)
            .bind(reward)
            .bind(now)
            .fetch_one(&mut *tx)
            .await?
            .try_get("balance")?;

            tx.commit().await?;
            ClaimOutcome::Claimed { balance_after: new_balance }
        }
    } else {
        // Brak wiersza — wstaw
        let new_balance: i64 = sqlx::query(
            r#"
            INSERT INTO users (id, balance, last_daily)
            VALUES ($1, $2, $3)
            RETURNING balance
            "#,
        )
        .bind(user_id)
        .bind(reward)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?
        .try_get("balance")?;

        tx.commit().await?;
        ClaimOutcome::Claimed { balance_after: new_balance }
    };

    Ok(outcome)
}

fn build_cooldown_embed(remaining_secs: i64) -> CreateEmbed {
    let hours = remaining_secs / 3600;
    let minutes = (remaining_secs % 3600) / 60;
    let seconds = remaining_secs % 60;

    CreateEmbed::new()
        .title("⏳ Jeszcze za wcześnie!")
        .description(format!(
            "Odbierzesz ponownie za **{:02}:{:02}:{:02}**.",
            hours, minutes, seconds
        ))
        .color(0xFFA500)
        .timestamp(Utc::now())
}

fn build_daily_reward_embed(reward: i64, user: &User, balance: i64) -> CreateEmbed {
    CreateEmbed::new()
        .title("🎁 Codzienna nagroda odebrana!")
        .description(format!(
            "{}\n\nZgarnąłeś **{} TK** za logowanie!",
            user.mention(),
            reward
        ))
        .color(0x33CC33)
        .field("💰 Zysk", format!("+{} TK", reward), true)
        .footer(CreateEmbedFooter::new(format!("Saldo: {} TK", balance)))
        .author(
            CreateEmbedAuthor::new(&user.name)
                .icon_url(user.avatar_url().unwrap_or_default()),
        )
        .timestamp(Utc::now())
}

async fn edit_embed(ctx: &Context, cmd: &CommandInteraction, embed: CreateEmbed) -> Result<()> {
    cmd.edit_response(
        &ctx.http,
        EditInteractionResponse::new()
            .content("")
            .embed(embed),
    ).await?;
    Ok(())
}

fn log_channel() -> Option<ChannelId> {
    std::env::var("LOG_CHANNEL_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&id| id != 0)
        .map(ChannelId::new)
}
