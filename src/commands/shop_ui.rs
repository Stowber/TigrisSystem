use anyhow::Result;
use chrono::Utc;
use once_cell::sync::OnceCell as SyncOnceCell;
use serenity::all::*;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateCommand, CreateEmbed, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption,
};
use sqlx::{PgPool, Row};
use std::collections::HashMap;

// =======================================
// ⚙️ Konfiguracja i cache
// =======================================

static LOG_CHAN: SyncOnceCell<Option<ChannelId>> = SyncOnceCell::new();

#[inline]
fn log_channel_id() -> Option<ChannelId> {
    *LOG_CHAN.get_or_init(|| {
        let id = std::env::var("LOG_CHANNEL_ID")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if id == 0 {
            None
        } else {
            Some(ChannelId::new(id))
        }
    })
}

// =======================================
// 🛒 Katalog
// =======================================

#[derive(Clone)]
struct Item {
    id: &'static str,
    name: &'static str,
    price: i64,
    emoji: &'static str,
    desc: &'static str,
}

const CATALOG: &[Item] = &[
    Item { id: "lottery", name: "Los na loterię", price: 150, emoji: "🎫", desc: "Szansa na bonusowe TK w eventach." },
    Item { id: "cookie",  name: "Ciasteczko",     price: 30,  emoji: "🍪", desc: "Mały boost morale. Słodkie!"     },
    Item { id: "pickaxe", name: "Kilof",          price: 800, emoji: "⛏️", desc: "Otwiera drogę do kopalni (mini-eventy)." },
    Item { id: "vipday",  name: "VIP 24h",        price: 2500,emoji: "💎", desc: "Status VIP na 24h (placeholder)." },
];

#[inline]
fn catalog_by_id() -> &'static HashMap<&'static str, &'static Item> {
    static MAP: SyncOnceCell<HashMap<&'static str, &'static Item>> = SyncOnceCell::new();
    MAP.get_or_init(|| CATALOG.iter().map(|i| (i.id, i)).collect())
}

// =======================================
// 🔧 Rejestracja komendy (pasuje do register(&mut c))
// =======================================

pub fn register(cmd: &mut CreateCommand) -> &mut CreateCommand {
    *cmd = CreateCommand::new("shop")
        .description("Otwórz sklep Tigrus™ (panel z przyciskami)");
    cmd
}

// =======================================
// 🔧 Helper: budowa komponentów bez closur
// =======================================

fn rows_to_components(rows: Vec<CreateActionRow>) -> Vec<CreateActionRow> {
    rows
}

// =======================================
// 🚀 Obsługa komendy
// =======================================

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    ensure_schema(db).await?;

    let opener_id = cmd.user.id.get();
    let init_item = &CATALOG[0];
    let qty = 1i64;

    let (embed, row_select, row_qty, row_actions) = render_panel(opener_id, init_item, qty);

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embed(embed)
                .components(
                    rows_to_components(vec![row_select, row_qty, row_actions])
                ),
        ),
    )
    .await?;

    Ok(())
}

// =======================================
// 🧩 Obsługa komponentów & „gift select”
// =======================================

pub async fn handle_component(
    ctx: &Context,
    ic: &ComponentInteraction,
    db: &PgPool,
) -> Result<()> {
    let cid = ic.data.custom_id.as_str();

    // --- 0) Gift user select ---
    // custom_id: shopgift|uid|item|{id}|qty|{n}
    if let Some(stripped) = cid.strip_prefix("shopgift|") {
        let mut it = stripped.split('|');
        let owner_ok = it
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            == Some(ic.user.id.get());
        let _kw_item = it.next(); // "item"
        let item_id = it.next().unwrap_or("");
        let _kw_qty = it.next(); // "qty"
        let qty = it
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(1)
            .clamp(1, 999);

        if owner_ok {
            let Some(&item) = catalog_by_id().get(item_id) else {
                ic.create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .ephemeral(true)
                            .content("❌ Nieznany przedmiot."),
                    ),
                )
                .await
                .ok();
                return Ok(());
            };

            let target_id_u64 = match &ic.data.kind {
                ComponentInteractionDataKind::UserSelect { values, .. } => {
                    values.get(0).map(|u| u.get())
                }
                _ => None,
            };

            let Some(target_id_u64) = target_id_u64 else {
                ic.create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .ephemeral(true)
                            .content("❌ Nie wybrano użytkownika."),
                    ),
                )
                .await
                .ok();
                return Ok(());
            };

            let buyer_id = ic.user.id.get() as i64;
            let target_id = target_id_u64 as i64;
            let total = item.price.saturating_mul(qty);

            match buy_item_tx(db, buyer_id, target_id, item.id, qty, total).await? {
                BuyResult::Ok { buyer_balance } => {
                    ic.create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().ephemeral(true).content(
                                format!(
                                    "✅ Podarowano **{}× {}** {} dla <@{}> (koszt **{} TK**). Twoje saldo: **{} TK**.",
                                    qty, item.name, item.emoji, target_id_u64, total, buyer_balance
                                ),
                            ),
                        ),
                    )
                    .await
                    .ok();

                    if let Some(ch) = log_channel_id() {
                        let http = ctx.http.clone();
                        let user_c = ic.user.clone();
                        tokio::spawn(async move {
                            let _ = ch
                                .send_message(
                                    &http,
                                    CreateMessage::new().embed(
                                        CreateEmbed::new()
                                            .title("🎁 Log: Podarunek")
                                            .field(
                                                "Kupujący",
                                                format!("{} (`{}`)", user_c.tag(), user_c.id.get()),
                                                true,
                                            )
                                            .field(
                                                "Obdarowany",
                                                format!("<@{}>", target_id_u64),
                                                true,
                                            )
                                            .field("Przedmiot", format!("`{}`", item.id), true)
                                            .field("Ilość", qty.to_string(), true)
                                            .field("Koszt", format!("{} TK", total), true)
                                            .color(0x9B59B6)
                                            .timestamp(Utc::now()),
                                    ),
                                )
                                .await;
                        });
                    }
                }
                BuyResult::InsufficientFunds { balance } => {
                    ic.create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().ephemeral(true).content(
                                format!(
                                    "❌ Za mało środków. Koszt: **{} TK**, Twoje saldo: **{} TK**.",
                                    total, balance
                                ),
                            ),
                        ),
                    )
                    .await
                    .ok();
                }
            }

            return Ok(());
        }
    }

    // --- 1) Standardowy panel sklepu ---
    // custom_id: shop|uid|item|{id}|qty|{n}|op|{sel/inc/dec/buy/gift}
    let parts: Vec<&str> = cid.split('|').collect();
    if parts.len() < 8 || parts[0] != "shop" {
        return Ok(());
    }

    let owner_uid = parts[1].parse::<u64>().unwrap_or(0);
    if ic.user.id.get() != owner_uid {
        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("❌ Ten panel nie należy do Ciebie. Użyj własnego `/shop`."),
            ),
        )
        .await
        .ok();
        return Ok(());
    }

    let mut current_item_id = parts[3];
    let mut qty = parts[5].parse::<i64>().unwrap_or(1).clamp(1, 999);
    let op = parts[7];

    if op == "sel" {
        if let ComponentInteractionDataKind::StringSelect { values, .. } = &ic.data.kind {
            if let Some(v) = values.get(0) {
                current_item_id = v.as_str();
            }
        }
    }

    let Some(&item) = catalog_by_id().get(current_item_id) else {
        ic.create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new().content("❌ Nieznany przedmiot."),
            ),
        )
        .await
        .ok();
        return Ok(());
    };

    match op {
        "inc" => qty = (qty + 1).min(999),
        "dec" => qty = (qty - 1).max(1),
        "buy" => {
            let buyer_id = ic.user.id.get() as i64;
            let total = item.price.saturating_mul(qty);
            match buy_item_tx(db, buyer_id, buyer_id, item.id, qty, total).await? {
                BuyResult::Ok { buyer_balance } => {
                    let embed = CreateEmbed::new()
                        .title("✅ Zakup zrealizowany")
                        .description(format!(
                            "Kupiłeś **{}× {}** {} za **{} TK**.",
                            qty, item.name, item.emoji, total
                        ))
                        .field("Twoje nowe saldo", format!("**{} TK**", buyer_balance), true)
                        .field("Przedmiot", format!("`{}` — {}", item.id, item.name), true)
                        .color(0x2ECC71)
                        .timestamp(Utc::now());

                    ic.create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(embed),
                        ),
                    )
                    .await
                    .ok();

                    if let Some(ch) = log_channel_id() {
                        let http = ctx.http.clone();
                        let user_c = ic.user.clone();
                        let cost = item.price * qty;
                        tokio::spawn(async move {
                            let _ = ch
                                .send_message(
                                    &http,
                                    CreateMessage::new().embed(
                                        CreateEmbed::new()
                                            .title("🛒 Log: Zakup")
                                            .field(
                                                "Kupujący",
                                                format!("{} (`{}`)", user_c.tag(), user_c.id.get()),
                                                true,
                                            )
                                            .field("Przedmiot", format!("`{}`", item.id), true)
                                            .field("Ilość", qty.to_string(), true)
                                            .field("Koszt", format!("{} TK", cost), true)
                                            .color(0x2ECC71)
                                            .timestamp(Utc::now()),
                                    ),
                                )
                                .await;
                        });
                    }

                    return Ok(());
                }
                BuyResult::InsufficientFunds { balance } => {
                    ic.create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().ephemeral(true).content(
                                format!(
                                    "❌ Za mało środków. Koszt: **{} TK**, Twoje saldo: **{} TK**.",
                                    total, balance
                                ),
                            ),
                        ),
                    )
                    .await
                    .ok();
                    return Ok(());
                }
            }
        }
        "gift" => {
            let (embed, row_select, row_qty, row_actions) = render_panel(owner_uid, item, qty);

            // dodajemy dodatkowy SelectUser
            let select = CreateSelectMenu::new(
                format!("shopgift|{}|item|{}|qty|{}", owner_uid, item.id, qty),
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
                        .components(rows_to_components(vec![
                            row_select,
                            row_qty,
                            row_actions,
                            row_gift,
                        ])),
                ),
            )
            .await
            .ok();

            return Ok(());
        }
        _ => {}
    }

    // odśwież panel po inc/dec/sel
    let (embed, row_select, row_qty, row_actions) = render_panel(owner_uid, item, qty);
    ic.create_response(
        &ctx.http,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embed(embed)
                .components(rows_to_components(vec![row_select, row_qty, row_actions])),
        ),
    )
    .await
    .ok();

    Ok(())
}

// (Modal handler nieużywany)
pub async fn handle_modal(_: &Context, _: &ModalInteraction, _: &PgPool) -> Result<()> {
    Ok(())
}

// =======================================
// 🖼️ Render panelu (bez Vec, zwraca 3 konkretne wiersze)
// =======================================

fn render_panel(
    owner_uid: u64,
    item: &'static Item,
    qty: i64,
) -> (CreateEmbed, CreateActionRow, CreateActionRow, CreateActionRow) {
    let total = item.price.saturating_mul(qty);

    let embed = CreateEmbed::new()
        .title("🛒 Sklep Tigrus™")
        .description("Wybierz przedmiot z listy, ustaw ilość przyciskami i kliknij **Kup** albo **Podaruj**.")
        .field(
            "Przedmiot",
            format!("{} {} (`{}`)\n{}", item.emoji, item.name, item.id, item.desc),
            false,
        )
        .field(
            "Cena",
            format!("**{} TK** × **{}** = **{} TK**", item.price, qty, total),
            true,
        )
        .color(0x00B7FF)
        .timestamp(Utc::now());

    // Select menu (items)
    let options: Vec<CreateSelectMenuOption> = CATALOG
        .iter()
        .map(|it| {
            let opt = CreateSelectMenuOption::new(
                format!("{} {} — {} TK", it.emoji, it.name, it.price),
                it.id,
            );
            if it.id == item.id {
                opt.default_selection(true)
            } else {
                opt
            }
        })
        .collect();

    let select = CreateSelectMenu::new(
        format!("shop|{}|item|{}|qty|{}|op|sel", owner_uid, item.id, qty),
        CreateSelectMenuKind::String { options },
    )
    .placeholder("Wybierz przedmiot…")
    .min_values(1)
    .max_values(1);

    let row_select = CreateActionRow::SelectMenu(select);

    // +/- ilość
    let row_qty = CreateActionRow::Buttons(vec![
        CreateButton::new(format!(
            "shop|{}|item|{}|qty|{}|op|dec",
            owner_uid, item.id, qty
        ))
        .label("−")
        .style(ButtonStyle::Secondary),
        CreateButton::new(format!(
            "shop|{}|item|{}|qty|{}|op|inc",
            owner_uid, item.id, qty
        ))
        .label("+")
        .style(ButtonStyle::Secondary),
    ]);

    // Kup / Podaruj
    let row_actions = CreateActionRow::Buttons(vec![
        CreateButton::new(format!(
            "shop|{}|item|{}|qty|{}|op|buy",
            owner_uid, item.id, qty
        ))
        .label("Kup")
        .style(ButtonStyle::Success),
        CreateButton::new(format!(
            "shop|{}|item|{}|qty|{}|op|gift",
            owner_uid, item.id, qty
        ))
        .label("🎁 Podaruj")
        .style(ButtonStyle::Primary),
    ]);

    (embed, row_select, row_qty, row_actions)
}

// =======================================
// 💾 DB
// =======================================

enum BuyResult {
    Ok { buyer_balance: i64 },
    InsufficientFunds { balance: i64 },
}

async fn buy_item_tx(
    db: &PgPool,
    buyer_id: i64,
    target_id: i64,
    item_id: &str,
    qty: i64,
    total_cost: i64,
) -> Result<BuyResult> {
    let mut tx = db.begin().await?;

    sqlx::query(
        r#"INSERT INTO users (id, balance) VALUES ($1,0),($2,0) ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(buyer_id)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;

    let buyer_balance: i64 = sqlx::query(r#"SELECT balance FROM users WHERE id=$1 FOR UPDATE"#)
        .bind(buyer_id)
        .fetch_one(&mut *tx)
        .await?
        .try_get("balance")?;

    if buyer_balance < total_cost {
        tx.rollback().await?;
        return Ok(BuyResult::InsufficientFunds {
            balance: buyer_balance,
        });
    }

    let new_balance: Option<i64> = sqlx::query_scalar(
        r#"UPDATE users SET balance = balance - $1 WHERE id=$2 AND balance >= $1 RETURNING balance"#,
    )
    .bind(total_cost)
    .bind(buyer_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(bal) = new_balance else {
        tx.rollback().await?;
        return Ok(BuyResult::InsufficientFunds {
            balance: buyer_balance,
        });
    };

    sqlx::query(
        r#"INSERT INTO inventory (user_id,item_id,qty) VALUES ($1,$2,$3)
           ON CONFLICT (user_id,item_id) DO UPDATE SET qty = inventory.qty + EXCLUDED.qty"#,
    )
    .bind(target_id)
    .bind(item_id)
    .bind(qty)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(BuyResult::Ok { buyer_balance: bal })
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
        );"#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS inventory (
            user_id BIGINT NOT NULL,
            item_id TEXT   NOT NULL,
            qty     BIGINT NOT NULL DEFAULT 0,
            PRIMARY KEY (user_id,item_id)
        );"#,
    )
    .execute(db)
    .await?;

    Ok(())
}
