use anyhow::{anyhow, Context as AnyCtx, Result};
use chrono::Utc;
use once_cell::sync::OnceCell as SyncOnceCell;
use serenity::all::*;
use serenity::all::CommandOptionType;
use serenity::builder::{CreateCommand, CreateCommandOption};
use serenity::all::{CommandDataOption, CommandDataOptionValue, CommandInteraction, User};
use sqlx::{PgPool, Row};
use std::collections::HashSet;

use crate::utils::log_action;

// =====================
// Stałe i cache
// =====================

static LOG_CHAN: SyncOnceCell<ChannelId> = SyncOnceCell::new();
static ADM_ROLES: SyncOnceCell<HashSet<RoleId>> = SyncOnceCell::new();

#[inline]
fn log_channel_id() -> Option<ChannelId> {
    LOG_CHAN.get().copied().or_else(|| {
        let id = std::env::var("LOG_CHANNEL_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())?;
        let ch = ChannelId::new(id);
        let _ = LOG_CHAN.set(ch);
        Some(ch)
    })
}

#[inline]
fn allowed_roles() -> &'static HashSet<RoleId> {
    ADM_ROLES.get_or_init(|| {
        let raw = std::env::var("ADMCONTROL_ROLE_IDS").unwrap_or_default();
        raw.split([',', ' '])
            .filter_map(|s| {
                let t = s.trim();
                if t.is_empty() {
                    return None;
                }
                Some(RoleId::new(t.parse::<u64>().ok()?))
            })
            .collect::<HashSet<_>>()
    })
}

// =====================
// Rejestracja komendy
// =====================

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("admcontrol")
        .description("Panel administracyjny ekonomii")
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "addmoney", "Dodaj TK graczowi")
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::User,
                        "gracz",
                        "Gracz, któremu dodać TK",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Integer,
                        "kwota",
                        "Kwota TK do dodania",
                    )
                    .required(true),
                ),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "removemoney", "Usuń TK graczowi")
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::User,
                        "gracz",
                        "Gracz, któremu usunąć TK",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Integer,
                        "kwota",
                        "Kwota TK do usunięcia",
                    )
                    .required(true),
                ),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "setmoney", "Ustaw dokładną ilość TK gracza")
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::User,
                        "gracz",
                        "Gracz, któremu ustawić TK",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Integer,
                        "kwota",
                        "Nowa ilość TK",
                    )
                    .required(true),
                ),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "resetcooldowns", "Resetuje cooldowny gracza")
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::User,
                        "gracz",
                        "Gracz do resetu cooldownów",
                    )
                    .required(true),
                ),
        );
    cmd
}

// =====================
// Główna obsługa
// =====================

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    // Defer — unikamy time-outu
    let _ = cmd
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new().ephemeral(true),
            ),
        )
        .await;

    if !is_authorized(cmd) {
        spawn_log(
            ctx.clone(),
            cmd.clone(),
            "no-perms".to_string(),
            None,
            None,
            Some("❌ Brak uprawnień".to_string()),
        );
        return edit_response(ctx, cmd, "❌ Brak uprawnień do użycia /admcontrol.").await;
    }

    let Some(sub) = cmd.data.options.first() else {
        spawn_log(
            ctx.clone(),
            cmd.clone(),
            "unknown".to_string(),
            None,
            None,
            Some("❌ Brak subkomendy".to_string()),
        );
        return edit_response(ctx, cmd, "❌ Nie podano subkomendy.").await;
    };

    match sub.name.as_str() {
        "addmoney" | "removemoney" | "setmoney" => {
            let (user, amount) = parse_user_amount(sub, cmd)
                .map_err(|e| anyhow!(e))
                .context("Parsowanie opcji gracz/kwota")?;

            if amount <= 0 && sub.name != "setmoney" {
                spawn_log(
                    ctx.clone(),
                    cmd.clone(),
                    sub.name.clone(),
                    Some(&user),
                    Some(amount),
                    Some("❌ Kwota ≤ 0".to_string()),
                );
                return edit_response(ctx, cmd, "❌ Kwota musi być dodatnia.").await;
            }

            let uid = i64::try_from(user.id.get()).context("ID użytkownika nie mieści się w i64")?;

            let final_balance = match sub.name.as_str() {
                "addmoney" => modify_balance(db, uid, amount).await?,
                "removemoney" => modify_balance(db, uid, -amount).await?,
                "setmoney" => set_balance(db, uid, amount).await?,
                _ => unreachable!(),
            };

            // log do bazy (best-effort)
            let _ = log_action(
                db,
                cmd.user.id.get(),
                sub.name.as_str(),
                Some(user.id.get()),
                Some(amount),
                None,
            )
            .await;

            // log do kanału + odpowiedź
            let summary = match sub.name.as_str() {
                "addmoney" => format!("✅ Dodano {amount} TK → nowe saldo: {final_balance}"),
                "removemoney" => format!("✅ Usunięto {amount} TK → nowe saldo: {final_balance}"),
                "setmoney" => format!("✅ Ustawiono saldo na {final_balance} TK"),
                _ => String::new(),
            };
            spawn_log(
                ctx.clone(),
                cmd.clone(),
                sub.name.clone(),
                Some(&user),
                Some(amount),
                Some(summary.clone()),
            );

            let msg = match sub.name.as_str() {
                "addmoney" => format!(
                    "✅ Dodano **{amount} TK** dla <@{}>. Nowe saldo: **{} TK**.",
                    user.id.get(),
                    final_balance
                ),
                "removemoney" => format!(
                    "✅ Usunięto **{amount} TK** od <@{}>. Nowe saldo: **{} TK**.",
                    user.id.get(),
                    final_balance
                ),
                "setmoney" => format!(
                    "✅ Ustawiono saldo <@{}> na **{} TK**.",
                    user.id.get(),
                    final_balance
                ),
                _ => unreachable!(),
            };
            edit_response(ctx, cmd, &msg).await?;
        }

        "resetcooldowns" => {
            let user = parse_user(sub, "gracz", cmd)
                .ok_or_else(|| anyhow!("Nie podano gracza"))?;
            let uid = i64::try_from(user.id.get()).context("ID użytkownika nie mieści się w i64")?;

            reset_cooldowns(db, uid).await?;
            let _ = log_action(
                db,
                cmd.user.id.get(),
                "resetcooldowns",
                Some(user.id.get()),
                None,
                None,
            )
            .await;

            spawn_log(
                ctx.clone(),
                cmd.clone(),
                "resetcooldowns".to_string(),
                Some(&user),
                None,
                Some("✅ Zresetowano cooldowny".to_string()),
            );
            edit_response(
                ctx,
                cmd,
                &format!("✅ Zresetowano cooldowny dla <@{}>.", user.id.get()),
            )
            .await?;
        }

        _ => {
            spawn_log(
                ctx.clone(),
                cmd.clone(),
                "unknown".to_string(),
                None,
                None,
                Some("❌ Nieznana subkomenda".to_string()),
            );
            edit_response(ctx, cmd, "❌ Nieznana subkomenda.").await?;
        }
    }

    Ok(())
}

// =====================
// Autoryzacja
// =====================

#[inline]
fn is_authorized(cmd: &CommandInteraction) -> bool {
    // Admin permisje zawsze przepuszczamy
    if cmd
        .member
        .as_ref()
        .and_then(|m| m.permissions)
        .map(|p| p.administrator())
        .unwrap_or(false)
    {
        return true;
    }
    // Jeśli nie admin: sprawdź role dozwolone przez ENV
    let allowed = allowed_roles();
    if allowed.is_empty() {
        // brak skonfigurowanej whitelisty => tylko admini
        return false;
    }
    // członek musi mieć co najmniej jedną z ról
    match &cmd.member {
        Some(member) => member.roles.iter().any(|rid| allowed.contains(rid)),
        None => false,
    }
}

// =====================
// Pomocnicze (parsowanie)
// =====================

fn sub_items(sub: &CommandDataOption) -> Option<&[CommandDataOption]> {
    match &sub.value {
        CommandDataOptionValue::SubCommand(v) => Some(v.as_slice()),
        CommandDataOptionValue::SubCommandGroup(v) => Some(v.as_slice()),
        _ => None,
    }
}

pub fn parse_user(
    sub: &CommandDataOption,
    name: &str,
    cmd: &CommandInteraction
) -> Option<User> {
    let items = sub_items(sub)?;
    items.iter().find_map(|o| {
        if o.name == name {
            match &o.value {
                CommandDataOptionValue::User(uid) => cmd.data.resolved.users.get(uid).cloned(),
                _ => None,
            }
        } else {
            None
        }
    })
}

pub fn parse_integer(sub: &CommandDataOption, name: &str) -> Option<i64> {
    let items = sub_items(sub)?;
    items.iter().find_map(|o| {
        if o.name == name {
            match o.value {
                CommandDataOptionValue::Integer(i) => Some(i), // bez *i
                _ => None,
            }
        } else {
            None
        }
    })
}

fn parse_user_amount(sub: &CommandDataOption, cmd: &CommandInteraction) -> Result<(User, i64)> {
    let user = parse_user(sub, "gracz", cmd).ok_or_else(|| anyhow!("Nie podano gracza"))?;
    let amount = parse_integer(sub, "kwota").ok_or_else(|| anyhow!("Nie podano kwoty"))?;
    Ok((user, amount))
}

// =====================
// DB operacje (zwracają saldo)
// =====================

/// Modyfikuje saldo o `change` (może być ujemne). Nie pozwala spaść poniżej 0.
async fn modify_balance(db: &PgPool, user_id: i64, change: i64) -> Result<i64> {
    let row = sqlx::query(
        r#"
        INSERT INTO users (id, balance)
        VALUES ($1, GREATEST(0, $2))
        ON CONFLICT (id) DO UPDATE
        SET balance = GREATEST(0, users.balance + $2)
        RETURNING balance
        "#,
    )
    .bind(user_id)
    .bind(change)
    .fetch_one(db)
    .await?;

    Ok(row.get::<i64, _>("balance"))
}

/// Ustawia saldo dokładnie na `new_balance` (przycina do ≥ 0).
async fn set_balance(db: &PgPool, user_id: i64, new_balance: i64) -> Result<i64> {
    let nb = new_balance.max(0);
    let row = sqlx::query(
        r#"
        INSERT INTO users (id, balance)
        VALUES ($1, $2)
        ON CONFLICT (id) DO UPDATE SET balance = EXCLUDED.balance
        RETURNING balance
        "#,
    )
    .bind(user_id)
    .bind(nb)
    .fetch_one(db)
    .await?;

    Ok(row.get::<i64, _>("balance"))
}

async fn reset_cooldowns(db: &PgPool, user_id: i64) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE users
        SET last_work  = NULL,
            last_daily = NULL,
            last_slut  = NULL,
            last_crime = NULL,
            last_rob   = NULL
        WHERE id = $1
        "#,
    )
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(())
}

// =====================
// Odpowiedzi
// =====================

async fn edit_response(ctx: &Context, cmd: &CommandInteraction, msg: &str) -> Result<()> {
    cmd.edit_response(
        &ctx.http,
        EditInteractionResponse::new()
            .content(msg)
            .allowed_mentions(CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}

// =====================
// Logowanie do kanału (best-effort, z cache ID)
// =====================

fn spawn_log(
    ctx: Context,
    cmd: CommandInteraction,
    action: String,
    target: Option<&User>,
    amount: Option<i64>,
    result: Option<String>,
) {
    if let Some(ch) = log_channel_id() {
        let _target_id = target.map(|u| u.id.get());
        let target_mention = target
            .map(|u| format!("<@{}>", u.id.get()))
            .unwrap_or_else(|| "—".to_string());
        let amount_s = amount.map(|a| a.to_string()).unwrap_or_else(|| "—".to_string());

        let invoker = format!("<@{}>", cmd.user.id.get());
        let guild_s = cmd.guild_id.map(|g| g.get().to_string()).unwrap_or_else(|| "DM".into());
        let channel_s = cmd.channel_id.get().to_string();
        let now_unix = Utc::now().timestamp();
        let action_owned = action;
        let result_owned = result;

        tokio::spawn({
    // sklonuj to, co potrzeba do taska
    let http = ctx.http.clone();
    let ch = ch;
    let action_owned = action_owned.clone();
    let invoker = invoker.clone();
    let target_mention = target_mention.clone();
    let amount_s = amount_s.clone();
    let guild_s = guild_s.clone();
    let channel_s = channel_s.clone();
    let result_owned = result_owned.clone();
    let now_unix = now_unix;

    async move {
        // zbuduj embed krokami (Serenity 0.12 konsumuje self)
        let mut e = CreateEmbed::new()
            .title("📜 Log: /admcontrol")
            .field("Komenda", action_owned, true)
            .field("Wykonujący", invoker, true)
            .field("Cel", target_mention, true)
            .field("Kwota", amount_s, true)
            .field("Guild", guild_s, true)
            .field("Kanał", channel_s, true);

        if let Some(r) = result_owned {
            e = e.field("Wynik", r, false);
        }

        e = e
            .footer(CreateEmbedFooter::new(format!(
                "czas: <t:{now_unix}:F> • <t:{now_unix}:R>"
            )))
            .timestamp(chrono::Utc::now());

        let m = CreateMessage::new()
            .allowed_mentions(CreateAllowedMentions::new())
            .embed(e);

        let _ = ch.send_message(&http, m).await;
    }
});
    }
}
