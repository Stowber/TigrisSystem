use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use dotenvy::dotenv;
use serenity::all::*;
use serenity::all::audit_log::{Action as AuditAction, MemberAction}; // <-- IMPORT
use serenity::async_trait;
use sqlx::{postgres::PgPoolOptions, PgPool};


pub mod engine;

use dashmap::DashMap;
use tokio::sync::Semaphore;

mod commands;
use crate::commands::{admcontrol, shop_ui};
use commands::{balance, crime, daily, pay, rob, slut, work, subscribers};
mod utils;

// ----------------------------
// Entrypoint
// ----------------------------

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

pub async fn run() -> anyhow::Result<()> {
    init_tracing();
    dotenv().ok();

    let token = env::var("DISCORD_TOKEN")?;
    let database_url = env::var("DATABASE_URL")?;

    // privileged intent do guild_member_update
    let intents = GatewayIntents::non_privileged() | GatewayIntents::GUILD_MEMBERS;

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
        sqlx::query("ALTER TABLE IF EXISTS logs ADD COLUMN IF NOT EXISTS target_id BIGINT")
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("ALTER TABLE IF EXISTS logs ADD COLUMN IF NOT EXISTS description TEXT")
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

    let inflight: Arc<DashMap<(u64, String), Instant>> = Arc::new(DashMap::new());
    let semaphore = Arc::new(Semaphore::new(max_inflight));

    // metrics channel parsujemy raz
    let metrics_channel = env::var("METRICS_CHANNEL_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&id| id != 0)
        .map(ChannelId::new);

    // --- Discord ---
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
            let mut c = builder::CreateCommand::new("shop");
            shop_ui::register(&mut c);
            commands.push(c);
        }
        {
            let mut c = builder::CreateCommand::new("subskrypcje");
            subscribers::register(&mut c);
            commands.push(c);
        }

        if let Err(err) = Command::set_global_commands(&ctx.http, commands).await {
            eprintln!("❌ Nie udało się ustawić globalnych komend: {err:?}");
        }

        // 🧹 usuń stare /shop z zakresu GUILD, żeby nie było duplikatów
        wipe_all_guild_commands(&ctx).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Component(ic) => {
                let id = ic.data.custom_id.as_str();
                eprintln!("[component] id={}", id);

                if id.starts_with("shop|") || id.starts_with("shopgift|") {
                    let _ = shop_ui::handle_component(&ctx, &ic, &self.db).await;
                    return;
                }
                if id.starts_with("work:") {
                    let _ = work::handle_component(&ctx, &ic, &self.db).await;
                    return;
                }
                if id.starts_with("slut:") {
                    let _ = slut::handle_component(&ctx, &ic, &self.db).await;
                    return;
                }
                if id.starts_with("crime:") {
                    let _ = crime::handle_component(&ctx, &ic, &self.db).await;
                    return;
                }

                let _ = ic
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .content("⚠️ Ta interakcja nie jest już obsługiwana. Użyj komendy ponownie."),
                        ),
                    )
                    .await;
            }

            Interaction::Modal(mi) => {
                let id = mi.data.custom_id.as_str();

                if id.starts_with("shop") {
                    let _ = shop_ui::handle_modal(&ctx, &mi, &self.db).await;
                    return;
                }
                if id.starts_with("crime:") {
                    let _ = crime::handle_modal(&ctx, &mi, &self.db).await;
                    return;
                }

                let _ = mi
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().ephemeral(true).content("⚠️ Nieznana interakcja."),
                        ),
                    )
                    .await;
            }

            Interaction::Command(cmd) => {
                let user_id = cmd.user.id.get();
                let name = cmd.data.name.as_str();

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

                let guard = InFlightGuard {
                    key: key.clone(),
                    map: self.inflight.clone(),
                };

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
                    "subskrypcje" => subscribers::run(&ctx, &cmd, &self.db).await,
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

            _ => {}
        }
    }

    async fn guild_member_update(
        &self,
        ctx: Context,
        _old_if_available: Option<Member>,
        new: Option<Member>,
        _event: GuildMemberUpdateEvent,
    ) {
        let Some(new) = new else { return };

        let rid = crate::commands::shop_ui::role_id();
        if new.roles.contains(&rid) {
            return;
        }

        let uid = new.user.id.get() as i64;
        let gid = new.guild_id.get() as i64;
        let rid_i = rid.get() as i64;

        let had_active: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            r#"SELECT expires_at FROM role_subscriptions
               WHERE user_id=$1 AND role_id=$2 AND guild_id=$3 AND active=true"#,
        )
        .bind(uid).bind(rid_i).bind(gid)
        .fetch_optional(&*self.db)
        .await
        .ok()
        .flatten();

        if had_active.is_none() {
            return;
        }

        let admin_name_or_id = match find_role_remover(&ctx.http, new.guild_id, new.user.id, rid).await {
            Some(u) => format!("<@{}>", u.get()),
            None => "nieustalony".to_string(),
        };

        let _ = sqlx::query(
            r#"UPDATE role_subscriptions
               SET active=false
               WHERE user_id=$1 AND role_id=$2 AND guild_id=$3 AND active=true"#,
        )
        .bind(uid).bind(rid_i).bind(gid)
        .execute(&*self.db)
        .await;

        crate::commands::shop_ui::dm_user(
            &ctx.http,
            new.user.id,
            CreateEmbed::new()
                .title("⚠️ Ranga cofnięta przez administrację")
                .description(
                    "Twoja ranga została cofnięta przez administrację serwera Unfaithful.\n\
                     Po więcej informacji skontaktuj się z administracją serwera unfaithful.",
                )
                .field("Administrator", admin_name_or_id.clone(), true)
                .field("Data", crate::commands::shop_ui::fmt_dt_full(chrono::Utc::now()), true)
                .color(0xE74C3C)
                .timestamp(chrono::Utc::now()),
        ).await;

        crate::commands::shop_ui::log_embed(
            &ctx.http,
            CreateEmbed::new()
                .title("❌ Log: Rola odebrana ręcznie")
                .description(format!(
                    "Rola <@&{}> została odebrana użytkownikowi <@{}> przez administratora.",
                    rid.get(),
                    new.user.id.get()
                ))
                .field("Administrator", admin_name_or_id, true)
                .field("Data", crate::commands::shop_ui::fmt_dt_full(chrono::Utc::now()), true)
                .color(0xE74C3C)
                .timestamp(chrono::Utc::now()),
        ).await;
    }
}

// Pomocnik: spróbuj zczytać z Audit Log kto zdjął rolę
async fn find_role_remover(
    http: &Http,
    guild_id: GuildId,
    target_user: UserId,
    _role_id: RoleId,
) -> Option<UserId> {
    // daj chwilę, aż wpis trafi do logów
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // docelowy identyfikator jako GenericId (From<u64> istnieje)
    let target_generic: GenericId = GenericId::from(target_user.get());

    // podejście wąskie: tylko Member(RoleUpdate), target = user
    if let Ok(logs) = guild_id
        .audit_logs(
            http,
            Some(AuditAction::Member(MemberAction::RoleUpdate)),
            Some(target_user),
            None,
            Some(50),
        )
        .await
    {
        for entry in logs.entries {
            // Action nie ma PartialEq, więc używamy pattern matching
            if matches!(entry.action, AuditAction::Member(MemberAction::RoleUpdate))
                && entry.target_id == Some(target_generic)
            {
                return Some(entry.user_id);
            }
        }
    }

    // fallback: bez filtra akcji — nadal porównujemy target_id jako GenericId
    if let Ok(logs) = guild_id
        .audit_logs(http, None, Some(target_user), None, Some(50))
        .await
    {
        let target_generic: GenericId = GenericId::from(target_user.get());
        for entry in logs.entries {
            if entry.target_id == Some(target_generic) {
                return Some(entry.user_id);
            }
        }
    }

    None
}

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

async fn wipe_all_guild_commands(ctx: &Context) {
    // lista gildii z cache'a
    for gid in ctx.cache.guilds() {
        // Serenity 0.12: set_commands nadpisuje cały zestaw komend w gildi.
        // Pusta lista => żadnych komend GUILD.
        let _ = gid.set_commands(&ctx.http, Vec::<builder::CreateCommand>::new()).await;
    }
}

async fn admin_display(http: &Http, uid: UserId) -> String {
    match http.get_user(uid).await {
        Ok(u) => format!("{} (`{}`)", u.tag(), uid.get()), // np. Tigris#1234 (`9876543210`)
        Err(_) => format!("`{}`", uid.get()),
    }
}
