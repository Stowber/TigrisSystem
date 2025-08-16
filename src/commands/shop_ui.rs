use anyhow::{Context as AnyhowContext, Result};
use chrono::{DateTime, Duration, Utc};
use once_cell::sync::OnceCell as SyncOnceCell;
use serenity::all::*;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateCommand, CreateEmbed, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, CreateSelectMenu, CreateSelectMenuKind,
    EditInteractionResponse,
};
use sqlx::{PgPool, Row};
use std::{env, fmt, num::NonZeroU64};

// =======================================
// ⚙️ Konfiguracja (cache'owana) + stałe
// =======================================

const THEME_ORANGE: u32 = 0xFF7A00; // żywy pomarańcz
const TIGER: &str = "🐯";
const CART: &str = "🛒";
const GIFT: &str = "🎁";
const PLUS: &str = "➕";
const MINUS: &str = "➖";
const CAL: &str = "🗓️";

#[derive(Clone, Copy, Debug)]
struct ShopConfig {
    role_id: RoleId,
    days_per_unit: i64,
    max_units: i64,
    price_tk: i64,
    log_channel: Option<ChannelId>,
}

static CONFIG: SyncOnceCell<ShopConfig> = SyncOnceCell::new();

fn config() -> &'static ShopConfig {
    CONFIG.get_or_init(|| {
        let role_id = env::var("SHOP_ROLE_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(RoleId::new)
            .unwrap_or_else(|| RoleId::new(1406257723774861416));

        let log_channel = env::var("LOG_CHANNEL_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .and_then(NonZeroU64::new)
            .map(|nz| ChannelId::new(nz.get()));

        let price_tk = env::var("ROLE_PRICE_TK")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(20_000);

        ShopConfig {
            role_id,
            days_per_unit: 30,
            max_units: 12,
            price_tk,
            log_channel,
        }
    })
}

// =======================================
// 🔧 Rejestracja komendy
// =======================================

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("shop")
        .description("Panel ekonomiczny: kup/przedłuż rangę premium (30 dni)");
    cmd
}

// =======================================
// 🧰 Pomocnicze
// =======================================

#[inline]
fn fmt_dt(dt: DateTime<Utc>) -> String {
    dt.format("%d-%m-%Y").to_string()
}

pub(crate) fn fmt_dt_full(dt: DateTime<Utc>) -> String {
    dt.format("%d-%m-%Y %H:%M UTC").to_string()
}

#[inline]
fn progress_bar(days_left: i32, total_days: i32) -> String {
    let segs = 10;
    let filled = ((days_left as f32 / total_days as f32) * segs as f32).round() as i32;
    let filled = filled.clamp(0, segs);
    let mut s = String::from("[");
    for i in 0..segs {
        if i < filled { s.push('▓') } else { s.push('░') }
    }
    s.push(']');
    s
}

pub(crate) async fn log_embed(http: &Http, embed: CreateEmbed) {
    if let Some(ch) = config().log_channel {
        let _ = ch.send_message(http, CreateMessage::new().embed(embed)).await;
    }
}

pub(crate) async fn dm_user(http: &Http, user_id: UserId, embed: CreateEmbed) {
    if let Ok(dm) = user_id.create_dm_channel(http).await {
        let _ = dm.id.send_message(http, CreateMessage::new().embed(embed)).await;
    }
}

async fn ensure_role_added(http: &Http, guild_id: GuildId, user_id: UserId) {
    if let Ok(member) = guild_id.member(http, user_id).await {
        if let Err(e) = member.add_role(http, config().role_id).await {
            log_embed(
                http,
                CreateEmbed::new()
                    .title("⚠️ Błąd nadawania roli")
                    .description(format!("Nie udało się nadać roli dla <@{}>: {}", user_id.get(), e))
                    .color(0xE74C3C)
                    .timestamp(Utc::now()),
            ).await;
        }
    } else {
        log_embed(
            http,
            CreateEmbed::new()
                .title("⚠️ Błąd pobrania membera")
                .description(format!("Nie udało się pobrać membera <@{}> do nadania roli.", user_id.get()))
                .color(0xE74C3C)
                .timestamp(Utc::now()),
        ).await;
    }
}

async fn ensure_role_removed(http: &Http, guild_id: GuildId, user_id: UserId) {
    if let Ok(member) = guild_id.member(http, user_id).await {
        let _ = member.remove_role(http, config().role_id).await;
    }
}

/// Nazwa roli do DM (mention ról nie działa w DM-ach).
async fn role_name_for_dm(http: &Http, guild_id: GuildId, role_id: RoleId) -> String {
    match guild_id.roles(http).await {
        Ok(map) => map.get(&role_id).map(|r| r.name.clone()).unwrap_or_else(|| format!("rola {}", role_id.get())),
        Err(_)  => format!("rola {}", role_id.get()),
    }
}

// udostępnij id roli dla lib.rs (event ręcznego zdjęcia)
pub(crate) fn role_id() -> RoleId { config().role_id }

// =======================================
// 🖼️ Render panelu
// =======================================
fn render_panel(
    owner_uid: u64,
    units: i64,
    current_expiry: Option<DateTime<Utc>>,
) -> (CreateEmbed, CreateActionRow, CreateActionRow) {
    let cfg = config();
    let price = cfg.price_tk;
    let total = price.saturating_mul(units);

    let status_line = if let Some(exp) = current_expiry {
        let days_left = (exp - Utc::now()).num_days().max(0);
        let bar = progress_bar(days_left as i32, cfg.days_per_unit as i32);
        format!(
            "**Status:** aktywna do **{}**\n{} **{}/{} dni**",
            fmt_dt(exp),
            bar,
            days_left,
            cfg.days_per_unit
        )
    } else {
        "**Status:** brak aktywnej subskrypcji".to_string()
    };

    let embed = CreateEmbed::new()
        .title(format!("{TIGER} Tigris Kalwaryjski — 30 dni"))
        .description(format!(
            "{TIGER} Odpal pazury premium na swoim koncie.\n\
             {CAL} Jedna jednostka = **30 dni**. Pakiety się **stackują** – kup kilka naraz i przedłużaj z góry."
        ))
        .field("Ranga", format!("{TIGER} <@&{}>", cfg.role_id.get()), true)
        .field("Cena", format!("**{} TK** / 30 dni", price), true)
        .field("Wybrano", format!("**{}×** 30 dni ⇒ **{} TK**", units, total), false)
        .field("Twój stan", status_line, false)
        .color(THEME_ORANGE)
        .timestamp(Utc::now());

    // 🔢 Zmiana ilości (30 dni)
    let row_qty = CreateActionRow::Buttons(vec![
        CreateButton::new(format!("shop|{}|qty|{}|op|dec", owner_uid, units))
            .label(format!("{MINUS} 30 dni"))
            .style(ButtonStyle::Secondary),
        CreateButton::new(format!("shop|{}|qty|{}|op|inc", owner_uid, units))
            .label(format!("{PLUS} 30 dni"))
            .style(ButtonStyle::Secondary),
    ]);

    // 🛒 Akcje
    let row_actions = CreateActionRow::Buttons(vec![
        CreateButton::new(format!("shop|{}|qty|{}|op|buy", owner_uid, units))
            .label(format!("{CART} Kup"))
            .style(ButtonStyle::Success),
        CreateButton::new(format!("shop|{}|qty|{}|op|gift", owner_uid, units))
            .label(format!("{GIFT} Podaruj"))
            .style(ButtonStyle::Primary),
    ]);

    (embed, row_qty, row_actions)
}

// =======================================
// 🚀 Obsługa komendy
// =======================================

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    ensure_schema(db).await?;
    if let Some(gid) = cmd.guild_id {
        let _ = expire_roles_tick(ctx, db, gid).await;
    }

    let opener_id = cmd.user.id.get();
    let units = 1i64;

    let current_exp = if let Some(gid) = cmd.guild_id {
        get_current_expiry(
            db,
            opener_id as i64,
            config().role_id.get() as i64,
            gid.get() as i64,
        )
        .await?
    } else {
        None
    };

    let (embed, row_qty, row_actions) = render_panel(opener_id, units, current_exp);

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embed(embed)
                .components(vec![row_qty, row_actions]),
        ),
    )
    .await
    .context("Nie udało się wysłać panelu `/shop`")?;

    Ok(())
}

// =======================================
// 🧩 Parsowanie akcji z custom_id
// =======================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelOp { Inc, Dec, Buy, Gift }

impl fmt::Display for PanelOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self { PanelOp::Inc => "inc", PanelOp::Dec => "dec", PanelOp::Buy => "buy", PanelOp::Gift => "gift" })
    }
}

fn parse_panel_action(custom_id: &str) -> Option<(u64, i64, PanelOp)> {
    let mut it = custom_id.split('|');
    if it.next()? != "shop" { return None; }
    let owner = it.next()?.parse::<u64>().ok()?;
    if it.next()? != "qty" { return None; }
    let units = it.next()?.parse::<i64>().ok()?;
    if it.next()? != "op" { return None; }
    let op = match it.next()? { "inc" => PanelOp::Inc, "dec" => PanelOp::Dec, "buy" => PanelOp::Buy, "gift" => PanelOp::Gift, _ => return None };
    Some((owner, units, op))
}

// =======================================
// 🧩 Obsługa komponentów (+ potwierdzenie podarunku)
// =======================================

pub async fn handle_component(ctx: &Context, ic: &ComponentInteraction, db: &PgPool) -> Result<()> {
    let cfg = config();
    let cid = ic.data.custom_id.as_str();

    if !(cid.starts_with("shop|") || cid.starts_with("shopgift|")) {
        return Ok(());
    }

    // --- [NOWE] Potwierdzenie podarunku ---
    // format: "shop|{owner}|qty|{units}|op|giftconfirm|to|{target_id}"
    if cid.starts_with("shop|") && cid.contains("|op|giftconfirm|") {
        let mut it = cid.split('|');
        let _ = it.next(); // "shop"
        let owner = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or_default();
        let _ = it.next(); // "qty"
        let units = it.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(1);
        let _ = it.next(); // "op"
        let _ = it.next(); // "giftconfirm"
        let _ = it.next(); // "to"
        let target_id_u64 = it.next().and_then(|s| s.parse::<u64>().ok());

        // tylko właściciel panelu może potwierdzić
        if ic.user.id.get() != owner || target_id_u64.is_none() {
            return Ok(());
        }
        let target_id_u64 = target_id_u64.unwrap();
        let Some(guild_id) = ic.guild_id else {
            return Ok(());
        };

        ic.defer(&ctx.http).await?;

        let units = units.clamp(1, cfg.max_units);
        let price = cfg.price_tk;
        let total = price.saturating_mul(units);

        match buy_role_tx(
            db,
            ic.user.id.get() as i64,
            target_id_u64 as i64,
            units,
            total,
            cfg.role_id.get() as i64,
            guild_id.get() as i64,
        ).await? {
            BuyRoleResult::Ok { buyer_balance, new_expires_at } => {
                ensure_role_added(&ctx.http, guild_id, UserId::new(target_id_u64)).await;

                // DM do obdarowanego
                let role_name = role_name_for_dm(&ctx.http, guild_id, cfg.role_id).await;
                let giver = &ic.user;
                let mut emb = CreateEmbed::new()
                    .title("🎁 Podarowano Ci rangę")
                    .description(format!(
                        "Masz dostęp do przywilejów rangi **{}**.\nMiłej zabawy i powodzenia na serwerze!",
                        role_name
                    ))
                    .field("Nadawca", format!("{} (`{}`)", giver.tag(), giver.id.get()), true)
                    .field("Ranga", role_name.clone(), true)
                    .field("Pakiet", format!("{}× 30 dni", units), true)
                    .field("Ważna do", fmt_dt_full(new_expires_at), false)
                    .color(THEME_ORANGE)
                    .timestamp(Utc::now());
                if let Some(avatar) = giver.avatar_url() {
                    emb = emb.thumbnail(avatar);
                }
                dm_user(&ctx.http, UserId::new(target_id_u64), emb).await;

                // potwierdzenie w UI (bez przycisków)
                ic.edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .embed(
                            CreateEmbed::new()
                                .title("✅ Podarowano rangę 30-dniową")
                                .description(format!(
                                    "Przyznano <@{}> **{}× 30 dni** rangi <@&{}>.",
                                    target_id_u64, units, cfg.role_id.get()
                                ))
                                .field("Łączny koszt", format!("**{} TK**", total), true)
                                .field("Twoje saldo", format!("**{} TK**", buyer_balance), true)
                                .field("Nowa data wygaśnięcia", fmt_dt(new_expires_at), false)
                                .color(0x9B59B6)
                                .timestamp(Utc::now()),
                        )
                        .components(Vec::<CreateActionRow>::new()),
                ).await?;

                // logi
                let buyer = ic.user.clone();
                log_embed(
                    &ctx.http,
                    CreateEmbed::new()
                        .title("🎁 Log: Podarunek rangi")
                        .field("Kupujący", format!("{} (`{}`)", buyer.tag(), buyer.id.get()), true)
                        .field("Obdarowany", format!("<@{}>", target_id_u64), true)
                        .field("Miesięcy", units.to_string(), true)
                        .field("Koszt", format!("{} TK", total), true)
                        .field("Wygasa", fmt_dt(new_expires_at), true)
                        .color(0x9B59B6)
                        .timestamp(Utc::now()),
                ).await;

                log_embed(
                    &ctx.http,
                    CreateEmbed::new()
                        .title("✅ Log: Rola nadana (podarunek)")
                        .field("Użytkownik", format!("<@{}>", target_id_u64), true)
                        .field("Wygasa", fmt_dt_full(new_expires_at), true)
                        .color(0x2ECC71)
                        .timestamp(Utc::now()),
                ).await;
            }
            BuyRoleResult::InsufficientFunds { balance } => {
                ic.edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content(format!(
                            "❌ Za mało środków. Koszt: **{} TK**, Twoje saldo: **{} TK**.",
                            total, balance
                        ))
                        .components(Vec::<CreateActionRow>::new()),
                ).await.ok();
            }
        }

        return Ok(());
    }

    // --- [NOWE] Anulowanie potwierdzenia (powrót do panelu) ---
    // format: "shop|{owner}|qty|{units}|op|giftcancel"
    if cid.starts_with("shop|") && cid.ends_with("|op|giftcancel") {
        let mut it = cid.split('|');
        let _ = it.next();
        let owner = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or_default();
        let _ = it.next(); // "qty"
        let units = it.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(1);

        if ic.user.id.get() != owner {
            return Ok(());
        }

        let current_exp = if let Some(gid) = ic.guild_id {
            get_current_expiry(db, ic.user.id.get() as i64, cfg.role_id.get() as i64, gid.get() as i64).await?
        } else { None };

        let (embed, row_qty, row_actions) = render_panel(owner, units.clamp(1, cfg.max_units), current_exp);

        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .embed(embed)
                    .components(vec![row_qty, row_actions]),
            ),
        ).await.ok();

        return Ok(());
    }

    // --- Gift: selektor użytkownika (KROK 1: wybór adresata) ---
    if let Some(stripped) = cid.strip_prefix("shopgift|") {
        let mut it = stripped.split('|');
        let owner_ok = it
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|uid| uid == ic.user.id.get())
            .unwrap_or(false);

        let _kw_qty = it.next();
        let units = it.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(1).clamp(1, cfg.max_units);

        if !owner_ok { return Ok(()); }

        let target_id_u64 = match &ic.data.kind {
            ComponentInteractionDataKind::UserSelect { values, .. } => values.get(0).map(|u| u.get()),
            _ => None,
        };

        let Some(target_id_u64) = target_id_u64 else {
            ic.create_response(
                &ctx.http,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .content("❌ Nie wybrano użytkownika.")
                ),
            ).await.ok();
            return Ok(());
        };

        let price = cfg.price_tk;
        let total = price.saturating_mul(units);

        // Pokaż ekran potwierdzenia
        let confirm_btn = CreateButton::new(format!("shop|{}|qty|{}|op|giftconfirm|to|{}", ic.user.id.get(), units, target_id_u64))
            .label("✅ Potwierdź")
            .style(ButtonStyle::Success);
        let cancel_btn = CreateButton::new(format!("shop|{}|qty|{}|op|giftcancel", ic.user.id.get(), units))
            .label("↩️ Anuluj")
            .style(ButtonStyle::Secondary);

        let row = CreateActionRow::Buttons(vec![confirm_btn, cancel_btn]);

        let embed = CreateEmbed::new()
            .title("🎁 Podarunek — potwierdzenie")
            .description("Zweryfikuj szczegóły i zatwierdź zakup.")
            .field("Adresat", format!("<@{}>", target_id_u64), true)
            .field("Pakiet", format!("{}× 30 dni", units), true)
            .field("Koszt", format!("**{} TK**", total), true)
            .color(THEME_ORANGE)
            .timestamp(Utc::now());

        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .embed(embed)
                    .components(vec![row]),
            ),
        ).await.ok();

        return Ok(());
    }

    // --- Panel główny (przyciski inc/dec/buy/gift) ---
    let Some((owner_uid, mut units, op)) = parse_panel_action(cid) else {
        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("⚠️ Ten panel jest nieaktualny. Otwórz `/shop` ponownie.")
            )
        ).await.ok();
        return Ok(());
    };

    if ic.user.id.get() != owner_uid {
        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("❌ Ten panel nie należy do Ciebie. Użyj własnego `/shop`."),
            ),
        ).await.ok();
        return Ok(());
    }

    units = units.clamp(1, cfg.max_units);

    match op {
        PanelOp::Inc => {
            units = (units + 1).min(cfg.max_units);
        }
        PanelOp::Dec => {
            units = (units - 1).max(1);
        }
        PanelOp::Buy => {
            let Some(guild_id) = ic.guild_id else {
                ic.create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().ephemeral(true).content("❌ Ta akcja wymaga serwera (guild)."),
                    ),
                ).await.ok();
                return Ok(());
            };

            // ✅ szybki ACK
            ic.defer(&ctx.http).await?;

            let price = cfg.price_tk;
            let total = price.saturating_mul(units);
            let buyer_id = ic.user.id.get() as i64;

            match buy_role_tx(db, buyer_id, buyer_id, units, total, cfg.role_id.get() as i64, guild_id.get() as i64).await? {
                BuyRoleResult::Ok { buyer_balance, new_expires_at } => {
                    ensure_role_added(&ctx.http, guild_id, ic.user.id).await;

                    // DM do kupującego (z nazwą roli)
                    let role_name = role_name_for_dm(&ctx.http, guild_id, cfg.role_id).await;

                    dm_user(
                        &ctx.http,
                        ic.user.id,
                        CreateEmbed::new()
                            .title("✅ Ranga przyznana")
                            .description(format!("Twoja ranga **{}** została dodana.", role_name))
                            .field("Wygasa", fmt_dt_full(new_expires_at), true)
                            .color(THEME_ORANGE)
                            .timestamp(Utc::now()),
                    ).await;

                    // potwierdzenie w miejscu przycisków (bez przycisków)
                    ic.edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .embed(
                                CreateEmbed::new()
                                    .title("✅ Zakup zrealizowany: Ranga 30-dniowa")
                                    .description(format!(
                                        "Kupiłeś **{}×** po 30 dni rangi <@&{}>.", units, cfg.role_id.get()
                                    ))
                                    .field("Łączny koszt", format!("**{} TK**", total), true)
                                    .field("Twoje nowe saldo", format!("**{} TK**", buyer_balance), true)
                                    .field("Nowa data wygaśnięcia", fmt_dt(new_expires_at), false)
                                    .color(0x2ECC71)
                                    .timestamp(Utc::now())
                            )
                            .components(Vec::<CreateActionRow>::new()),
                    ).await?;

                    // logi
                    let user_c = ic.user.clone();
                    log_embed(
                        &ctx.http,
                        CreateEmbed::new()
                            .title("🛒 Log: Zakup rangi")
                            .field("Kupujący", format!("{} (`{}`)", user_c.tag(), user_c.id.get()), true)
                            .field("Miesięcy", units.to_string(), true)
                            .field("Koszt", format!("{} TK", cfg.price_tk * units), true)
                            .field("Wygasa", fmt_dt(new_expires_at), true)
                            .color(0x2ECC71)
                            .timestamp(Utc::now()),
                    ).await;

                    log_embed(
                        &ctx.http,
                        CreateEmbed::new()
                            .title("✅ Log: Rola nadana")
                            .field("Użytkownik", format!("<@{}>", ic.user.id.get()), true)
                            .field("Wygasa", fmt_dt_full(new_expires_at), true)
                            .color(0x2ECC71)
                            .timestamp(Utc::now())
                    ).await;

                    return Ok(());
                }
                BuyRoleResult::InsufficientFunds { balance } => {
                    ic.edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .content(format!(
                                "❌ Za mało środków. Koszt: **{} TK**, Twoje saldo: **{} TK**.",
                                total, balance
                            ))
                            .components(Vec::<CreateActionRow>::new()),
                    ).await.ok();
                    return Ok(());
                }
            }
        }
        PanelOp::Gift => {
            // pokaż selektor użytkownika (KROK 1)
            let current_exp = if let Some(gid) = ic.guild_id {
                get_current_expiry(db, ic.user.id.get() as i64, cfg.role_id.get() as i64, gid.get() as i64).await?
            } else { None };

            let (embed, row_qty, row_actions) = render_panel(owner_uid, units, current_exp);

            let select = CreateSelectMenu::new(
                format!("shopgift|{}|qty|{}", owner_uid, units),
                CreateSelectMenuKind::User { default_users: None },
            )
            .placeholder("Wybierz obdarowanego…")
            .min_values(1)
            .max_values(1);

            let row_gift = CreateActionRow::SelectMenu(select);

            ic.create_response(
                &ctx.http,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .embed(embed)
                        .components(vec![row_qty, row_actions, row_gift]),
                ),
            ).await.ok();
            return Ok(());
        }
    }

    // odśwież panel po inc/dec
    let current_exp = if let Some(gid) = ic.guild_id {
        get_current_expiry(db, ic.user.id.get() as i64, cfg.role_id.get() as i64, gid.get() as i64).await?
    } else {
        None
    };
    let (embed, row_qty, row_actions) = render_panel(owner_uid, units, current_exp);

    ic.create_response(
        &ctx.http,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embed(embed)
                .components(vec![row_qty, row_actions]),
        ),
    ).await.ok();

    Ok(())
}

pub async fn handle_modal(_: &Context, _: &ModalInteraction, _: &PgPool) -> Result<()> {
    Ok(())
}

// =======================================
// 💾 DB + logika zakupów
// =======================================

enum BuyRoleResult {
    Ok { buyer_balance: i64, new_expires_at: DateTime<Utc> },
    InsufficientFunds { balance: i64 },
}

async fn buy_role_tx(
    db: &PgPool,
    buyer_id: i64,
    target_id: i64,
    units: i64,
    total_cost: i64,
    role_id: i64,
    guild_id: i64,
) -> Result<BuyRoleResult> {
    let mut tx = db.begin().await?;

    sqlx::query(
        r#"INSERT INTO users (id,balance) VALUES ($1,0),($2,0)
           ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(buyer_id)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;

    let new_balance: Option<i64> = sqlx::query_scalar(
        r#"UPDATE users
           SET balance = balance - $1
           WHERE id=$2 AND balance >= $1
           RETURNING balance"#,
    )
    .bind(total_cost)
    .bind(buyer_id)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(bal) = new_balance {
        let now = Utc::now();
        let current: Option<DateTime<Utc>> = sqlx::query_scalar(
            r#"SELECT expires_at FROM role_subscriptions
               WHERE user_id=$1 AND role_id=$2 AND guild_id=$3 AND active=true
               FOR UPDATE"#,
        )
        .bind(target_id)
        .bind(role_id)
        .bind(guild_id)
        .fetch_optional(&mut *tx)
        .await?;

        let base = current.unwrap_or(now);
        let base = if base > now { base } else { now };
        let new_expires = base + Duration::days(config().days_per_unit * units);

        sqlx::query(
            r#"
            INSERT INTO role_subscriptions (user_id, role_id, guild_id, expires_at, active)
            VALUES ($1,$2,$3,$4,true)
            ON CONFLICT (user_id,role_id,guild_id)
            DO UPDATE SET expires_at = EXCLUDED.expires_at, active=true
            "#,
        )
        .bind(target_id)
        .bind(role_id)
        .bind(guild_id)
        .bind(new_expires)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(BuyRoleResult::Ok { buyer_balance: bal, new_expires_at: new_expires })
    } else {
        let balance: i64 = sqlx::query(r#"SELECT balance FROM users WHERE id=$1"#)
            .bind(buyer_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("balance")?;

        tx.rollback().await?;
        Ok(BuyRoleResult::InsufficientFunds { balance })
    }
}

async fn get_current_expiry(
    db: &PgPool,
    user_id: i64,
    role_id: i64,
    guild_id: i64,
) -> Result<Option<DateTime<Utc>>> {
    let exp: Option<DateTime<Utc>> = sqlx::query_scalar(
        r#"SELECT expires_at FROM role_subscriptions
           WHERE user_id=$1 AND role_id=$2 AND guild_id=$3 AND active=true"#,
    )
    .bind(user_id)
    .bind(role_id)
    .bind(guild_id)
    .fetch_optional(db)
    .await?;
    Ok(exp)
}

async fn expire_roles_tick(ctx: &Context, db: &PgPool, guild_id: GuildId) -> Result<()> {
    let expired: Vec<(i64,)> = sqlx::query_as(
        r#"SELECT user_id FROM role_subscriptions
           WHERE guild_id=$1 AND active=true AND expires_at <= NOW()"#,
    )
    .bind(guild_id.get() as i64)
    .fetch_all(db)
    .await?;

    if expired.is_empty() { return Ok(()); }

    sqlx::query(
        r#"UPDATE role_subscriptions
           SET active=false
           WHERE guild_id=$1 AND active=true AND expires_at <= NOW()"#,
    )
    .bind(guild_id.get() as i64)
    .execute(db)
    .await?;

    let removed_count = expired.len();
    for (uid,) in expired {
        ensure_role_removed(&ctx.http, guild_id, UserId::new(uid as u64)).await;
    }

    log_embed(
        &ctx.http,
        CreateEmbed::new()
            .title("🧹 Subskrypcje: wygasłe role zdjęte")
            .description(format!("Usunięto rolę <@&{}> {} użytkownikom.", config().role_id.get(), removed_count))
            .color(0xE67E22)
            .timestamp(Utc::now()),
    ).await;

    Ok(())
}

async fn ensure_schema(db: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id BIGINT PRIMARY KEY,
            balance BIGINT NOT NULL DEFAULT 0,
            last_work TIMESTAMPTZ,
            last_slut TIMESTAMPTZ,
            last_crime TIMESTAMPTZ,
            last_rob TIMESTAMPTZ
        );
        "#,
    ).execute(db).await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS role_subscriptions (
            user_id BIGINT NOT NULL,
            role_id BIGINT NOT NULL,
            guild_id BIGINT NOT NULL,
            expires_at TIMESTAMPTZ NOT NULL,
            active BOOLEAN NOT NULL DEFAULT true,
            PRIMARY KEY (user_id, role_id, guild_id)
        );
        "#,
    ).execute(db).await?;

    Ok(())
}
