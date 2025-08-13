use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use dotenvy::dotenv;
use serenity::all::*;
use serenity::async_trait;
use sqlx::{postgres::PgPoolOptions, PgPool};
pub mod engine;

use dashmap::DashMap;
use tokio::sync::Semaphore;

mod commands;
use crate::commands::{admcontrol, shop_ui};
use commands::{balance, crime, daily, pay, rob, slut, work};
mod utils;

// ----------------------------
// Entrypoint
// ----------------------------

pub fn init_tracing() {
    // nie wywali się, jeśli już zainicjalizowane
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

pub async fn run() -> anyhow::Result<()> {
    init_tracing();
    dotenv().ok();

    let token = env::var("DISCORD_TOKEN")?;
    let database_url = env::var("DATABASE_URL")?;

    // --- DB pool ---
    let max_conn: u32 = env::var("DB_MAX_CONN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let pool = PgPoolOptions::new()
        .max_connections(max_conn)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .max_lifetime(Duration::from_secs(60 * 30))
        .idle_timeout(Duration::from_secs(60 * 10))
        .test_before_acquire(true)
        .after_connect(|conn, _meta| Box::pin(async move {
            // Mały warmup by rozgrzać statement cache
            let _ = sqlx::query("SELECT 1").execute(conn).await;
            Ok(())
        }))
        .connect(&database_url)
        .await?;

    // bootstrap w jednej transakcji
    {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT 1").execute(&mut *tx).await?; // healthcheck

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS users (
                id BIGINT PRIMARY KEY,
                balance BIGINT NOT NULL DEFAULT 0,
                last_work TIMESTAMPTZ
            )",
        )
        .execute(&mut *tx)
        .await?;

        // opcjonalne kolumny w logs
        sqlx::query("ALTER TABLE IF NOT EXISTS logs ADD COLUMN IF NOT EXISTS target_id BIGINT")
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("ALTER TABLE IF NOT EXISTS logs ADD COLUMN IF NOT EXISTS description TEXT")
            .execute(&mut *tx)
            .await
            .ok();

        tx.commit().await?;
    }

    let db = Arc::new(pool);

    // --- anty-spam + throttling ---
    let max_inflight: usize = env::var("MAX_INFLIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    // Usuwamy potrzebe Box::leak: używamy Stringa jako części klucza
    let inflight: Arc<DashMap<(u64, String), Instant>> = Arc::new(DashMap::new());
    let semaphore = Arc::new(Semaphore::new(max_inflight));

    // metrics channel parsujemy raz
    let metrics_channel = env::var("METRICS_CHANNEL_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&id| id != 0)
        .map(ChannelId::new);

    // --- Discord ---
    let intents = GatewayIntents::non_privileged();

    let mut client = Client::builder(token, intents)
        .event_handler(Handler {
            db,
            inflight,
            semaphore,
            metrics_channel,
        })
        .await?;

    client.start().await?;
    Ok(())
}

// ----------------------------
// Handler
// ----------------------------
struct Handler {
    db: Arc<PgPool>,
    inflight: Arc<DashMap<(u64, String), Instant>>, // (user_id, command)
    semaphore: Arc<Semaphore>,
    metrics_channel: Option<ChannelId>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} jest online!", ready.user.name);

        let mut commands: Vec<CreateCommand> = Vec::new();
        commands.push(slut::register());

        {
            let mut c = builder::CreateCommand::new("work");
            work::register(&mut c);
            commands.push(c);
        }
        {
            let c = builder::CreateCommand::new("crime");
            let c = crime::register();
            commands.push(c);
        }
        {
            let mut c = builder::CreateCommand::new("daily");
            daily::register(&mut c);
            commands.push(c);
        }
        {
            let mut c = builder::CreateCommand::new("rob");
            rob::register(&mut c);
            commands.push(c);
        }
        {
            let mut c = builder::CreateCommand::new("balance");
            balance::register(&mut c);
            commands.push(c);
        }
        {
            let mut c = builder::CreateCommand::new("pay");
            pay::register(&mut c);
            commands.push(c);
        }
        {
            let mut c = builder::CreateCommand::new("admcontrol");
            admcontrol::register(&mut c);
            commands.push(c);
        }
        {
            let mut c = builder::CreateCommand::new("shop"); // lub "tigrisshop"
            shop_ui::register(&mut c);
            commands.push(c);
        }

        if let Err(err) = Command::set_global_commands(&ctx.http, commands).await {
            eprintln!("❌ Nie udało się ustawić globalnych komend: {err:?}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Component(component) => {
    let id = component.data.custom_id.as_str();

    if id.starts_with("work:") {
        let _ = work::handle_component(&ctx, &component, &self.db).await;
    } else if id.starts_with("shop:") || id.starts_with("tshop:") {
        let _ = shop_ui::handle_component(&ctx, &component, &self.db).await;
    } else if id.starts_with("slut:") {
        let _ = slut::handle_component(&ctx, &component, &self.db).await;
    } else if id.starts_with("crime:") {
        let _ = crime::handle_component(&ctx, &component, &self.db).await;
    } else {
        let _ = component.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("⚠️ Ta interakcja nie jest już obsługiwana. Użyj komendy ponownie."),
            ),
        ).await;
    }
}

            Interaction::Modal(modal) => {
    let id = modal.data.custom_id.as_str();

    if id.starts_with("shop:") || id.starts_with("tshop:") {
        let _ = shop_ui::handle_modal(&ctx, &modal, &self.db).await;
    } else if id.starts_with("work:") {
        let _ = modal.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("📝 Ten modal nie jest obsługiwany."),
            ),
        ).await;
    } else if id.starts_with("crime:") {
        let _ = crime::handle_modal(&ctx, &modal, &self.db).await;
    } else {
        let _ = modal.create_response(
    &ctx.http,
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .ephemeral(true)
            .content("⚠️ Nieznana interakcja."),
    ),
).await;
    }
}

            Interaction::Command(cmd) => {
                let user_id = cmd.user.id.get();
                let name = cmd.data.name.as_str();

                // anti-spam: (user, command)
                let key = (user_id, name.to_owned());
                use dashmap::mapref::entry::Entry;
                match self.inflight.entry(key.clone()) {
                    Entry::Occupied(_) => {
                        let _ = cmd
                            .create_response(
                                &ctx.http,
                                CreateInteractionResponse::Message(
                                    CreateInteractionResponseMessage::new()
                                        .ephemeral(true)
                                        .content("⏳ Ta komenda już się wykonuje. Daj mi chwilkę…"),
                                ),
                            )
                            .await;
                        return;
                    }
                    Entry::Vacant(v) => {
                        v.insert(std::time::Instant::now());
                    }
                }

                // guard usuwający wpis z inflight
                let guard = InFlightGuard {
                    key: key.clone(),
                    map: self.inflight.clone(),
                };

                // globalny limit równoległości
                let _permit = match self.semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        let _ = cmd
                            .create_response(
                                &ctx.http,
                                CreateInteractionResponse::Message(
                                    CreateInteractionResponseMessage::new()
                                        .ephemeral(true)
                                        .content("🛠️ Bot się restartuje. Spróbuj za chwilę."),
                                ),
                            )
                            .await;
                        return;
                    }
                };

                let start_total = std::time::Instant::now();
                let result = match name {
                    "work" => work::run(&ctx, &cmd, &self.db).await,
                    "crime" => crime::run(&ctx, &cmd, &self.db).await,
                    "slut" => slut::run(&ctx, &cmd, &self.db).await,
                    "daily" => daily::run(&ctx, &cmd, &self.db).await,
                    "rob" => rob::run(&ctx, &cmd, &self.db).await,
                    "balance" => balance::run(&ctx, &cmd, &self.db).await,
                    "pay" => pay::run(&ctx, &cmd, &self.db).await,
                    "admcontrol" => admcontrol::run(&ctx, &cmd, &self.db).await,
                    "shop" | "tigrisshop" => shop_ui::run(&ctx, &cmd, &self.db).await,
                    _ => Ok(()),
                };

                drop(guard);

                let total_ms = start_total.elapsed().as_millis() as u64;
                let ok = result.is_ok();

                if let Err(e) = result {
                    eprintln!("❌ Błąd /{}: {:?}", name, e);
                }

                if let Some(ch) = self.metrics_channel {
                    let http: std::sync::Arc<Http> = ctx.http.clone();
                    let uname = cmd.user.name.clone();
                    let uid = cmd.user.id.get();
                    let cname = name.to_string();

                    tokio::spawn(async move {
                        let _ = log_command_metric_http(http, ch, uname, uid, cname, total_ms, None, ok).await;
                    });
                }
            }

            _ => {} // ignorujemy inne typy interakcji
        } // <— zamknięcie match
    } // <— zamknięcie fn interaction_create
} // <— zamknięcie impl EventHandler


// guard usuwający wpis z inflight
struct InFlightGuard {
    key: (u64, String),
    map: Arc<DashMap<(u64, String), Instant>>,
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.map.remove(&self.key);
    }
}

// ===== Helpers: metryki =====

async fn log_command_metric_http(
    http: Arc<Http>,
    channel_id: ChannelId,
    user_name: String,
    user_id: u64,
    command_name: String,
    total_ms: u64,
    shard_latency_ms: Option<u64>,
    ok: bool,
) -> anyhow::Result<()> {
    let status = if ok { "✅ OK" } else { "❌ ERR" };
    let shard_s = shard_latency_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into());

    let embed = CreateEmbed::new()
        .title("⏱️ Metryka komendy")
        .field("Komenda", format!("/{}", command_name), true)
        .field("Użytkownik", format!("{} (`{}`)", user_name, user_id), true)
        .field("Całkowity czas", format!("{total_ms} ms"), true)
        .field("Shard latency", shard_s, true)
        .field("Status", status, true)
        .timestamp(Utc::now());

    let msg = CreateMessage::new()
        .allowed_mentions(CreateAllowedMentions::new())
        .embed(embed);

    channel_id.send_message(&http, msg).await?;
    Ok(())
}
