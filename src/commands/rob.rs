use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use once_cell::sync::OnceCell as SyncOnceCell;
use rand::Rng; // rand 0.9: daje random_bool / random_range
use serenity::all::*;
use serenity::all::CommandOptionType;
use serenity::builder::{CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedAuthor, CreateMessage};
use sqlx::{PgPool, Row};
use tokio::sync::OnceCell as AsyncOnceCell;

use crate::utils::log_action;

// =======================
// ⚙️ Stałe
// =======================

const ROB_COOLDOWN_SECS: i64 = 600;
const MIN_BALANCE_TO_ROB: i64 = 50;
const MIN_STOLEN: i64 = 25;
const MAX_STOLEN: i64 = 150;
const MIN_FINE: i64 = 25;
const MAX_FINE: i64 = 75;

// Cache kanału logów z ENV (raz na proces)
static LOG_CHAN: SyncOnceCell<Option<ChannelId>> = SyncOnceCell::new();
fn log_channel_id() -> Option<ChannelId> {
    *LOG_CHAN.get_or_init(|| {
        let id = std::env::var("LOG_CHANNEL_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if id == 0 { None } else { Some(ChannelId::new(id)) }
    })
}

// ensure_schema tylko raz na proces
static ENSURE_SCHEMA_ONCE: AsyncOnceCell<()> = AsyncOnceCell::const_new();

// =======================
// 🔧 Rejestracja komendy
// =======================

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("rob")
        .description("Spróbuj okraść innego gracza 💼")
        .add_option(
            CreateCommandOption::new(CommandOptionType::User, "cel", "Kogo chcesz okraść?")
                .required(true),
        );
    cmd
}

// =======================
// 🚀 Obsługa komendy
// =======================

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    // Jednorazowy bootstrap schematu
    let _ = ENSURE_SCHEMA_ONCE
        .get_or_try_init(|| async {
            ensure_schema(db).await?;
            Ok::<(), anyhow::Error>(())
        })
        .await;

    let robber = &cmd.user;
    let robber_id = robber.id.get();

    let Some(target_user) = parse_target_user(cmd) else {
        return respond_ephemeral(ctx, cmd, "❌ Nieprawidłowy użytkownik docelowy.").await;
    };

    if target_user.id.get() == robber_id {
        return respond_ephemeral(ctx, cmd, "🙅‍♂️ Nie możesz okradać samego siebie.").await;
    }

    // RNG (rand 0.9)
    // RNG (rand 0.9) — zamknięty w bloku, żeby nie żył przez await
    let (success, amount_opt, fine_opt) = {
    let mut rng = rand::rng();
    let success = rng.random_bool(0.5);
    let amount_opt = if success {
        Some(rng.random_range(MIN_STOLEN..=MAX_STOLEN))
    } else {
        None
    };
    let fine_opt = if success {
        None
    } else {
        Some(rng.random_range(MIN_FINE..=MAX_FINE))
    };
    (success, amount_opt, fine_opt)
    };

    // Próba rabunku (atomicznie)
    match try_rob(
        db,
        robber_id as i64,
        target_user.id.get() as i64,
        success,
        amount_opt,
        fine_opt,
    )
    .await?
    {
        RobState::Cooldown { remaining_secs } => {
            let embed = build_cooldown_embed(remaining_secs);
            respond_embed(ctx, cmd, embed).await?;
            spawn_ready_after(ctx.clone(), cmd.clone(), robber.clone(), remaining_secs, "/rob".to_string());
        }
        RobState::TargetTooPoor => {
            return respond_ephemeral(ctx, cmd, "👛 Cel jest zbyt biedny, nic nie ukradniesz!").await;
        }
        RobState::Success { amount, robber_balance, when } => {
            let embed = build_result_embed(
                true, amount, ROB_COOLDOWN_SECS, when, robber, &target_user, robber_balance,
            );
            respond_embed(ctx, cmd, embed).await?;

            // Logi w tle
            let pool = db.clone();
            let http = ctx.http.clone();
            let robber_c = robber.clone();
            let target_c = target_user.clone();
            tokio::spawn(async move {
                let _ = log_action(
                    &pool,
                    robber_id,
                    "rob",
                    Some(target_c.id.get()),
                    Some(amount),
                    Some(&format!("Ukradł {} TK od {}", amount, target_c.tag())),
                ).await;

                if let Some(ch) = log_channel_id() {
                    let _ = ch.send_message(
                        &http,
                        CreateMessage::new().embed(
                            CreateEmbed::new()
                                .title("💼 Log: Udany napad (/rob)")
                                .description(format!(
                                    "**{}** (`{}`) okradł **{}** (`{}`) na **{} TK**.",
                                    robber_c.tag(), robber_c.id.get(), target_c.tag(), target_c.id.get(), amount
                                ))
                                .field("👤 Złodziej", format!("{}\n`{}`", robber_c.mention(), robber_c.id.get()), true)
                                .field("🎯 Cel", format!("{}\n`{}`", target_c.mention(), target_c.id.get()), true)
                                .field("💰 Skradziona kwota", format!("**{} TK**", amount), false)
                                .color(0x00CC66)
                                .timestamp(Utc::now())
                        )
                    ).await;
                }
            });

            spawn_ready_after(ctx.clone(), cmd.clone(), robber.clone(), ROB_COOLDOWN_SECS, "/rob".to_string());
        }
        RobState::Failure { fine, robber_balance, when } => {
            let embed = build_result_embed(
                false, fine, ROB_COOLDOWN_SECS, when, robber, &target_user, robber_balance,
            );
            respond_embed(ctx, cmd, embed).await?;

            // Logi w tle
            let pool = db.clone();
            let http = ctx.http.clone();
            let robber_c = robber.clone();
            let target_c = target_user.clone();
            tokio::spawn(async move {
                let _ = log_action(
                    &pool,
                    robber_id,
                    "rob",
                    Some(target_c.id.get()),
                    Some(-fine),
                    Some(&format!("Grzywna {} TK dla {}", fine, target_c.tag())),
                ).await;

                if let Some(ch) = log_channel_id() {
                    let _ = ch.send_message(
                        &http,
                        CreateMessage::new().embed(
                            CreateEmbed::new()
                                .title("🚨 Log: Nieudany napad (/rob)")
                                .description(format!(
                                    "**{}** (`{}`) próbował okraść **{}** (`{}`), ale został złapany i zapłacił grzywnę **{} TK**.",
                                    robber_c.tag(), robber_c.id.get(), target_c.tag(), target_c.id.get(), fine
                                ))
                                .field("👤 Złodziej", format!("{}\n`{}`", robber_c.mention(), robber_c.id.get()), true)
                                .field("🎯 Cel", format!("{}\n`{}`", target_c.mention(), target_c.id.get()), true)
                                .field("💸 Grzywna", format!("**{} TK**", fine), false)
                                .color(0xCC3300)
                                .timestamp(Utc::now())
                        )
                    ).await;
                }
            });

            spawn_ready_after(ctx.clone(), cmd.clone(), robber.clone(), ROB_COOLDOWN_SECS, "/rob".to_string());
        }
    }

    Ok(())
}

fn spawn_ready_after(
    ctx: Context,
    cmd: CommandInteraction,
    user: User,
    secs: i64,
    command: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(secs as u64)).await;
        let ready = build_cooldown_ready_embed(&user, &command);
        let _ = cmd
            .edit_response(&ctx.http, EditInteractionResponse::new().embed(ready))
            .await;
    });
}

// =======================
// 🎯 Parsowanie celu
// =======================

fn parse_target_user(cmd: &CommandInteraction) -> Option<User> {
    let opt = cmd.data.options.get(0)?;
    if opt.name != "cel" {
        return None;
    }
    match &opt.value {
        CommandDataOptionValue::User(uid) => cmd.data.resolved.users.get(uid).cloned(),
        _ => None,
    }
}

// =======================
// 🔁 Cooldown + logika w DB
// =======================

enum RobState {
    Cooldown { remaining_secs: i64 },
    TargetTooPoor,
    Success { amount: i64, robber_balance: i64, when: DateTime<Utc> },
    Failure { fine: i64, robber_balance: i64, when: DateTime<Utc> },
}

async fn try_rob(
    db: &PgPool,
    robber_id: i64,
    target_id: i64,
    success: bool,
    amount_opt: Option<i64>,
    fine_opt: Option<i64>,
) -> Result<RobState> {
    let now = Utc::now();

    // Jedna transakcja, minimalne RTT
    let mut tx = db.begin().await?;

    // Upewnij się, że rekordy istnieją
    sqlx::query(
        r#"
        INSERT INTO users (id, balance)
        VALUES ($1, 0), ($2, 0)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(robber_id)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;

    // Zablokuj oba wiersze do końca transakcji
    let robber_row = sqlx::query(
        r#"SELECT balance, last_rob FROM users WHERE id = $1 FOR UPDATE"#,
    )
    .bind(robber_id)
    .fetch_one(&mut *tx)
    .await?;
    let _initial_balance: i64 = robber_row.try_get("balance")?;
    let last_rob: Option<DateTime<Utc>> = robber_row.try_get("last_rob")?;
    let robber_balance: i64; // ustawimy w gałęziach success/failure

    let target_row = sqlx::query(
        r#"SELECT balance FROM users WHERE id = $1 FOR UPDATE"#,
    )
    .bind(target_id)
    .fetch_one(&mut *tx)
    .await?;
    let target_balance: i64 = target_row.try_get("balance")?;

    // Cooldown
    if let Some(last) = last_rob {
        let elapsed = (now - last).num_seconds();
        if elapsed < ROB_COOLDOWN_SECS {
            tx.rollback().await?;
            return Ok(RobState::Cooldown { remaining_secs: ROB_COOLDOWN_SECS - elapsed });
        }
    }

    // Za biedny cel
    if target_balance < MIN_BALANCE_TO_ROB {
        tx.rollback().await?;
        return Ok(RobState::TargetTooPoor);
    }

    if success {
        // Kwota kradzieży ograniczona saldem celu
        let mut steal_amount = amount_opt.unwrap_or(MIN_STOLEN);
        steal_amount = steal_amount.clamp(1, MAX_STOLEN);
        let steal_amount = steal_amount.min(target_balance).max(1);

        // 1) Odejmiemy z celu (warunek zapobiega zejściu poniżej zera)
        let updated = sqlx::query(
            r#"
            UPDATE users
            SET balance = balance - $1
            WHERE id = $2 AND balance >= $1
            RETURNING balance
            "#,
        )
        .bind(steal_amount)
        .bind(target_id)
        .fetch_optional(&mut *tx)
        .await?;

        if updated.is_none() {
            tx.rollback().await?;
            return Ok(RobState::TargetTooPoor);
        }

        // 2) Dodamy złodziejowi i ustawimy cooldown
        robber_balance = sqlx::query_scalar(
            r#"
            UPDATE users
            SET balance = balance + $1, last_rob = $2
            WHERE id = $3
            RETURNING balance
            "#,
        )
        .bind(steal_amount)
        .bind(now)
        .bind(robber_id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(RobState::Success { amount: steal_amount, robber_balance, when: now })
    } else {
        let fine = fine_opt.unwrap_or(MIN_FINE).clamp(MIN_FINE, MAX_FINE);

        // Odejmij grzywnę od złodzieja + cooldown
        robber_balance = sqlx::query_scalar(
            r#"
            UPDATE users
            SET balance = balance - $1, last_rob = $2
            WHERE id = $3
            RETURNING balance
            "#,
        )
        .bind(fine)
        .bind(now)
        .bind(robber_id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(RobState::Failure { fine, robber_balance, when: now })
    }
}

// =======================
// 🧱 Embedy
// =======================

fn build_cooldown_embed(remaining_secs: i64) -> CreateEmbed {
    let retry_time = Utc::now() + Duration::seconds(remaining_secs.max(0));
    let next_unix = retry_time.timestamp();

    CreateEmbed::new()
        .title("⏳ Jeszcze za wcześnie!")
        .description(format!(
            "Spróbuj ponownie **<t:{next_unix}:T>** • <t:{next_unix}:R>."
        ))
        .field(
            "Pozostało",
            format!(
                "`{:02}:{:02}`",
                remaining_secs.max(0) / 60,
                remaining_secs.max(0) % 60
            ),
            true,
        )
        .color(0xFFA500)
}

fn build_cooldown_ready_embed(user: &User, command: &str) -> CreateEmbed {
    CreateEmbed::new()
        .color(0x2ECC71)
        .author(
            CreateEmbedAuthor::new(&user.name)
                .icon_url(user.avatar_url().unwrap_or_default()),
        )
        .title("✅ Cooldown zakończony")
        .description(format!("Możesz już spróbować ponownie — użyj `{}`.", command))
        .timestamp(Utc::now())
}
fn build_result_embed(
    success: bool,
    amount: i64,
    cooldown_secs: i64,
    when: DateTime<Utc>,
    robber: &User,
    target: &User,
    robber_balance: i64,
) -> CreateEmbed {
    let next_at = when + Duration::seconds(cooldown_secs);
    let next_unix = next_at.timestamp();
    let remaining = (next_at - Utc::now()).num_seconds().max(0);

    CreateEmbed::new()
        .title(if success { "💼 Udany skok!" } else { "🚨 Porażka!" })
        .description(if success {
            format!(
                "{} okradł {} na **{} TK**!",
                robber.mention(),
                target.mention(),
                amount
            )
        } else {
            format!(
                "{} został złapany i zapłacił grzywnę **{} TK**.",
                robber.mention(),
                amount
            )
        })
        .color(if success { 0x00CC66 } else { 0xCC0000 })
        .field(
            if success { "💰 Zysk" } else { "💸 Grzywna" },
            format!("**{:+} TK**", if success { amount } else { -amount }),
            true,
        )
        .field("💳 Twoje saldo", format!("**{} TK**", robber_balance), true)
        .field(
            "⏳ Cooldown",
            format!(
                "`{:02}:{:02}` • do **<t:{next_unix}:T>** • <t:{next_unix}:R>",
                remaining / 60,
                remaining % 60
            ),
            false,
        )
        .author(
            CreateEmbedAuthor::new(&robber.name)
                .icon_url(robber.avatar_url().unwrap_or_default()),
        )
        .timestamp(when)
}

// =======================
// 📤 Odpowiedzi
// =======================

async fn respond_ephemeral(
    ctx: &Context,
    cmd: &CommandInteraction,
    msg: &str,
) -> Result<()> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .content(msg),
        ),
    ).await?;
    Ok(())
}

async fn respond_embed(
    ctx: &Context,
    cmd: &CommandInteraction,
    embed: CreateEmbed,
) -> Result<()> {
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

// =======================
// 🗄️ Schemat (idempotentny)
// =======================

async fn ensure_schema(db: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id          BIGINT PRIMARY KEY,
            balance     BIGINT NOT NULL DEFAULT 0,
            last_work   TIMESTAMPTZ,
            last_slut   TIMESTAMPTZ,
            last_crime  TIMESTAMPTZ,
            last_rob    TIMESTAMPTZ
        );
        "#,
    )
    .execute(db)
    .await?;

    // Na wypadek starych tabel bez kolumn:
    sqlx::query(r#"ALTER TABLE users ADD COLUMN IF NOT EXISTS balance    BIGINT     NOT NULL DEFAULT 0"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE users ADD COLUMN IF NOT EXISTS last_work  TIMESTAMPTZ"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE users ADD COLUMN IF NOT EXISTS last_slut  TIMESTAMPTZ"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE users ADD COLUMN IF NOT EXISTS last_crime TIMESTAMPTZ"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE users ADD COLUMN IF NOT EXISTS last_rob   TIMESTAMPTZ"#).execute(db).await?;

    Ok(())
}
