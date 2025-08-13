//! commands/crime.rs — SOLO (Simon) + trwałe profile/ustawienia w Postgres (Serenity 0.12.4)

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use dashmap::DashMap;
use once_cell::sync::OnceCell;
use tokio::sync::Mutex;

use serenity::all::{
    ButtonStyle, CommandInteraction, CommandOptionType, ComponentInteraction, ComponentInteractionDataKind,
    Context, CreateActionRow, CreateButton, CreateCommand, CreateCommandOption, CreateEmbed,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, InteractionResponseFlags, ModalInteraction, UserId,
};
use sqlx::PgPool;

use crate::engine::{
    core::resolve_solo,
    items,
    minigames,
    repo::{MemorySoloRepo, SoloRepo},
    types::*,
};

// =================== Service & Sessions ===================

static SERVICE: OnceCell<Arc<CrimeService>> = OnceCell::new();

fn service() -> Arc<CrimeService> {
    SERVICE.get_or_init(|| Arc::new(CrimeService::new_in_memory())).clone()
}

pub struct CrimeService {
    pub repo: Arc<MemorySoloRepo>,           // HEAT/PP/skill in-memory (mirror DB)
    pub sessions: DashMap<u64, SoloSession>, // per user_id
    pub create_lock: Mutex<()>,
}
impl CrimeService {
    pub fn new_in_memory() -> Self {
        Self {
            repo: Arc::new(MemorySoloRepo::new()),
            sessions: DashMap::new(),
            create_lock: Mutex::new(()),
        }
    }

    pub async fn get_or_create_session(
        &self,
        user: UserId,
    ) -> dashmap::mapref::one::RefMut<'_, u64, SoloSession> {
        if let Some(e) = self.sessions.get_mut(&user.get()) {
            return e;
        }
        let _g = self.create_lock.lock().await;
        if let Some(e) = self.sessions.get_mut(&user.get()) {
            return e;
        }
        self.sessions
            .entry(user.get())
            .or_insert_with(|| SoloSession::new(user.get()))
    }
}

#[derive(Debug, Clone)]
pub struct SoloSession {
    pub user_id: u64,
    pub state: SoloState,
    pub base_cfg: SoloHeistConfig, // snapshot do resolve
}
impl SoloSession {
    pub fn new(user_id: u64) -> Self {
        Self {
            user_id,
            state: SoloState::Config(SoloHeistConfig::default()),
            base_cfg: SoloHeistConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SoloState {
    Config(SoloHeistConfig),
    InSimon {
        spec: SimonSpec,
        seq: Vec<char>,
        cursor: usize,
        result: Option<MinigameResult>,
        reveal_until: Option<Instant>,
        reveals_left: u8,
    },
    Resolved(ResolvedView),
}

#[derive(Debug, Clone)]
pub struct ResolvedView {
    pub outcome: HeistOutcome,
    pub cfg: SoloHeistConfig,
    pub mg: MinigameResult,
    pub before: PlayerProfile, // balance = z DB „przed”
    pub after: PlayerProfile,  // balance = z DB „po”
    pub newly_unlocked: Vec<ItemKey>,
}

// =================== Publiczny interfejs ===================

pub fn register() -> CreateCommand {
    CreateCommand::new("crime")
        .description("Napad SOLO (Simon) z przedmiotami")
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "start",
            "Rozpocznij napad (SOLO)",
        ))
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "profil",
            "Pokaż swój profil i odblokowane przedmioty",
        ))
}

pub async fn run(ctx: &Context, cmd: &CommandInteraction, db: &PgPool) -> Result<()> {
    let sub = cmd
        .data
        .options
        .get(0)
        .map(|o| o.name.as_str())
        .unwrap_or("start");
    let _ = ensure_schema_all(db).await;
    let svc = service();

    match sub {
        "profil" => show_profile(ctx, cmd, &svc, db).await,
        _ => start_solo(ctx, cmd, &svc, db).await, // <- przekazujemy db
    }
}

pub async fn handle_component(ctx: &Context, mci: &ComponentInteraction, db: &PgPool) -> Result<()> {
    if !mci.data.custom_id.starts_with("crime:solo:") {
        return Ok(());
    }
    let svc = service();

    let user = mci.user.id;
    let mut entry = svc.get_or_create_session(user).await;
    let session = entry.value_mut();

    // crime:solo:{action}[:payload]
    let parts: Vec<&str> = mci.data.custom_id.split(':').collect();
    let action = parts.get(2).copied().unwrap_or_default();
    let payload = parts.get(3).copied();

    match action {
        // konfiguracja (przyciski)
        "mode" => {
            let mut to_save: Option<SoloHeistConfig> = None;
            if let SoloState::Config(cfg) = &mut session.state {
                if let Some(k) = payload {
                    cfg.mode = Some(from_key_mode(k));
                    to_save = Some(cfg.clone());
                }
            }
            if let Some(cfg) = to_save {
                save_settings_db(db, user.get(), &cfg).await.ok();
            }
        }
        "risk" => {
            let mut to_save: Option<SoloHeistConfig> = None;
            if let SoloState::Config(cfg) = &mut session.state {
                if let Some(k) = payload {
                    cfg.risk = Some(from_key_risk(k));
                    to_save = Some(cfg.clone());
                }
            }
            if let Some(cfg) = to_save {
                save_settings_db(db, user.get(), &cfg).await.ok();
            }
        }

        // podgląd sekwencji (skrót używany przez UI)
        "simon_show" => {
            if let SoloState::InSimon { seq, result, reveal_until, reveals_left, .. } = &mut session.state {
                if result.is_none() && *reveals_left > 0 {
                    let total_ms = 800u64 * seq.len() as u64;
                    *reveal_until = Some(Instant::now() + Duration::from_millis(total_ms));
                    *reveals_left -= 1;
                }
            }
        }

        // konfiguracja (multiselect items)
        "itemselect" => {
            let mut to_save: Option<SoloHeistConfig> = None;
            if let SoloState::Config(cfg) = &mut session.state {
                let profile = svc.repo.get_or_create(user.get());
                let avail: std::collections::HashSet<_> =
                    items::available_items(profile.pp).into_iter().collect();

                if let ComponentInteractionDataKind::StringSelect { values } = &mci.data.kind {
                    let mut picked = Vec::new();
                    for v in values {
                        if picked.len() >= 3 { break; }
                        if let Some(k) = from_key_item(v) {
                            if avail.contains(&k) {
                                picked.push(k);
                            }
                        }
                    }
                    cfg.items = picked;
                    to_save = Some(cfg.clone());
                }
            }
            if let Some(cfg) = to_save {
                save_settings_db(db, user.get(), &cfg).await.ok();
            }
        }

        // alternatywny przycisk podglądu (zostawiony kompatybilnie)
        "simon_reveal" => {
            // pobierz ryzyko bez mutable borrowów
            let risk_for_preview = extract_cfg(session).risk.unwrap_or(Risk::Medium);

            if let SoloState::InSimon { seq, reveal_until, reveals_left, .. } = &mut session.state {
                if let Some(t) = *reveal_until {
                    if Instant::now() < t {
                        let left = t.saturating_duration_since(Instant::now()).as_millis();
                        return mci.create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .flags(InteractionResponseFlags::EPHEMERAL)
                                    .content(format!("👁️ Podgląd już trwa — jeszcze ~{}ms.", left))
                            )
                        ).await.map_err(Into::into);
                    }
                }
                if *reveals_left == 0 {
                    return mci.create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .flags(InteractionResponseFlags::EPHEMERAL)
                                .content("👁️ Brak pozostałych podglądów.")
                        )
                    ).await.map_err(Into::into);
                }

                let ms = simon_preview_ms(risk_for_preview, seq.len(), 1.0);
                *reveals_left -= 1;
                *reveal_until = Some(Instant::now() + Duration::from_millis(ms));
            }
        }

        "item" => {
            // (legacy toggle; nieużywane przez UI, zostawione kompatybilnie)
            let mut to_save: Option<SoloHeistConfig> = None;
            if let SoloState::Config(cfg) = &mut session.state {
                if let Some(k) = payload {
                    if let Some(item) = from_key_item(k) {
                        if let Some(pos) = cfg.items.iter().position(|i| i == &item) {
                            cfg.items.remove(pos);
                        } else if cfg.items.len() < 3 {
                            cfg.items.push(item);
                        }
                        to_save = Some(cfg.clone());
                    }
                }
            }
            if let Some(cfg) = to_save {
                save_settings_db(db, user.get(), &cfg).await.ok();
            }
        }

        "start" => {
            if let SoloState::Config(cfg0) = &session.state {
                if cfg0.mode.is_some() && cfg0.risk.is_some() {
                    // wymuszamy Simon i zapisujemy snapshot:
                    let mut cfg = cfg0.clone();
                    cfg.minigame = MinigameKind::Simon;
                    session.base_cfg = cfg.clone();

                    let effects = items::aggregate(&cfg.items);

                    let spec = minigames::simon_spec_for(cfg.risk.unwrap(), effects.simon_seq_delta);
                    let seq  = minigames::gen_simon_seq(&spec);

                    let reveals_left = match cfg.risk.unwrap_or(Risk::Medium) {
                        Risk::Low => 2,
                        Risk::Medium => 1,
                        Risk::High | Risk::Hardcore => 0,
                    };

                    let ms = simon_preview_ms(cfg.risk.unwrap(), seq.len(), effects.simon_time_mult);

                    session.state = SoloState::InSimon {
                        spec,
                        seq,
                        cursor: 0,
                        result: None,
                        reveal_until: Some(Instant::now() + Duration::from_millis(ms)),
                        reveals_left,
                    };

                    // zapisz aktualne ustawienia do DB (dla pewności)
                    save_settings_db(db, user.get(), &cfg).await.ok();
                }
            }
        }

        // Simon — wprowadzanie znaków
        "simon_key" => {
            if let (Some(k), SoloState::InSimon { seq, cursor, result, reveal_until, .. }) =
                (payload, &mut session.state)
            {
                if let Some(t) = *reveal_until {
                    if Instant::now() < t {
                        return Ok(());
                    } else {
                        *reveal_until = None;
                    }
                }
                if result.is_some() { return Ok(()); }

                if *cursor >= seq.len() {
                    *result = Some(MinigameResult::Success);
                    return Ok(());
                }

                let got = k.chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or('?');
                let expected = seq[*cursor];

                if minigames::check_simon_step(expected, got) {
                    *cursor += 1;
                    if *cursor >= seq.len() {
                        *result = Some(MinigameResult::Success);
                    }
                } else {
                    *result = Some(MinigameResult::Fail);
                }
            }
        }

        "resolve" => {
            // 1) wejście do resolvera
            let cfg = extract_cfg(session);
            let mg_res = match &session.state {
                SoloState::InSimon { result, .. } => result.unwrap_or(MinigameResult::NotPlayed),
                SoloState::Config(_) => MinigameResult::NotPlayed,
                SoloState::Resolved(v) => v.mg,
            };

            // 2) profil „pamięciowy” (HEAT/PP/skill)
            let before_mem = svc.repo.get_or_create(user.get());

            // 3) BALANCE z DB — stan „przed”
            let db_before = fetch_balance(db, user.get()).await.unwrap_or(0);

            // 4) rozstrzygnięcie (amount_final = delta TK)
            let (after_mem, outcome) = resolve_solo(before_mem.clone(), &cfg, mg_res);

            // 5) BALANCE z DB — atomowo dodaj delta TK i zwróć stan „po”
            let db_after = add_balance(db, user.get(), outcome.amount_final)
                .await
                .unwrap_or(db_before);

            // 6) nowo odblokowane itemy (pochodne od PP)
            let before_av = items::available_items(before_mem.pp);
            let after_av = items::available_items(after_mem.pp);
            let newly_unlocked: Vec<ItemKey> =
                after_av.into_iter().filter(|i| !before_av.contains(i)).collect();

            // 7) zapisz profil pamięciowy i do DB (balance z DB)
            let mut after_mem_fixed = after_mem.clone();
            let mut before_mem_fixed = before_mem.clone();
            before_mem_fixed.balance = db_before;
            after_mem_fixed.balance = db_after;

            // persist w DB
            save_profile_db(db, user.get(), &after_mem_fixed).await.ok();
            // mirror in-memory
            svc.repo.save(&after_mem_fixed);

            session.state = SoloState::Resolved(ResolvedView {
                outcome,
                cfg,
                mg: mg_res,
                before: before_mem_fixed,
                after: after_mem_fixed,
                newly_unlocked,
            });
        }

        "reset" => {
            // reset tylko w Config lub po rozstrzygnięciu
            if matches!(&session.state, SoloState::Config(_) | SoloState::Resolved(_)) {
                session.base_cfg = SoloHeistConfig::default();
                session.state = SoloState::Config(SoloHeistConfig::default());
            } else {
                // w trakcie minigierki – blokada cofania
                return mci
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .flags(InteractionResponseFlags::EPHEMERAL)
                                .content("⛔ Nie możesz wrócić do konfiguracji w trakcie minigierki. Dokończ rundę i rozstrzygnij napad."),
                        ),
                    )
                    .await
                    .map_err(Into::into);
            }
        }

        _ => {}
    }

    // Render (UpdateMessage)
    let (embed, rows) = render_session(&service(), mci.user.id, &session).await;
    mci.create_response(
        &ctx.http,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .add_embed(embed)
                .components(rows),
        ),
    )
    .await?;

    Ok(())
}

pub async fn handle_modal(_ctx: &Context, _modal: &ModalInteraction, _db: &PgPool) -> Result<()> {
    Ok(())
}

// =================== Slash flows ===================

async fn start_solo(ctx: &Context, cmd: &CommandInteraction, svc: &CrimeService, db: &PgPool) -> Result<()> {
    // 1) wczytaj profil z DB do pamięci (mirror)
    let mut p = load_profile_db(db, cmd.user.id.get()).await.unwrap_or_default();
    // dołóż realny balance z DB
    if let Ok(bal) = fetch_balance(db, cmd.user.id.get()).await {
        p.balance = bal;
    }
    svc.repo.save(&p);

    // 2) nowa sesja
    {
        let mut entry = svc.get_or_create_session(cmd.user.id).await;
        *entry = SoloSession::new(cmd.user.id.get());
    }

    // 3) wczytaj ostatnie ustawienia i ustaw w sesji
    if let Ok(Some(s)) = load_settings_db(db, cmd.user.id.get()).await {
        let mut entry = svc.get_or_create_session(cmd.user.id).await;
        if let SoloState::Config(cfg) = &mut entry.state {
            cfg.mode = s.mode;
            cfg.risk = s.risk;
            cfg.items = s.items;
            cfg.minigame = MinigameKind::Simon; // zawsze Simon
        }
    }

    let entry = svc.get_or_create_session(cmd.user.id).await;
    let session = entry.value();
    let (embed, rows) = render_session(svc, cmd.user.id, session).await;

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .flags(InteractionResponseFlags::EPHEMERAL)
                .add_embed(embed)
                .components(rows),
        ),
    )
    .await?;

    Ok(())
}

async fn show_profile(ctx: &Context, cmd: &CommandInteraction, svc: &CrimeService, db: &PgPool) -> Result<()> {
    // balance z DB
    let bal = fetch_balance(db, cmd.user.id.get()).await.unwrap_or(0);
    // profil z DB (jeśli brak, domyślny)
    let mut p = load_profile_db(db, cmd.user.id.get()).await.unwrap_or_default();
    p.balance = bal;
    // mirror in-memory (żeby embed gry był spójny)
    svc.repo.save(&p);

    let available = items::available_items(p.pp);
    let names: Vec<&'static str> = available.iter().map(|k| items::item_name(*k)).collect();

    let embed = CreateEmbed::new()
        .title(format!("🧾 Profil — {}", cmd.user.name))
        .field("Saldo (TK)", format!("{}", bal), true)
        .field("HEAT", format!("{}", p.heat), true)
        .field("Umiejętność", format!("{}/50", p.thief_skill), true)
        .field("PP", format!("{}", p.pp), true)
        .field(
            "Odblokowane przedmioty",
            if names.is_empty() { "—".into() } else { names.join(", ") },
            false,
        )
        .color(0x95a5a6);

    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .flags(InteractionResponseFlags::EPHEMERAL)
                .add_embed(embed),
        ),
    )
    .await?;

    Ok(())
}

// =================== Render ===================

async fn render_session(
    svc: &CrimeService,
    user: UserId,
    s: &SoloSession,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    match &s.state {
        SoloState::Config(cfg) => render_config(svc, user, cfg).await,
        SoloState::InSimon { spec, seq, cursor, result, reveal_until, reveals_left } => {
            render_simon(spec, seq, *cursor, *result, *reveal_until, *reveals_left)
        }
        SoloState::Resolved(view) => render_outcome(view),
    }
}

async fn render_config(
    svc: &CrimeService,
    user: UserId,
    cfg: &SoloHeistConfig,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let p = svc.repo.get_or_create(user.get());
    let chosen: HashSet<ItemKey> = cfg.items.iter().copied().collect();

    // KROKI kreatora
    let step_mode = chip_step("1️⃣ Tryb", cfg.mode.is_some());
    let step_risk = chip_step("2️⃣ Ryzyko", cfg.risk.is_some());
    let step_eq   = chip_step(&format!("3️⃣ Ekwipunek {} / 3", cfg.items.len()), !cfg.items.is_empty());
    let step_go   = chip_step("4️⃣ Start", cfg.mode.is_some() && cfg.risk.is_some());

    // Chipy/preset
    let mode_chip = cfg.mode.map(|m| format!("`{}` {}", mode_label(m), emoji_for_mode(m))).unwrap_or("`—`".into());
    let risk_chip = cfg.risk.map(|r| format!("`{:?}` {}", r, emoji_for_risk(r))).unwrap_or("`—`".into());
    let mg_chip   = format!("`Simon` {}", emoji_for_minigame(MinigameKind::Simon));
    let bag_bar   = bag_bar3(cfg.items.len() as u32, 3);

    // Prognoza & preview (jeśli mamy m+r)
    let mut forecast = "—".to_string();
    let mut mg_preview = "—".to_string();
    if let (Some(m), Some(r)) = (cfg.mode, cfg.risk) {
        let (min_r, max_r) = crate::engine::balance::reward_range(m, r);
        let base_chance = crate::engine::balance::base_chance(m, r) * 100.0;

        let eff = items::aggregate(&cfg.items);
        let spec = minigames::simon_spec_for(r, eff.simon_seq_delta);
        mg_preview = format!("🧠 Simon • Długość **{}** • Alfabet **{}**", spec.length, spec.alphabet.len());

        forecast = format!(
            "Szansa bazowa: **{:.0}%**\nWidełki łupu: **{}–{}**",
            base_chance, min_r, max_r
        );
    }

    // Wybrane itemy (z krótkim opisem)
    let items_str = if cfg.items.is_empty() {
        "—".into()
    } else {
        cfg.items
            .iter()
            .map(|k| format!("{} {} — {}", emoji_for_item(*k), items::item_name(*k), item_short_desc(*k)))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let description = format!(
        "**Kreator napadu (SOLO)**\n\
         {step_mode}  {step_risk}  {step_eq}  {step_go}\n\n\
         **Preset**  {mode_chip} • {risk_chip} • {mg_chip}\n\
         **Pojemność**  {bag_bar}"
    );

    let e = CreateEmbed::new()
        .title("🧭 Plan napadu — konfiguracja")
        .description(description)
        .color(0x3b82f6)
        .field("🔮 Prognoza", forecast, true)
        .field("🕹️ Minigra (podgląd)", mg_preview, true)
        .field("🎒 Ekwipunek (max 3)", items_str, false);

    let mut rows: Vec<CreateActionRow> = Vec::new();
    rows.push(row_modes_cfg(cfg));
    rows.push(row_risks_cfg(cfg));
    rows.push(row_select_items(p.pp, &chosen));

    // Start / Reset
    let can_start = cfg.mode.is_some() && cfg.risk.is_some();
    let mut start = CreateButton::new("crime:solo:start")
        .label("🚀 Start napadu")
        .style(ButtonStyle::Success);
    if !can_start { start = start.disabled(true); }
    rows.push(CreateActionRow::Buttons(vec![
        start,
        CreateButton::new("crime:solo:reset")
            .label("♻️ Reset")
            .style(ButtonStyle::Secondary),
    ]));

    (e, rows)
}

fn render_simon(
    spec: &SimonSpec,
    seq: &[char],
    cursor: usize,
    result: Option<MinigameResult>,
    reveal_until: Option<Instant>,
    reveals_left: u8,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let total = seq.len();
    let hit = cursor.min(total);
    let now = Instant::now();
    let reveal_active = reveal_until.map(|t| now < t).unwrap_or(false);

    let status_chip = if reveal_active {
        "👁️ `PODGLĄD`"
    } else {
        match result {
            Some(MinigameResult::Success)            => "✅ `SUKCES`",
            Some(MinigameResult::Fail)               => "❌ `PORAŻKA`",
            Some(MinigameResult::Partial(_))         => "🟡 `CZĘŚCIOWO`",
            Some(MinigameResult::NotPlayed) | None   => "🕹️ `W TRAKCIE`",
        }
    };

    let shown: String = if reveal_active {
        seq.iter().map(|c| format!("`{}`", c)).collect::<Vec<_>>().join(" ")
    } else {
        seq.iter().enumerate()
            .map(|(i, c)| if i < hit { format!("`{}`", c) } else { "`?`".to_string() })
            .collect::<Vec<_>>()
            .join(" ")
    };

    let hud = format!(
        "`Długość:` **{}**   •   `Alfabet:` **{}**\n\
         `Postęp:` **{}/{}**   •   {}\n\
         `Progres:` {}",
        spec.length,
        spec.alphabet.len(),
        hit, total,
        status_chip,
        progress_bar(hit, total),
    );

    let (title, color) = match result {
        Some(MinigameResult::Success) => ("🧠 Simon Says — WYGRANA!", 0x2ecc71),
        Some(MinigameResult::Fail)    => ("🧠 Simon Says — Porażka", 0xe74c3c),
        _                             => ("🧠 Simon Says — powtórz sekwencję", 0xf39c12),
    };

    let e = CreateEmbed::new()
        .title(title)
        .color(color)
        .field("HUD", hud, false)
        .field("Sekwencja", if total == 0 { "—".into() } else { shown }, false)
        .footer(serenity::all::CreateEmbedFooter::new(
            "Wciśnij przyciski poniżej w poprawnej kolejności.",
        ));

    // Klawiatura
    let mut rows = keyboard_rows_from_chars(spec.alphabet, result.is_some());

    // Podgląd + rozstrzygnięcie + (disabled) reset podczas gry
    let mut reveal_btn = CreateButton::new("crime:solo:simon_reveal")
        .label(format!("👁️ Pokaż sekwencję ({})", reveals_left))
        .style(ButtonStyle::Secondary);

    if reveals_left == 0 || reveal_active {
        reveal_btn = reveal_btn.disabled(true);
    }

    rows.push(CreateActionRow::Buttons(vec![
        reveal_btn,
        CreateButton::new("crime:solo:resolve")
            .label("✅ Rozstrzygnij napad")
            .style(ButtonStyle::Primary)
            .disabled(result.is_none()),
        CreateButton::new("crime:solo:reset")
            .label("↩️ Konfiguracja")
            .style(ButtonStyle::Secondary)
            .disabled(true),
    ]));

    (e, rows)
}

// ===== Pomocnicze dla Simon / UI =====

fn keyboard_rows_from_chars(chars: &[char], disabled: bool) -> Vec<CreateActionRow> {
    let mut buttons = Vec::new();
    for &ch in chars {
        let mut b = CreateButton::new(format!("crime:solo:simon_key:{ch}"))
            .label(ch.to_string())
            .style(ButtonStyle::Secondary);
        if disabled { b = b.disabled(true); }
        buttons.push(b);
    }
    rows_from_buttons(buttons)
}

fn rows_from_buttons(mut buttons: Vec<CreateButton>) -> Vec<CreateActionRow> {
    let mut rows = Vec::new();
    while !buttons.is_empty() {
        let take = buttons.split_off(buttons.len().saturating_sub(5));
        let mut chunk = take;
        chunk.reverse();
        rows.push(CreateActionRow::Buttons(chunk));
    }
    rows.reverse();
    rows
}

// =================== Raport ===================

fn render_outcome(v: &ResolvedView) -> (CreateEmbed, Vec<CreateActionRow>) {
    let success = v.outcome.success;

    let tk_delta = v.outcome.amount_final;
    let saldo_before = v.before.balance;
    let saldo_after = v.after.balance;

    let heat_before = v.before.heat.max(0) as u32;
    let heat_after  = v.after.heat.max(0) as u32;

    let pp_before = v.before.pp;
    let pp_after  = v.after.pp;

    let sk_before = v.before.thief_skill as i64;
    let sk_after  = v.after.thief_skill as i64;

    let used_items = if v.cfg.items.is_empty() {
        "—".into()
    } else {
        v.cfg.items
            .iter()
            .map(|k| format!("{} {} — {}", emoji_for_item(*k), items::item_name(*k), item_short_desc(*k)))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let newly = if v.newly_unlocked.is_empty() {
        "—".into()
    } else {
        v.newly_unlocked
            .iter()
            .map(|k| format!("🎁 {}", items::item_name(*k)))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mode_chip  = v.cfg.mode.map(|m| format!("`{}` {}", mode_label(m), emoji_for_mode(m))).unwrap_or("`—`".into());
    let risk_chip  = v.cfg.risk.map(|r| format!("`{:?}` {}", r, emoji_for_risk(r))).unwrap_or("`—`".into());
    let mg_chip    = format!("`Simon` {}", emoji_for_minigame(MinigameKind::Simon));

    let heat_gauge_before = bar10(heat_before.min(100));
    let heat_gauge_after  = bar10(heat_after.min(100));

    let perf_medal = match (success, v.mg) {
        (true, MinigameResult::Success) => "🏅 **Gold**",
        (true, MinigameResult::Partial(_)) => "🥈 **Silver**",
        (true, _) => "🥉 **Bronze**",
        (false, MinigameResult::Success | MinigameResult::Partial(_)) => "🧯 **Clutch**",
        _ => "💤 —",
    };

    let title = if success { "🏆 SUKCES — Raport z napadu" } else { "💥 PORAŻKA — Raport z napadu" };
    let color = if success { 0x2ecc71 } else { 0xe74c3c };

    let summary = format!(
        "**Konfiguracja**  {mode_chip} • {risk_chip} • {mg_chip}\n\
         **Wynik**         {perf_medal}\n\
         **Przedmioty**\n{used_items}",
    );

    let finance_block = format!(
        "```ansi\n\
         \x1b[1mTK Delta\x1b[0m     {:+}\n\
         \x1b[1mSaldo\x1b[0m        {}  →  {}  ({:+})\n\
         ```",
        tk_delta,
        saldo_before,
        saldo_after,
        saldo_after - saldo_before
    );

    let heat_block = format!(
        "```ansi\n\
         \x1b[1mHEAT\x1b[0m    {:>3}%  {}\n\
                    ↓\n\
                  {:>3}%  {}\n\
         ```",
        heat_before.min(100),
        heat_gauge_before,
        heat_after.min(100),
        heat_gauge_after
    );

    let progress_block = format!(
        "```ansi\n\
         \x1b[1mPP\x1b[0m           {:>3}  →  {:>3}  ({:+})\n\
         \x1b[1mUmiejętność\x1b[0m  {:>3}  →  {:>3}  ({:+})\n\
         \x1b[1mMinigierka\x1b[0m   {:?}\n\
         ```",
        pp_before,
        pp_after,
        (pp_after as i64 - pp_before as i64),
        sk_before,
        sk_after,
        (sk_after - sk_before),
        v.mg
    );

    let e = CreateEmbed::new()
        .title(title)
        .color(color)
        .description(summary)
        .field("💰 Łup / Saldo", finance_block, true)
        .field("🔥 HEAT", heat_block, true)
        .field("📈 Postęp", progress_block, true)
        .field("🎁 Nowo odblokowane", newly, false)
        .footer(serenity::all::CreateEmbedFooter::new("Użyj przycisku poniżej, aby zagrać ponownie."));

    let rows = vec![CreateActionRow::Buttons(vec![
        CreateButton::new("crime:solo:reset")
            .label("🔁 Zagraj ponownie")
            .style(ButtonStyle::Success),
    ])];

    (e, rows)
}

// =================== DB helpers (saldo + profil + ustawienia) ===================

async fn ensure_row_users(db: &PgPool, user_id: u64) -> Result<()> {
    sqlx::query(r#"INSERT INTO users (id, balance) VALUES ($1, 0) ON CONFLICT (id) DO NOTHING"#)
        .bind(user_id as i64)
        .execute(db)
        .await?;
    Ok(())
}

/// Pobierz saldo z DB (tworzy wiersz jeśli brak)
async fn fetch_balance(db: &PgPool, user_id: u64) -> Result<i64> {
    ensure_row_users(db, user_id).await?;
    let bal = sqlx::query_scalar::<_, i64>(r#"SELECT balance FROM users WHERE id = $1"#)
        .bind(user_id as i64)
        .fetch_one(db)
        .await?;
    Ok(bal)
}

/// Dodaj delta do salda w DB. Zwraca saldo „po”.
async fn add_balance(db: &PgPool, user_id: u64, delta: i64) -> Result<i64> {
    ensure_row_users(db, user_id).await?;
    let new_bal = sqlx::query_scalar::<_, i64>(
        r#"UPDATE users SET balance = balance + $1 WHERE id = $2 RETURNING balance"#,
    )
    .bind(delta)
    .bind(user_id as i64)
    .fetch_one(db)
    .await?;
    Ok(new_bal)
}

// ---- Profile (HEAT/PP/skill) ----

#[derive(Debug, Clone, Default)]
struct DbProfile {
    heat: i32,
    pp: i32,
    thief_skill: i32,
}

async fn ensure_row_profiles(db: &PgPool, user_id: u64) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO profiles (user_id, heat, pp, thief_skill)
           VALUES ($1, 0, 0, 0)
           ON CONFLICT (user_id) DO NOTHING"#,
    )
    .bind(user_id as i64)
    .execute(db)
    .await?;
    Ok(())
}

async fn load_profile_db(db: &PgPool, user_id: u64) -> Result<PlayerProfile> {
    ensure_row_profiles(db, user_id).await?;
    let rec = sqlx::query_as::<_, (i32, i32, i32)>(
        r#"SELECT heat, pp, thief_skill FROM profiles WHERE user_id = $1"#,
    )
    .bind(user_id as i64)
    .fetch_one(db)
    .await?;

    // balance dociągamy osobno, tutaj 0 (uzupełniany przy użyciu)
    Ok(PlayerProfile {
        user_id,
        balance: 0,
        heat: rec.0 as i64,
        pp: rec.1 as u32,
        thief_skill: rec.2 as u32,
    })
}

async fn save_profile_db(db: &PgPool, user_id: u64, p: &PlayerProfile) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO profiles (user_id, heat, pp, thief_skill)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (user_id) DO UPDATE
           SET heat = EXCLUDED.heat,
               pp = EXCLUDED.pp,
               thief_skill = EXCLUDED.thief_skill,
               updated_at = now()"#,
    )
    .bind(user_id as i64)
    .bind(p.heat)
    .bind(p.pp as i32)
    .bind(p.thief_skill as i32)
    .execute(db)
    .await?;
    Ok(())
}

// ---- Ustawienia (mode/risk/items) ----

#[derive(Debug, Clone)]
struct DbSettings {
    mode: Option<CrimeMode>,
    risk: Option<Risk>,
    items: Vec<ItemKey>,
}

async fn ensure_row_settings(db: &PgPool, user_id: u64) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO crime_settings (user_id, mode, risk, loadout)
           VALUES ($1, NULL, NULL, ARRAY[]::TEXT[])
           ON CONFLICT (user_id) DO NOTHING"#,
    )
    .bind(user_id as i64)
    .execute(db)
    .await?;
    Ok(())
}

fn mode_to_str(m: CrimeMode) -> &'static str {
    match m {
        CrimeMode::Standard  => "standard",
        CrimeMode::Szybki    => "szybki",
        CrimeMode::Ostrozny  => "ostrozny",
        CrimeMode::Shadow    => "shadow",
        CrimeMode::Hardcore  => "hardcore",
        CrimeMode::Ryzykowny => "ryzykowny",
        CrimeMode::Planowany => "planowany",
        CrimeMode::Szalony   => "szalony",
    }
}
fn risk_to_str(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
        Risk::Hardcore => "hardcore",
    }
}

async fn load_settings_db(db: &PgPool, user_id: u64) -> Result<Option<DbSettings>> {
    ensure_row_settings(db, user_id).await?;
    let row = sqlx::query_as::<_, (Option<String>, Option<String>, Option<Vec<String>>)>(
        r#"SELECT mode, risk, loadout FROM crime_settings WHERE user_id = $1"#,
    )
    .bind(user_id as i64)
    .fetch_optional(db)
    .await?;

    if let Some((mode_s, risk_s, loadout_s)) = row {
        let mode = mode_s.as_deref().map(from_key_mode);
        let risk = risk_s.as_deref().map(from_key_risk);

        let items = loadout_s
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| from_key_item(&s))
            .collect::<Vec<_>>();

        Ok(Some(DbSettings { mode, risk, items }))
    } else {
        Ok(None)
    }
}

async fn save_settings_db(db: &PgPool, user_id: u64, cfg: &SoloHeistConfig) -> Result<()> {
    ensure_row_settings(db, user_id).await?;
    let mode_str: Option<&str> = cfg.mode.map(mode_to_str);
    let risk_str: Option<&str> = cfg.risk.map(risk_to_str);
    let loadout: Vec<&'static str> = cfg.items.iter().map(|k| key_item(*k)).collect();

    sqlx::query(
        r#"INSERT INTO crime_settings (user_id, mode, risk, loadout, updated_at)
           VALUES ($1, $2, $3, $4, now())
           ON CONFLICT (user_id) DO UPDATE
           SET mode = EXCLUDED.mode,
               risk = EXCLUDED.risk,
               loadout = EXCLUDED.loadout,
               updated_at = now()"#,
    )
    .bind(user_id as i64)
    .bind(mode_str)
    .bind(risk_str)
    .bind(loadout)
    .execute(db)
    .await?;
    Ok(())
}

// =================== Helpers UI / keys ===================

fn mode_label(m: CrimeMode) -> &'static str {
    match m {
        CrimeMode::Standard => "Standard",
        CrimeMode::Szybki => "Szybki",
        CrimeMode::Ostrozny => "Ostrożny",
        CrimeMode::Shadow => "Shadow",
        CrimeMode::Hardcore => "Hardcore",
        CrimeMode::Ryzykowny => "Ryzykowny",
        CrimeMode::Planowany => "Planowany",
        CrimeMode::Szalony => "Szalony",
    }
}

// Opisy przedmiotów (krótkie)
fn item_short_desc(k: ItemKey) -> &'static str {
    match k {
        ItemKey::HackerLaptop  => "Ułatwia łamanie — krótsza sekwencja/czas.",
        ItemKey::ProGloves     => "Mniejsze pomyłki — lekka tolerancja wejść.",
        ItemKey::Toolkit       => "Bonus do nagrody / stabilniejszy łup.",
        ItemKey::Adrenaline    => "Po podglądzie łatwiej przez chwilę.",
        ItemKey::SmokeGrenade  => "Dłuższy podgląd sekwencji (1x).",
        ItemKey::LockpickSet   => "Mniejsza kara za porażkę.",
    }
}

fn row_select_items(pp: u32, chosen: &HashSet<ItemKey>) -> CreateActionRow {
    let options = items::ITEM_META
        .iter()
        .map(|(k, meta)| {
            let unlocked = pp >= meta.required_pp;
            let value = key_item(*k);
            let label = if unlocked {
                format!("{}", items::item_name(*k))
            } else {
                format!("🔒 {} (PP:{})", items::item_name(*k), meta.required_pp)
            };
            let desc = if unlocked {
                item_short_desc(*k).to_string()
            } else {
                format!("Wymaga PP:{} • {}", meta.required_pp, item_short_desc(*k))
            };

            let mut o = CreateSelectMenuOption::new(label, value).description(desc);
            if chosen.contains(k) {
                o = o.default_selection(true);
            }
            o
        })
        .collect::<Vec<_>>();

    let menu = CreateSelectMenu::new(
        "crime:solo:itemselect",
        CreateSelectMenuKind::String { options },
    )
    .placeholder("🎒 Wybierz do 3 przedmiotów (opis w dymku)")
    .min_values(0)
    .max_values(3);

    CreateActionRow::SelectMenu(menu)
}

fn row_modes_cfg(cfg: &SoloHeistConfig) -> CreateActionRow {
    let cur = cfg.mode.unwrap_or(CrimeMode::Standard);
    let btn = |label: &str, key: &str, is_cur: bool| {
        let mut b = CreateButton::new(format!("crime:solo:mode:{key}"))
            .label(label)
            .style(ButtonStyle::Secondary);
        if is_cur {
            b = b.style(ButtonStyle::Success);
        }
        b
    };
    CreateActionRow::Buttons(vec![
        btn("Standard", "standard", cur == CrimeMode::Standard),
        btn("Szybki", "szybki", cur == CrimeMode::Szybki),
        btn("Ostrożny", "ostrozny", cur == CrimeMode::Ostrozny),
        btn("Shadow", "shadow", cur == CrimeMode::Shadow),
        btn("Hardcore", "hardcore", cur == CrimeMode::Hardcore),
    ])
}

fn row_risks_cfg(cfg: &SoloHeistConfig) -> CreateActionRow {
    let cur = cfg.risk.unwrap_or(Risk::Medium);
    let btn = |label: &str, key: &str, is_cur: bool| {
        let mut b = CreateButton::new(format!("crime:solo:risk:{key}"))
            .label(label)
            .style(ButtonStyle::Secondary);
        if is_cur {
            b = b.style(ButtonStyle::Success);
        }
        b
    };
    CreateActionRow::Buttons(vec![
        btn("Low", "low", cur == Risk::Low),
        btn("Medium", "medium", cur == Risk::Medium),
        btn("High", "high", cur == Risk::High),
        btn("Hardcore", "hardcore", cur == Risk::Hardcore),
    ])
}

fn from_key_mode(k: &str) -> CrimeMode {
    match k {
        "standard" => CrimeMode::Standard,
        "szybki" => CrimeMode::Szybki,
        "ostrozny" => CrimeMode::Ostrozny,
        "shadow" => CrimeMode::Shadow,
        "hardcore" => CrimeMode::Hardcore,
        "ryzykowny" => CrimeMode::Ryzykowny,
        "planowany" => CrimeMode::Planowany,
        "szalony" => CrimeMode::Szalony,
        _ => CrimeMode::Standard,
    }
}
fn from_key_risk(k: &str) -> Risk {
    match k {
        "low" => Risk::Low,
        "medium" => Risk::Medium,
        "high" => Risk::High,
        "hardcore" => Risk::Hardcore,
        _ => Risk::Medium,
    }
}
fn from_key_item(k: &str) -> Option<ItemKey> {
    Some(match k {
        "laptop" => ItemKey::HackerLaptop,
        "gloves" => ItemKey::ProGloves,
        "toolkit" => ItemKey::Toolkit,
        "adrenaline" => ItemKey::Adrenaline,
        "smoke" => ItemKey::SmokeGrenade,
        "lockpick" => ItemKey::LockpickSet,
        _ => return None,
    })
}
fn key_item(k: ItemKey) -> &'static str {
    match k {
        ItemKey::HackerLaptop  => "laptop",
        ItemKey::ProGloves     => "gloves",
        ItemKey::Toolkit       => "toolkit",
        ItemKey::Adrenaline    => "adrenaline",
        ItemKey::SmokeGrenade  => "smoke",
        ItemKey::LockpickSet   => "lockpick",
    }
}

fn extract_cfg(s: &SoloSession) -> SoloHeistConfig {
    match &s.state {
        SoloState::Config(cfg) => cfg.clone(),
        _ => s.base_cfg.clone(),
    }
}

fn bar10(value_0_100: u32) -> String {
    let width = 10u32;
    let filled = ((value_0_100.min(100) as u32 * width) + 99) / 100;
    let mut s = String::with_capacity(10);
    for i in 0..width {
        if i < filled { s.push('▰'); } else { s.push('▱'); }
    }
    s
}

fn emoji_for_item(i: ItemKey) -> &'static str {
    match i {
        ItemKey::HackerLaptop  => "💻",
        ItemKey::ProGloves     => "🧤",
        ItemKey::Toolkit       => "🧰",
        ItemKey::Adrenaline    => "⚗️",
        ItemKey::SmokeGrenade  => "💨",
        ItemKey::LockpickSet   => "🗝️",
    }
}

fn emoji_for_risk(r: Risk) -> &'static str {
    match r {
        Risk::Low      => "🟢",
        Risk::Medium   => "🟡",
        Risk::High     => "🟠",
        Risk::Hardcore => "🔴",
    }
}

fn emoji_for_mode(m: CrimeMode) -> &'static str {
    match m {
        CrimeMode::Standard  => "⚙️",
        CrimeMode::Szybki    => "⚡",
        CrimeMode::Ostrozny  => "👣",
        CrimeMode::Shadow    => "🌑",
        CrimeMode::Hardcore  => "🔥",
        CrimeMode::Ryzykowny => "🎲",
        CrimeMode::Planowany => "🗺️",
        CrimeMode::Szalony   => "🤪",
    }
}

fn emoji_for_minigame(k: MinigameKind) -> &'static str {
    match k {
        MinigameKind::Qte   => "🎯", // nieużywane, ale zostawione dla kompletności
        MinigameKind::Simon => "🧠",
    }
}

fn chip_step(label: &str, done: bool) -> String {
    if done { format!("`{label}` ✅") } else { format!("`{label}` ⬜") }
}

fn bag_bar3(cur: u32, maxv: u32) -> String {
    let cur = cur.min(maxv);
    let mut s = String::new();
    s.push_str("🎒 [");
    for i in 0..maxv {
        if i < cur { s.push('▰'); } else { s.push('▱'); }
    }
    s.push_str(&format!("] {}/{}", cur, maxv));
    s
}

fn progress_bar(current: usize, total: usize) -> String {
    let total = total.max(1);
    let done = current.min(total);
    let pct = ((done * 100) / total) as u32; // 0..100
    format!("[{}] {}/{}", bar10(pct), done, total)
}

fn simon_preview_ms(risk: Risk, len: usize, time_mult: f32) -> u64 {
    let per_char_ms: u64 = match risk {
        Risk::Low      => 950,
        Risk::Medium   => 750,
        Risk::High     => 550,
        Risk::Hardcore => 380,
    };
    let base = (per_char_ms as f32 * time_mult).round() as u64;
    let total = base.saturating_mul(len as u64);
    total.clamp(500, 12_000)
}

// ===== TEMP: auto-DDL bootstrap (usuń po utworzeniu tabel) ===================

/// Utwórz wymagane tabele, jeśli jeszcze nie istnieją.
pub async fn ensure_schema_all(db: &PgPool) -> Result<()> {
    // 1) users: saldo
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS users (
            id BIGINT PRIMARY KEY,
            balance BIGINT NOT NULL DEFAULT 0,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )
    "#).execute(db).await?;

    // 2) profiles: HEAT / PP / skill
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS profiles (
            user_id     BIGINT PRIMARY KEY,
            heat        INTEGER NOT NULL DEFAULT 0,
            pp          INTEGER NOT NULL DEFAULT 0,
            thief_skill INTEGER NOT NULL DEFAULT 0,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )
    "#).execute(db).await?;

    // 3) crime_settings: ostatnie ustawienia gry
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS crime_settings (
            user_id    BIGINT PRIMARY KEY REFERENCES profiles(user_id) ON DELETE CASCADE,
            mode       TEXT NULL,                         -- "standard" | "szybki" | ...
            risk       TEXT NULL,                         -- "low" | "medium" | "high" | "hardcore"
            loadout    TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],  -- ["laptop","gloves",...]
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )
    "#).execute(db).await?;

    Ok(())
}