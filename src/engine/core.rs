use rand::Rng;

use super::{
    balance,
    items::aggregate,
    types::{HeistOutcome, MinigameResult, PlayerProfile, Risk, SoloHeistConfig, CrimeMode},
};

pub fn resolve_solo(
    mut profile: PlayerProfile,
    cfg: &SoloHeistConfig,
    mg: MinigameResult,
) -> (PlayerProfile, HeistOutcome) {
    let mode = cfg.mode.unwrap_or(CrimeMode::Standard);
    let risk = cfg.risk.unwrap_or(Risk::Medium);

    let effects = aggregate(&cfg.items);

    // bazowa szansa
    let mut chance = balance::base_chance(mode, risk);

    // umiejętność 0..50 -> do +15 pp
    chance += (profile.thief_skill as f32 / 50.0) * 15.0;

    // przedmioty
    chance += effects.success_pp_bonus;

    // minigierka
    match mg {
        MinigameResult::Success => chance += 18.0,
        MinigameResult::Partial(diff) => {
            // im bliżej, tym więcej (do +12)
            let bonus = (12.0 - (diff as f32 / 25.0)).clamp(0.0, 12.0);
            chance += bonus;
        }
        MinigameResult::Fail => chance -= 22.0,
        MinigameResult::NotPlayed => chance -= 10.0,
    }

    chance = chance.clamp(1.0, 99.0);

    // losowanie (rand 0.9)
    let roll = rand::rng().random_range(0.0..100.0);
    let success = roll < chance;

    let (min_r, max_r) = balance::reward_range(mode, risk);
    let reward = rand::rng().random_range(min_r..=max_r);

    // HEAT
    let mut heat = balance::heat_gain(risk);
    heat = ((heat as f32) * effects.heat_mult).round() as i64;

    let (amount_base, amount_final, heat_delta) = if success {
        (reward, reward, heat)
    } else {
        let penalty = ((reward as f32) * 0.35 * effects.fail_penalty_mult) as i64;
        (-penalty, -penalty, heat + 2)
    };

    profile.balance += amount_final;
    profile.heat += heat_delta;
    // prosty progres
    if profile.thief_skill < 50 {
        profile.thief_skill += 1;
    }
    if success {
        profile.pp = profile.pp.saturating_add(1);
    }

    (
        profile,
        HeistOutcome {
            success,
            amount_base,
            amount_final,
            heat_delta,
        },
    )
}