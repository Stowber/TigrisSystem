// src/commands/slut.rs

use anyhow::{Context as AnyhowContext, Result};
use chrono::{DateTime, Duration, Utc};
use once_cell::sync::OnceCell as SyncOnceCell;
use rand::{rng, Rng};
use serenity::all::*;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateCommand, CreateEmbed, CreateEmbedAuthor,
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    EditInteractionResponse,
};
use sqlx::PgPool;

use num_format::{Locale, ToFormattedString};
use tokio::sync::OnceCell as AsyncOnceCell;

use crate::utils::log_action;

// ========================
// ⚙️ Konfiguracja
// ========================

/// Cooldown w sekundach
const CD_SECS: i64 = 30;
/// 📱 Numer – rzadki drop
const RARE_DROP_BONUS: i64 = 150;
/// Minimalna/maksymalna reputacja
const REP_MIN: i32 = -100;
const REP_MAX: i32 = 100;

/// Ile „części szansy” daje 1 punkt reputacji (±0.08)
const REP_POINT_BONUS: f32 = 0.0008;

// Cache kanału logów
static LOG_CHAN: SyncOnceCell<Option<ChannelId>> = SyncOnceCell::new();
fn log_channel_id() -> Option<ChannelId> {
    *LOG_CHAN.get_or_init(|| {
        let id = std::env::var("LOG_CHANNEL_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        (id != 0).then(|| ChannelId::new(id))
    })
}

// ensure_schema tylko raz
static ENSURE_SCHEMA_ONCE: AsyncOnceCell<()> = AsyncOnceCell::const_new();

// ========================
// 🧾 Rejestracja
// ========================

pub fn register() -> CreateCommand {
    CreateCommand::new("slut").description("Flirtuj — różne style, różne ryzyko 💋")
}

// ========================
// 🎭 Style flirtu
// ========================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Approach {
    Gentle,  // stabilne
    Daring,  // balans
    Chaotic, // hazard
}

impl Approach {
    const GENTLE_ID: &str = "slut:gentle";
    const DARING_ID: &str = "slut:daring";
    const CHAOTIC_ID: &str = "slut:chaotic";

    fn id(self) -> &'static str {
        match self {
            Self::Gentle => Self::GENTLE_ID,
            Self::Daring => Self::DARING_ID,
            Self::Chaotic => Self::CHAOTIC_ID,
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            Self::GENTLE_ID => Some(Self::Gentle),
            Self::DARING_ID => Some(Self::Daring),
            Self::CHAOTIC_ID => Some(Self::Chaotic),
            _ => None,
        }
    }

    fn emoji(self) -> &'static str {
        match self {
            Self::Gentle => "💐",
            Self::Daring => "🔥",
            Self::Chaotic => "🎭",
        }
    }
}

// ========================
// ▶️ /slut – wybór stylu
// ========================

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    ENSURE_SCHEMA_ONCE
        .get_or_try_init(|| async { ensure_schema(db).await?; Ok::<(), anyhow::Error>(()) })
        .await?;

    if let Some(rem) = current_cd(db, cmd.user.id.get() as i64).await? {
        let emb = build_cd_embed(&cmd.user, rem);
        cmd.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embed(emb),
            ),
        )
        .await?;
        return Ok(());
    }

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .content(format!(
                    "{}, wybierz styl flirtu:\n> **Tip:** Reputacja zmienia szansę powodzenia (od -8% do +8%).",
                    cmd.user.mention()
                ))
                .components(vec![build_choice_row()]),
        ),
    )
    .await?;

    Ok(())
}

fn build_cd_embed(user: &User, remaining: i64) -> CreateEmbed {
    let ends = Utc::now() + Duration::seconds(remaining.max(0));
    let ts = ends.timestamp();

    CreateEmbed::new()
        .color(0xFF66CC)
        .author(
            CreateEmbedAuthor::new(&user.name)
                .icon_url(user.avatar_url().unwrap_or_default()),
        )
        .title("💋 Odpocznij chwilkę…")
        .description(format!(
            "{}\nSpróbuj ponownie **<t:{ts}:T>** • <t:{ts}:R>.",
            user.mention()
        ))
        .field(
            "⏳ Pozostało",
            format!("`{:02}:{:02}`", remaining / 60, remaining % 60),
            true,
        )
        .timestamp(Utc::now())
}

// ========================
// 🧩 Obsługa przycisków
// ========================

pub async fn handle_component(
    ctx: &Context,
    ic: &ComponentInteraction,
    db: &PgPool,
) -> Result<()> {
    let Some(style) = Approach::from_id(&ic.data.custom_id) else {
        return Ok(());
    };

    let user = &ic.user;
    let uid_u64 = user.id.get();
    let uid_i64 = uid_u64 as i64;

    // CD/expired?
    if let Some(_rem) = current_cd(db, uid_i64).await? {
        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("⏳ Ta interakcja wygasła — użyj ponownie `/slut`."),
            ),
        )
        .await?;
        return Ok(());
    }

    // wynik flirtu
    let out = match process_flirt(db, uid_i64, style).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("process_flirt error: {e:?}");
            ic.create_response(
                &ctx.http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .content(format!("❌ Wystąpił błąd: {e}")),
                ),
            )
            .await?;
            return Ok(());
        }
    };

    // embed wynikowy
    ic.create_response(
        &ctx.http,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embeds(vec![outcome_embed_ultra(user, &out, style)])
                .components(vec![]),
        ),
    )
    .await?;

    // log kanałowy (w tle)
    if let Some(ch) = log_channel_id() {
        let http = ctx.http.clone();
        let u_c = user.clone();
        let out_c = out.clone();
        tokio::spawn(async move {
            let _ = send_log(http, ch, &u_c, &out_c).await;
        });
    }

    // log do DB (w tle)
    {
        let db_c = db.clone();
        let desc = format!(
            "{} {} | rep {:+} (Δ {:+}) | streak {}",
            style.emoji(),
            if out.success { "sukces" } else { "porażka" },
            out.rep_after,
            out.rep_delta,
            out.streak_after
        );
        tokio::spawn(async move {
            let _ = log_action(&db_c, uid_u64, "slut", None, Some(out.amount), Some(&desc)).await;
        });
    }

    // Po cooldownie — gotowy embed + przyciski
    {
        let ctx_c = ctx.clone();
        let ic_c = ic.clone();
        let u_c = user.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(CD_SECS as u64)).await;
            let (ready_embed, rows) = build_cd_ready_with_buttons(&u_c);
            let _ = ic_c
                .edit_response(
                    &ctx_c.http,
                    EditInteractionResponse::new().embed(ready_embed).components(rows),
                )
                .await;
        });
    }

    Ok(())
}

// ========================
// 🗄️ DB & logika
// ========================

#[derive(sqlx::FromRow)]
struct UserRow {
    balance: i64,
    last_slut: Option<DateTime<Utc>>,
    flirt_rep: i32,
    flirt_streak: i32,
    flirt_fails: i32,
}

#[derive(Clone)]
struct Outcome {
    success: bool,
    amount: i64,     // suma końcowa
    work_part: i64,  // baza * multiplikator (lub strata)
    flat_bonus: i64, // stały bonus TK (w tym rare drop)
    rare_drop: bool, // czy wpadł „📱 Numer”
    message: String,
    rep_delta: i32,
    rep_after: i32,
    streak_after: i32,
    multiplier: f32,
    now: DateTime<Utc>,
    balance_after: i64,
}

async fn ensure_schema(db: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users(
          id BIGINT PRIMARY KEY,
          balance BIGINT NOT NULL DEFAULT 0,
          last_work TIMESTAMPTZ,
          last_slut TIMESTAMPTZ,
          flirt_rep INTEGER NOT NULL DEFAULT 0,
          flirt_streak INTEGER NOT NULL DEFAULT 0,
          flirt_fails INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )
    .execute(db)
    .await
    .context("creating users table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS logs(
          id BIGSERIAL PRIMARY KEY,
          user_id BIGINT NOT NULL,
          action TEXT NOT NULL,
          amount BIGINT,
          message TEXT,
          meta JSONB,
          created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        "#,
    )
    .execute(db)
    .await
    .ok();

    Ok(())
}

async fn current_cd(db: &PgPool, uid: i64) -> Result<Option<i64>> {
    let rem: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT GREATEST(0, $1 - EXTRACT(EPOCH FROM (now() - last_slut))::BIGINT)
          FROM users WHERE id = $2
        "#,
    )
    .bind(CD_SECS)
    .bind(uid)
    .fetch_optional(db)
    .await?;
    Ok(rem.filter(|v| *v > 0))
}

fn rep_bonus_percent(rep: i32) -> f32 {
    (rep as f32 * REP_POINT_BONUS * 100.0).clamp(-8.0, 8.0)
}

#[inline]
fn clamp_rep(r: i32) -> i32 {
    r.clamp(REP_MIN, REP_MAX)
}

#[inline]
fn streak_mult(streak: i32) -> f32 {
    (1.0 + (streak as f32) * 0.06).clamp(1.0, 1.60)
}

fn base_for(style: Approach, rng: &mut impl Rng) -> (i64, i32, &'static str) {
    match style {
        Approach::Gentle => (rng.random_range(30..=80) as i64, 2, "Delikatny urok działa."),
        Approach::Daring => (rng.random_range(50..=140) as i64, 3, "Śmiały krok robi wrażenie."),
        Approach::Chaotic => (rng.random_range(0..=200) as i64, 4, "Chaotyczna energia przyciąga."),
    }
}

fn success_chance(style: Approach, rep: i32) -> f32 {
    let base = match style {
        Approach::Gentle => 0.70,
        Approach::Daring => 0.55,
        Approach::Chaotic => 0.45,
    };
    let rep_boost = (rep as f32) * REP_POINT_BONUS;
    (base + rep_boost).clamp(0.10, 0.95)
}

fn series_bonus(streak_after: i32) -> i64 {
    match streak_after {
        s if s >= 15 => 50,
        s if s >= 10 => 25,
        s if s >= 5 => 10,
        _ => 0,
    }
}

fn style_tweak(style: Approach) -> f32 {
    match style {
        Approach::Gentle => 0.95,
        Approach::Daring => 1.00,
        Approach::Chaotic => 1.05,
    }
}

fn rep_bar(rep: i32) -> String {
    let width = 10;
    let normalized = ((rep - REP_MIN) as f32) / ((REP_MAX - REP_MIN) as f32);
    let fill = (normalized * width as f32).round() as i32;
    bar("💞", fill, width) + &format!(" | {:+}", rep)
}

async fn process_flirt(db: &PgPool, uid: i64, style: Approach) -> Result<Outcome> {
    let mut tx = db.begin().await?;

    // insert jeśli brak
    sqlx::query("INSERT INTO users(id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(uid)
        .execute(&mut *tx)
        .await?;

    let mut u: UserRow = sqlx::query_as(
        r#"
        SELECT balance, last_slut, flirt_rep, flirt_streak, flirt_fails
          FROM users
         WHERE id = $1
         FOR UPDATE
        "#,
    )
    .bind(uid)
    .fetch_one(&mut *tx)
    .await?;

    let now = Utc::now();
    if let Some(last) = u.last_slut {
        if (now - last).num_seconds() < CD_SECS {
            tx.rollback().await?;
            return Ok(Outcome {
                success: false,
                amount: 0,
                work_part: 0,
                flat_bonus: 0,
                rare_drop: false,
                message: "⏳ Cooldown jeszcze trwa…".into(),
                rep_delta: 0,
                rep_after: u.flirt_rep,
                streak_after: u.flirt_streak,
                multiplier: 1.0,
                now,
                balance_after: u.balance,
            });
        }
    }

    // Losowania w krótkim bloku
    let (success, base_tk, rep_succ, base_msg, rare) = {
        let mut r = rng();
        let pity = u.flirt_fails >= 3;
        let chance = if pity { 1.0 } else { success_chance(style, u.flirt_rep) };
        let success = r.random_bool(chance as f64);
        let (base_tk, rep_succ, base_msg) = base_for(style, &mut r);
        let rare = success && r.random_bool(0.03);
        (success, base_tk, rep_succ, base_msg, rare)
    };

    // Obliczenia bez RNG
    let rep_fail = -3;
    let rep_delta = if success { rep_succ } else { rep_fail };
    let streak_after = if success { u.flirt_streak + 1 } else { 0 };
    let mult = if success { streak_mult(streak_after) } else { 1.0 };

    let rare_bonus = if rare { RARE_DROP_BONUS } else { 0 };
    let s_bonus = if success { series_bonus(streak_after) } else { 0 };

    let work_part = if success {
        ((base_tk as f32) * mult * style_tweak(style)).round() as i64
    } else {
        -(base_tk / 2)
    };

    let flat_bonus = s_bonus + rare_bonus;
    let amount = work_part + flat_bonus;

    // update usera
    u.balance += amount;
    u.last_slut = Some(now);
    u.flirt_rep = clamp_rep(u.flirt_rep + rep_delta);
    u.flirt_streak = streak_after;
    u.flirt_fails = if success { 0 } else { u.flirt_fails + 1 };

    sqlx::query(
        r#"
        UPDATE users
           SET balance=$2, last_slut=$3, flirt_rep=$4, flirt_streak=$5, flirt_fails=$6
         WHERE id=$1
        "#,
    )
    .bind(uid)
    .bind(u.balance)
    .bind(now)
    .bind(u.flirt_rep)
    .bind(u.flirt_streak)
    .bind(u.flirt_fails)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let message = if success {
        format!("{} {} Zarobiłeś **{} TK**.", style.emoji(), base_msg, work_part)
    } else {
        "Próba nie wyszła. Wystawka do wiatru…".into()
    };

    Ok(Outcome {
        success,
        amount,
        work_part,
        flat_bonus,
        rare_drop: rare,
        message,
        rep_delta,
        rep_after: u.flirt_rep,
        streak_after,
        multiplier: mult,
        now,
        balance_after: u.balance,
    })
}

// ========================
// 🔘 Helpery z przyciskami i formaty
// ========================

fn build_choice_row() -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new(Approach::GENTLE_ID)
            .label("💐 Delikatne")
            .style(ButtonStyle::Primary),
        CreateButton::new(Approach::DARING_ID)
            .label("🔥 Śmiałe")
            .style(ButtonStyle::Success),
        CreateButton::new(Approach::CHAOTIC_ID)
            .label("🎭 Chaotyczne")
            .style(ButtonStyle::Danger),
    ])
}

fn build_cd_ready_with_buttons(user: &User) -> (CreateEmbed, Vec<CreateActionRow>) {
    let embed = CreateEmbed::new()
        .color(0x2ECC71)
        .author(
            CreateEmbedAuthor::new(&user.name).icon_url(user.avatar_url().unwrap_or_default()),
        )
        .title("✅ Cooldown zakończony")
        .description(format!("{} – wybierz styl flirtu:", user.mention()))
        .timestamp(Utc::now());
    (embed, vec![build_choice_row()])
}

fn fmt_tk(n: i64) -> String {
    format!("{} TK", n.to_formatted_string(&Locale::pl))
}

fn bar(label: &str, filled: i32, width: i32) -> String {
    let f = filled.clamp(0, width);
    let empty = (width - f).max(0);
    format!("{} {}{}", label, "▰".repeat(f as usize), "▱".repeat(empty as usize))
}

fn streak_bar(streak: i32) -> String {
    let width = 10;
    bar("🔥", streak.clamp(0, width), width) + &format!(" | {}", streak)
}

/// Zwraca (baza, extra z mnożnika), tak by „baza ± extra = work_part”.
fn split_base_and_extra(work_part: i64, mult: f32) -> (i64, i64) {
    if mult <= 0.0 {
        return (work_part, 0);
    }
    // work_part ≈ round(base * mult) ⇒ base ≈ round(work_part / mult)
    let base = ((work_part as f32) / mult).round() as i64;
    let extra = work_part - base;
    (base, extra)
}

/// Jednolite „Rozbicie” dla wszystkich embedów i logów.
fn format_breakdown(o: &Outcome) -> String {
    let (base_tk, extra_from_mult) = split_base_and_extra(o.work_part, o.multiplier);
    let extra_sign = if extra_from_mult >= 0 { "+" } else { "-" };
    let extra_abs = extra_from_mult.abs();

    let mut s = format!(
        "z flirtu {} {}{} (mnożnik x{:.2})",
        fmt_tk(base_tk),
        extra_sign,
        fmt_tk(extra_abs),
        o.multiplier
    );

    if o.flat_bonus > 0 {
        s.push_str(&format!("  •  bonus +{}", fmt_tk(o.flat_bonus)));
    }
    if o.rare_drop {
        s.push_str("  •  📱 Numer");
    }
    s
}

// ========================
// 🎨 Główny embed wyniku
// ========================

fn outcome_embed_ultra(user: &User, o: &Outcome, style: Approach) -> CreateEmbed {
    let next_at = o.now + Duration::seconds(CD_SECS);
    let next_unix = next_at.timestamp();
    let remain = (next_at - o.now).num_seconds().max(0);

    let color = if o.success { 0x00C853 } else { 0xD50000 };
    let chance_now = success_chance(style, o.rep_after) * 100.0;

    let amount_big = if o.amount >= 0 {
        format!("**+{}**", fmt_tk(o.amount))
    } else {
        format!("**{}**", fmt_tk(o.amount))
    };

    CreateEmbed::new()
        .color(color)
        .author(
            CreateEmbedAuthor::new(&user.name).icon_url(user.avatar_url().unwrap_or_default()),
        )
        .title(if o.success {
            format!("💋 Udane: {}", style_name(style))
        } else {
            format!("💔 Odmowa: {}", style_name(style))
        })
        // górny panel
        .field("💰 Wynik", amount_big, true)
        .field("💳 Saldo", format!("**{}**", fmt_tk(o.balance_after)), true)
        .field("🎯 Szansa", format!("{:.1}%", chance_now), true)
        // paski
        .field(
            "💞 Reputacja",
            format!(
                "{}\nΔ {:+} • wpływ: {:+.1}%",
                rep_bar(o.rep_after),
                o.rep_delta,
                rep_bonus_percent(o.rep_after)
            ),
            false,
        )
        .field(
            "🔥 Seria",
            format!("{}\n×{:.2}", streak_bar(o.streak_after), o.multiplier),
            false,
        )
        // opis i rozbicie
        .description(format!("{}\n> {}", user.mention(), o.message))
        .field("🧮 Rozbicie", format_breakdown(o), false)
        .field(
            "⏳ Cooldown",
            format!("**<t:{next_unix}:R>** • `{:02}:{:02}`", remain / 60, remain % 60),
            true,
        )
        .footer(CreateEmbedFooter::new(format!(
            "Styl: {} • +{} rep przy sukcesie, porażka −3",
            style_name(style),
            rep_gain_for(style)
        )))
        .timestamp(o.now)
}

fn style_name(style: Approach) -> &'static str {
    match style {
        Approach::Gentle  => "Delikatne",
        Approach::Daring  => "Śmiałe",
        Approach::Chaotic => "Chaotyczne",
    }
}

fn rep_gain_for(style: Approach) -> i32 {
    match style {
        Approach::Gentle => 2,
        Approach::Daring => 3,
        Approach::Chaotic => 4,
    }
}

// ========================
// 🛰️ Log kanałowy
// ========================

async fn send_log(
    http: std::sync::Arc<serenity::http::Http>,
    ch: ChannelId,
    user: &User,
    o: &Outcome,
) -> Result<()> {
    let emb = CreateEmbed::new()
        .title("💋 Log flirtu (/slut)")
        .description(format!(
            "{} — {}",
            user.mention(),
            if o.success { "sukces" } else { "porażka" }
        ))
        .color(if o.success { 0xFF66CC } else { 0xAA336A })
        .field(
            "👤 Użytkownik",
            format!("{} (`{}`)\n{}", user.tag(), user.id.get(), user.mention()),
            true,
        )
        .field(
            if o.amount >= 0 { "💰 Wypłata" } else { "💸 Strata" },
            format!("**{}**", fmt_tk(o.amount)),
            true,
        )
        .field("📈 Detale", format_breakdown(o), false)
        .field("🔥 Seria", format!("{}", o.streak_after), true)
        .field(
            "💞 Reputacja",
            format!(
                "{:+} → {:+} (Δ {:+})",
                o.rep_after - o.rep_delta,
                o.rep_after,
                o.rep_delta
            ),
            true,
        )
        .footer(CreateEmbedFooter::new("Zalogowano przez system Tigrus™"))
        .timestamp(Utc::now());

    ch.send_message(&http, CreateMessage::new().embed(emb))
        .await
        .context("sending log message")?;
    Ok(())
}
