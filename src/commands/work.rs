// src/commands/work.rs

#![allow(dead_code)] // Tymczasowo: wycisza ostrzeżenia „never used”, dopóki komenda nie jest wywoływana z main/lib.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Duration, Utc};
use once_cell::sync::{Lazy, OnceCell as SyncOnceCell};
use rand::Rng;
use serde::Deserialize;
use serenity::all::*;
use serenity::builder::{CreateCommand, CreateInteractionResponseMessage};
use sqlx::PgPool;

use num_format::{Locale, ToFormattedString};
use tokio::sync::OnceCell as AsyncOnceCell;

use crate::utils::log_action;

// ========================
// ⚙️ Konfiguracja i dane
// ========================

const TEXTS_JSON: &str = include_str!("../../texts.json");
const COOLDOWN_SECS: i64 = 30;

// stałe dla custom_id przycisków
const BTN_SAFE: &str = "work:choose:safe";
const BTN_BALANCED: &str = "work:choose:balanced";
const BTN_HIGH: &str = "work:choose:high";

#[derive(Debug, Clone, Deserialize)]
struct WorkTask {
    place: String,
    text: String, // powinien zawierać opcjonalny placeholder {amount}
}

#[derive(Debug, Clone, Deserialize)]
struct TextsRoot {
    work_tasks: Vec<WorkTask>,
}

static WORK_TASKS: Lazy<Vec<WorkTask>> = Lazy::new(|| {
    let parsed: TextsRoot =
        serde_json::from_str(TEXTS_JSON).expect("Błędny JSON w texts.json (oczekiwano { work_tasks: [...] })");
    assert!(
        !parsed.work_tasks.is_empty(),
        "texts.json: work_tasks nie może być puste"
    );
    parsed.work_tasks
});

// Cache kanału logów z ENV (None jeśli brak/0)
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

// Jednorazowe ensure_schema na proces
static ENSURE_SCHEMA_ONCE: AsyncOnceCell<()> = AsyncOnceCell::const_new();

// ========================
// 🧾 Rejestracja komendy
// ========================

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("work")
        .description("Pracuj, aby zdobyć trochę TK 😊");
    cmd
}

// ========================
// 🔀 Kontrakty pracy
// ========================

#[derive(Debug, Clone, Copy)]
enum WorkChoice {
    Safe,      // stała wypłata low
    Balanced,  // średnia z lekkim ryzykiem
    HighRisk,  // wysoka z dużym ryzykiem
}

impl WorkChoice {
    fn from_custom_id(s: &str) -> Option<Self> {
        match s {
            BTN_SAFE => Some(Self::Safe),
            BTN_BALANCED => Some(Self::Balanced),
            BTN_HIGH => Some(Self::HighRisk),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Safe => "Bezpieczny",
            Self::Balanced => "Zbalansowany",
            Self::HighRisk => "Wysokie ryzyko",
        }
    }
}

// ========================
// ▶️ Główna komenda
// ========================

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    // Schemat odpalany tylko raz
    ENSURE_SCHEMA_ONCE
        .get_or_try_init(|| async {
            ensure_schema(db).await?;
            Ok::<(), anyhow::Error>(())
        })
        .await?;

    let user = &cmd.user;

    // Sprawdź tylko cooldown – bez wypłaty jeszcze
    let cd = current_cooldown(db, user.id.get() as i64, COOLDOWN_SECS).await?;
    if cd > 0 {
        let embed = build_cooldown_embed(user, cd);
        return send_embed(ctx, cmd, embed).await;
    }

    // Pokaż wybór kontraktów
    let row = CreateActionRow::Buttons(vec![
        CreateButton::new(BTN_SAFE).label("🛡️ Bezpieczny").style(ButtonStyle::Success),
        CreateButton::new(BTN_BALANCED).label("⚖️ Zbalansowany").style(ButtonStyle::Primary),
        CreateButton::new(BTN_HIGH).label("🎲 Wysokie ryzyko").style(ButtonStyle::Danger),
    ]);

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .content(format!("{}, wybierz kontrakt pracy:", user.mention()))
                .components(vec![row])
        ),
    ).await?;

    Ok(())
}

// ========================
// 🧩 Obsługa kliknięć przycisków
// ========================

pub async fn handle_component(ctx: &Context, ic: &ComponentInteraction, db: &PgPool) -> Result<()> {
    // rozpoznaj przycisk
    let Some(choice) = WorkChoice::from_custom_id(&ic.data.custom_id) else {
        return Ok(());
    };
    let user = &ic.user;

    // szybki check cooldownu
    if current_cooldown(db, user.id.get() as i64, COOLDOWN_SECS).await? > 0 {
        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("⏳ Ten wybór wygasł — użyj ponownie `/work`."),
            ),
        ).await?;
        return Ok(());
    }

    // wynik transakcji
    let WorkOutcome { amount, message, place, new_balance, now, streak, multiplier } =
        process_work_tx(db, user.id.get() as i64, choice).await?;

    // paski + opis bonusu (prezentacja)
    let streak_total = 10;
    let filled = streak.min(streak_total);
    let streak_bar = bar(filled, streak_total);
    let bonus_bar = bonus_progress_bar(streak);
    let bonus_text = bonus_series_text(streak);

    // płaski bonus +TK z progów 5/10/15
    let extra = bonus_flat_for_tier(bonus_tier(streak));
    let work_part = amount.saturating_sub(extra); // ile z samej pracy (po mnożniku)
    let work_part_fmt = format!("{} TK", format_tk(work_part));
    let extra_fmt = format!("{} TK", format_tk(extra));
    let amount_fmt = format!("{} TK", format_tk(amount));

    // zbuduj embed zależnie od wyniku
    let mut embed = if amount == 0 {
        build_fail_embed(user, &message, &place, new_balance, now, false)
            .field(
                "🎯 Kontrakt",
                format!("{} {}", contract_emoji(Some(choice)), choice.label()),
                true,
            )
    } else {
        build_result_embed(user, amount, &message, &place, new_balance, now, false)
            .field(
                "🎯 Kontrakt",
                format!("{} {}", contract_emoji(Some(choice)), choice.label()),
                true,
            )
            .field(
                "🔥 Seria",
                format!("{} | x{:.2} (streak: {})", streak_bar, multiplier, streak),
                true,
            )
            .field(
                "🎁 Bonus serii",
                format!("{} | {}", bonus_bar, bonus_text),
                true,
            )
            .field(
                "🧮 Rozbicie wypłaty",
                format!("{} (x{:.2}) + {} = **{}**", work_part_fmt, multiplier, extra_fmt, amount_fmt),
                false, // pełna szerokość – czytelniej
)
    };

    // dopisz informację o dodatkowym +TK tylko dla udanej zmiany
    if amount > 0 && extra > 0 {
        embed = embed.field("🎁 Bonus tej zmiany", format!("**+{} TK**", extra), true);
    }

    // aktualizujemy oryginalną wiadomość (ukrywamy przyciski)
    ic.create_response(
        &ctx.http,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embeds(vec![embed])
                .components(vec![]),
        ),
    ).await?;

    // log na kanał (asynchronicznie)
    if let Some(log_ch) = log_channel_id() {
        let http = ctx.http.clone();
        let u = user.clone();
        let msg = message.clone();
        tokio::spawn(async move {
            let _ = send_log_to_channel_http(http, log_ch, &u, amount, &msg).await;
        });
    }
    {
        let db = db.clone();
        let uid = user.id.get();
        let msg = message.clone();
        tokio::spawn(async move {
            let _ = log_action(&db, uid, "work", None, Some(amount), Some(&msg)).await;
        });
    }

    // po cooldownie zaktualizuj *ephemeral* odpowiedź przez edit_response
    let ctx_clone = ctx.clone();
    let ic_clone = ic.clone();
    let user_clone = user.clone();
    let place_clone = place.clone();
    let msg_clone = message.clone();
    let choice_clone = choice;
    let amount_clone = amount;
    let new_balance_clone = new_balance;
    let streak_clone = streak;
    let multiplier_clone = multiplier;

    // ile zostało do końca CD
    let now_ts = Utc::now();
    let ready_at = now_ts + chrono::Duration::seconds(COOLDOWN_SECS);
    let sleep_secs = (ready_at - now_ts).num_seconds().max(0) as u64;

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

        let now_ready = Utc::now();

        // odśwież paski i bonus
        let streak_total = 10;
        let filled = streak_clone.min(streak_total);
        let streak_bar = bar(filled, streak_total);
        let bonus_bar = bonus_progress_bar(streak_clone);
        let bonus_text = bonus_series_text(streak_clone);
        let extra = bonus_flat_for_tier(bonus_tier(streak_clone));

        let mut updated = if amount_clone == 0 {
            build_fail_embed(&user_clone, &msg_clone, &place_clone, new_balance_clone, now_ready, true)
                .field(
                    "🎯 Kontrakt",
                    format!("{} {}", contract_emoji(Some(choice_clone)), choice_clone.label()),
                    true,
                )
        } else {
            build_result_embed(&user_clone, amount_clone, &msg_clone, &place_clone, new_balance_clone, now_ready, true)
                .field(
                    "🎯 Kontrakt",
                    format!("{} {}", contract_emoji(Some(choice_clone)), choice_clone.label()),
                    true,
                )
                .field(
                    "🔥 Seria",
                    format!("{} | x{:.2} (streak: {})", streak_bar, multiplier_clone, streak_clone),
                    true,
                )
                .field(
                    "🎁 Bonus serii",
                    format!("{} | {}", bonus_bar, bonus_text),
                    true,
                )
        };

        if amount_clone > 0 && extra > 0 {
            updated = updated.field("🎁 Bonus tej zmiany", format!("**+{} TK**", extra), true);
        }

        let _ = ic_clone
            .edit_response(&ctx_clone.http, EditInteractionResponse::new().embeds(vec![updated]))
            .await;
    });

    Ok(())
}



// ========================
// 🗄️ Schemat DB (jednorazowo)
// ========================

async fn ensure_schema(db: &PgPool) -> Result<()> {
    // users
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id          BIGINT PRIMARY KEY,
            balance     BIGINT NOT NULL DEFAULT 0,
            last_work   TIMESTAMPTZ,
            streak      INTEGER NOT NULL DEFAULT 0,
            last_streak TIMESTAMPTZ
        );
        "#,
    )
    .execute(db)
    .await?;

    // logs – przykładowa tabela (na wypadek użycia log_action)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS logs (
            id         BIGSERIAL PRIMARY KEY,
            user_id    BIGINT NOT NULL,
            action     TEXT   NOT NULL,
            amount     BIGINT,
            message    TEXT,
            meta       JSONB,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(r#"CREATE INDEX IF NOT EXISTS idx_logs_user_id ON logs(user_id);"#)
        .execute(db)
        .await?;
    sqlx::query(r#"CREATE INDEX IF NOT EXISTS idx_logs_action  ON logs(action);"#)
        .execute(db)
        .await?;

    // bezpieczne ALTER-y (jeśli tabela była starsza)
    sqlx::query(
        r#"
        ALTER TABLE users
            ADD COLUMN IF NOT EXISTS streak INTEGER NOT NULL DEFAULT 0,
            ADD COLUMN IF NOT EXISTS last_streak TIMESTAMPTZ;
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

// ========================
// 🔁 Cooldown helper
// ========================

async fn current_cooldown(db: &PgPool, user_id: i64, cooldown_secs: i64) -> Result<i64> {
    let remaining: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT GREATEST(0, $1 - EXTRACT(EPOCH FROM (now() - last_work))::BIGINT)
        FROM users WHERE id = $2
        "#,
    )
    .bind(cooldown_secs)
    .bind(user_id)
    .fetch_optional(db)
    .await?;

    Ok(remaining.unwrap_or(0))
}

// ========================
// 🔧 Helpery do pasków postępu, bonusów i emoji kontraktów
// ========================

#[inline]
fn bar(filled: i32, width: i32) -> String {
    let f = filled.clamp(0, width);
    let empty = (width - f).max(0);
    "▰".repeat(f as usize) + &"▱".repeat(empty as usize)
}

#[inline]
fn contract_emoji(choice: Option<WorkChoice>) -> &'static str {
    match choice {
        Some(WorkChoice::Safe) => "🛡️",
        Some(WorkChoice::Balanced) => "⚖️",
        Some(WorkChoice::HighRisk) => "🎲",
        None => "🧰",
    }
}

// progi bonusów
const BONUS_STEPS: [i32; 3] = [5, 10, 15];

#[inline]
fn bonus_cap() -> f32 {
    1.50
}

/// Określa mnożnik dla danej liczby streak
#[inline]
fn streak_multiplier(streak: i32) -> f32 {
    (1.0 + (streak as f32 - 1.0) * 0.05).clamp(1.0, bonus_cap())
}

/// Zwraca następny próg i jego mnożnik, jeśli jeszcze nie osiągnięto CAP
fn next_bonus_tier(streak: i32) -> Option<(i32, f32)> {
    for &t in &BONUS_STEPS {
        if streak < t {
            return Some((t, streak_multiplier(t)));
        }
    }
    None
}

/// Zwraca opis bonusu serii – pokazuje próg, mnożnik i dodatkowe TK
fn bonus_series_text(streak: i32) -> String {
    if let Some((next, mult)) = next_bonus_tier(streak) {
        let rem = (next - streak).max(0);
        let extra_tk = bonus_flat_for_tier(bonus_tier(next));
        format!(
            "następny próg: **{}** (x{:.2} + **+{} TK**) • brakuje **{}** zmian",
            next, mult, extra_tk, rem
        )
    } else {
        format!("osiągnięto CAP: **x{:.2}** + **+{} TK** na zmianę", bonus_cap(), bonus_flat_for_tier(3))
    }
}

/// Pasek postępu do najbliższego progu (co 5 udanych zmian)
fn bonus_progress_bar(streak: i32) -> String {
    let width = 5;

    // CAP: od 15 wzwyż pasek zawsze pełny
    if streak >= BONUS_STEPS[BONUS_STEPS.len() - 1] {
        return bar(width, width);
    }

    // W przeciwnym razie: pełny na 5/10, częściowy między progami
    let mut fill = streak % width;
    if fill == 0 && streak > 0 {
        fill = width;
    }
    bar(fill, width)
}

/// Zwraca numer progu bonusu
#[inline]
fn bonus_tier(streak: i32) -> u8 {
    if streak >= 15 { 3 }
    else if streak >= 10 { 2 }
    else if streak >= 5 { 1 }
    else { 0 }
}

/// Zwraca dodatkowe TK dla danego progu
#[inline]
fn bonus_flat_for_tier(tier: u8) -> i64 {
    match tier {
        1 => 10,   // od 5+
        2 => 25,   // od 10+
        3 => 50,   // od 15+
        _ => 0,
    }
}

// ========================
// 💰 Losowanie narracji
// ========================

/// Zwraca sformatowaną wiadomość i miejsce na podstawie `WORK_TASKS`,
/// wstawiając `final_amount` do placeholdera `{amount}` (jeśli wystąpi).
fn narrative_for_amount(final_amount: i64) -> (String, String) {
    let mut rng = rand::rng();
    let tasks = WORK_TASKS.as_slice();
    let idx = rng.random_range(0..tasks.len());
    let task = &tasks[idx];

    let message = task.text.replace("{amount}", &final_amount.to_string());
    (message, task.place.clone())
}

// ========================
// 👷‍♂️ Wynik pracy + transakcja
// ========================

struct WorkOutcome {
    amount: i64,
    message: String,
    place: String,
    new_balance: i64,
    now: DateTime<Utc>,
    streak: i32,
    multiplier: f32,
}

// Baza nagrody wg kontraktu (bez mnożnika)
fn generate_contract_base(choice: WorkChoice) -> (i64, &'static str) {
    let mut rng = rand::rng();
    match choice {
        WorkChoice::Safe => (rng.random_range(30..=50), "Ukończyłeś rutynowe zadania bez potknięć."),
        WorkChoice::Balanced => {
            if rng.random_bool(0.10) {
                (0, "Projekt się wykrzaczył i zamknąłeś dzień na zero.")
            } else {
                (rng.random_range(40..=90), "Dopiąłeś sprint z przyzwoitym wynikiem.")
            }
        }
        WorkChoice::HighRisk => {
            if rng.random_bool(0.10) {
                (rng.random_range(120..=200), "💥 Krytyczny sukces! Zrobiłeś robotę życia.")
            } else if rng.random_bool(0.30) {
                (0, "Ups… ryzyko nie wypaliło. Dziś nic nie zarobiłeś.")
            } else {
                (rng.random_range(60..=140), "Duży deal, duże nerwy — udało się.")
            }
        }
    }
}

// transakcja: wiersz użytkownika, cooldown, streak, update
async fn process_work_tx(db: &PgPool, user_id: i64, choice: WorkChoice) -> Result<WorkOutcome> {
    let mut tx = db.begin().await?;

    // 0) upewnij się, że user istnieje
    sqlx::query("INSERT INTO users (id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    // 1) wczytaj usera z blokadą
    #[derive(sqlx::FromRow)]
    struct UserRow {
        balance: i64,
        last_work: Option<DateTime<Utc>>,
        streak: i32,
        last_streak: Option<DateTime<Utc>>,
    }

    let user_row: UserRow = sqlx::query_as(
        r#"
        SELECT balance, last_work, streak, last_streak
        FROM users
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .context("Nie udało się pobrać użytkownika")?;

    let now = Utc::now();

    // 2) cooldown check w transakcji
    if let Some(lw) = user_row.last_work {
        if (now - lw).num_seconds() < COOLDOWN_SECS {
            tx.rollback().await?;
            return Ok(WorkOutcome {
                amount: 0,
                message: "⏳ Wciąż masz cooldown — spróbuj później.".into(),
                place: "—".into(),
                new_balance: user_row.balance,
                now,
                streak: user_row.streak,
                multiplier: 1.0,
            });
        }
    }

    // 3) baza wyniku z wyboru
let (base_amount, base_msg) = generate_contract_base(choice);
let fail = base_amount == 0;

// 4) streak
let new_streak = if fail {
    0
} else {
    match user_row.last_work {
        Some(prev) if (now - prev).num_seconds() <= 2 * 3600 => (user_row.streak + 1).max(1),
        _ => 1,
    }
};

let multiplier = if fail { 1.0 } else { streak_multiplier(new_streak) };
let tier = bonus_tier(new_streak);
let extra = bonus_flat_for_tier(tier);

let final_amount = if fail {
    0
} else {
    ((base_amount as f32) * multiplier).round() as i64 + extra
};

// 5) update usera (last_streak aktualizujemy tylko jeśli streak > 0)
let new_balance: i64 = sqlx::query_scalar(
    r#"
    UPDATE users
       SET balance = balance + $2,
           last_work = $3,
           streak    = $4,
           last_streak = CASE WHEN $4 > 0 THEN $3 ELSE last_streak END
     WHERE id = $1
 RETURNING balance
    "#,
)
.bind(user_id)
.bind(final_amount)
.bind(now)
.bind(new_streak)
.fetch_one(&mut *tx)
.await?;

    tx.commit().await?;

    // 6) narracja – zawsze wstawiaj final_amount do {amount}
    let (narrative, place) = narrative_for_amount(final_amount);
    let message = format!("{base_msg} {narrative}");

    Ok(WorkOutcome {
        amount: final_amount,
        message,
        place,
        new_balance,
        now,
        streak: new_streak,
        multiplier,
    })
}

// ========================
// 🧱 Embedy
// ========================

fn build_cooldown_embed(user: &User, remaining: i64) -> CreateEmbed {
    let ends_at = Utc::now() + Duration::seconds(remaining.max(0));
    let next_unix = ends_at.timestamp();

    CreateEmbed::new()
        .color(0xF1C40F)
        .author(CreateEmbedAuthor::new(&user.name).icon_url(user.avatar_url().unwrap_or_default()))
        .title("⏳ Cooldown")
        .field("Pozostało", format!("`{}`", fmt_mmss(remaining)), true)
        .field("Do godz.", format!("<t:{next_unix}:T> • <t:{next_unix}:R>"), true)
        .timestamp(Utc::now())
}

#[allow(dead_code)]
fn build_cooldown_ready_embed(user: &User) -> CreateEmbed {
    CreateEmbed::new()
        .color(0x2ECC71)
        .author(CreateEmbedAuthor::new(&user.name).icon_url(user.avatar_url().unwrap_or_default()))
        .title("✅ Cooldown zakończony")
        .description("Możesz już pracować — użyj `/work`.")
        .timestamp(Utc::now())
}

fn format_tk(n: i64) -> String {
    n.to_formatted_string(&Locale::pl)
}

fn fmt_mmss(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}", s / 60, s % 60)
}

pub fn build_result_embed(
    user: &User,
    amount: i64,
    msg: &str,
    place: &str,
    balance: i64,
    now: DateTime<Utc>,
    ready: bool,
) -> CreateEmbed {
    let amount_fmt = format!("{} TK", format_tk(amount));
    let balance_fmt = format!("{} TK", format_tk(balance));

    let next_at = now + Duration::seconds(COOLDOWN_SECS);
    let next_unix = next_at.timestamp();
    let remaining = (next_at - now).num_seconds().max(0);

    let (color, status) = if ready {
        (0x21D19F, "✅ **Gotowe do pracy.** Użyj `/work`!".to_string())
    } else {
        (
            0x10C6A0,
            format!("⏳ `{}` • **<t:{next_unix}:T>** • <t:{next_unix}:R>", fmt_mmss(remaining)),
        )
    };

    CreateEmbed::new()
        .color(color)
        .author(
            CreateEmbedAuthor::new(&user.name)
                .icon_url(user.avatar_url().unwrap_or_default()),
        )
        .title("✅ Zmiana zakończona")
        .description(format!("{}\n> {}", user.mention(), msg.trim()))
        .field("📍 Miejsce", place, true)
        .field("💵 Wypłata", format!("**{}**", amount_fmt), true)
        .field("💳 Saldo", format!("**{}**", balance_fmt), true)
        .field("⌛ Cooldown", status, false)
        .timestamp(now)
}

fn build_fail_embed(
    user: &User,
    msg: &str,
    place: &str,
    balance: i64,
    now: DateTime<Utc>,
    ready: bool,
) -> CreateEmbed {
    let next_at = now + Duration::seconds(COOLDOWN_SECS);
    let next_unix = next_at.timestamp();
    let remaining = (next_at - now).num_seconds().max(0);

    let (color, status) = if ready {
        (0x2ECC71, "✅ **Gotowe do pracy.** Użyj `/work`!".to_string())
    } else {
        (0xE4572E,
            format!("⏳ `{}` • **<t:{next_unix}:T>** • <t:{next_unix}:R>",
                fmt_mmss(remaining))
        )
    };

    CreateEmbed::new()
    .color(color)
    .author(CreateEmbedAuthor::new(&user.name).icon_url(user.avatar_url().unwrap_or_default()))
    .title("❌ Zmiana nieudana")
    .description(format!("{}\n> {}", user.mention(), msg.trim()))
    .field("📍 Miejsce", place, true)
    .field("💵 Wypłata", "**0 TK**", true)
    .field("💳 Saldo", format!("**{} TK**", format_tk(balance)), true)
    .field(
        "🎁 Bonus serii",
        format!("{} | {}", bar(0, 5), bonus_series_text(0)),
        true,
    )
    .field("⌛ Cooldown", status, false)
    .timestamp(now)
}

// ========================
// 📤 Komunikaty
// ========================

async fn send_embed(ctx: &Context, cmd: &CommandInteraction, embed: CreateEmbed) -> Result<()> {
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

// ========================
// 📡 Log na kanał (w tle)
// ========================

async fn send_log_to_channel_http(
    http: std::sync::Arc<serenity::http::Http>,
    channel_id: ChannelId,
    user: &User,
    amount: i64,
    message: &str,
) -> Result<()> {
    let embed = CreateEmbed::new()
        .title("🛠️ Log pracy (/work)")
        .description("Użytkownik zakończył sesję pracy i otrzymał wynagrodzenie.")
        .color(0x66CCFF)
        .thumbnail("https://cdn-icons-png.flaticon.com/512/201/201623.png")
        .field(
            "👤 Pracownik",
            format!("{} (`{}`)\n{}", user.tag(), user.id.get(), user.mention()),
            true,
        )
        .field("💰 Wynagrodzenie", format!("**{} TK**", amount), true)
        .field("📝 Opis zadania", message, false)
        .footer(CreateEmbedFooter::new("Zalogowano przez system Tigrus™"))
        .timestamp(Utc::now());

    channel_id
        .send_message(&http, CreateMessage::new().embed(embed))
        .await?;
    Ok(())
}
