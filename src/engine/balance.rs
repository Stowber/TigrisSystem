use super::types::{CrimeMode, Risk};

pub fn base_chance(mode: CrimeMode, risk: Risk) -> f32 {
    // szansa bazowa (w punktach procentowych), tryb wpływa delikatnie
    let r: f32 = match risk {
        Risk::Low => 62.0_f32,
        Risk::Medium => 52.0_f32,
        Risk::High => 42.0_f32,
        Risk::Hardcore => 32.0_f32,
    };
    let m: f32 = match mode {
        CrimeMode::Standard => 0.0_f32,
        CrimeMode::Szybki => -3.0_f32,
        CrimeMode::Ostrozny => 3.0_f32,
        CrimeMode::Shadow => 2.0_f32,
        CrimeMode::Hardcore => -6.0_f32,
        CrimeMode::Ryzykowny => -4.0_f32,
        CrimeMode::Planowany => 4.0_f32,
        CrimeMode::Szalony => -8.0_f32,
    };
    (r + m).clamp(5.0_f32, 95.0_f32)
}

pub fn reward_range(mode: CrimeMode, risk: Risk) -> (i64, i64) {
    // proste widełki
    let base = match risk {
        Risk::Low => (300, 600),
        Risk::Medium => (600, 1200),
        Risk::High => (1200, 2400),
        Risk::Hardcore => (2400, 4200),
    };
    // tryb lekko moduluje
    let bump: f32 = match mode {
        CrimeMode::Planowany | CrimeMode::Shadow => 1.15,
        CrimeMode::Ostrozny => 1.05,
        CrimeMode::Standard => 1.0,
        CrimeMode::Ryzykowny => 1.1,
        CrimeMode::Szybki => 0.95,
        CrimeMode::Hardcore => 1.2,
        CrimeMode::Szalony => 1.25,
    };
    (((base.0 as f32) * bump) as i64, ((base.1 as f32) * bump) as i64)
}

pub fn heat_gain(risk: Risk) -> i64 {
    match risk {
        Risk::Low => 4,
        Risk::Medium => 7,
        Risk::High => 10,
        Risk::Hardcore => 14,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HeatEffects {
    pub chance_mult: f32,        // mnożnik szansy (× niżej = trudniej)
    pub reward_mult: f32,        // mnożnik łupu
    pub qte_window_mult: f32,    // okno QTE (× niżej = trudniej)
    pub simon_seq_delta: i32,    // +ile znaków do sekwencji
    pub extra_cooldown_secs: u64,// bonusowy CD (sekundy)
    pub ambush_chance_pct: u8,   // % na "Zasadzkę" przy starcie
}

// ---- bazowe progi HEAT (niezależnie od risk/mode) ----
fn base_heat_effects(heat: u32) -> HeatEffects {
    let h = heat.min(100);
    match h {
        0..=24 => HeatEffects { chance_mult: 1.00, reward_mult: 1.00, qte_window_mult: 1.00, simon_seq_delta: 0, extra_cooldown_secs: 0,  ambush_chance_pct: 0 },
        25..=49 => HeatEffects { chance_mult: 0.95, reward_mult: 0.95, qte_window_mult: 0.95, simon_seq_delta: 0, extra_cooldown_secs: 0,  ambush_chance_pct: 0 },
        50..=74 => HeatEffects { chance_mult: 0.90, reward_mult: 0.90, qte_window_mult: 0.85, simon_seq_delta: 1, extra_cooldown_secs: 2,  ambush_chance_pct: 0 },
        75..=89 => HeatEffects { chance_mult: 0.80, reward_mult: 0.85, qte_window_mult: 0.75, simon_seq_delta: 2, extra_cooldown_secs: 5,  ambush_chance_pct: 0 },
        _       => HeatEffects { chance_mult: 0.65, reward_mult: 0.75, qte_window_mult: 0.60, simon_seq_delta: 3, extra_cooldown_secs: 10, ambush_chance_pct: 20 },
    }
}

// ---- wagi od ryzyka (im większe ryzyko, tym mocniej „gryzie” HEAT) ----
fn risk_factor(r: Risk) -> f32 {
    match r {
        Risk::Low      => 0.70,
        Risk::Medium   => 1.00,
        Risk::High     => 1.25,
        Risk::Hardcore => 1.50,
    }
}

// ---- wagi / modyfikacje od trybu (niektóre łagodzą, inne zaostrzają) ----
#[derive(Debug, Clone, Copy)]
struct ModeScale {
    all: f32,          // ogólna „ostrość” (szansa/łup/QTE/CD)
    simon: f32,        // skala dla simon_seq_delta
    ambush: f32,       // skala dla szansy zasadzki
}
fn mode_scale(m: CrimeMode) -> ModeScale {
    match m {
        CrimeMode::Standard  => ModeScale { all: 1.00, simon: 1.00, ambush: 1.00 },
        CrimeMode::Szybki    => ModeScale { all: 1.10, simon: 1.00, ambush: 1.10 },
        CrimeMode::Ostrozny  => ModeScale { all: 0.85, simon: 0.85, ambush: 0.85 },
        CrimeMode::Shadow    => ModeScale { all: 0.90, simon: 0.90, ambush: 0.50 }, // stealth: mniejsza szansa zasadzki
        CrimeMode::Hardcore  => ModeScale { all: 1.60, simon: 1.30, ambush: 1.60 },
        CrimeMode::Ryzykowny => ModeScale { all: 1.25, simon: 1.15, ambush: 1.40 },
        CrimeMode::Planowany => ModeScale { all: 0.90, simon: 0.85, ambush: 0.90 },
        CrimeMode::Szalony   => ModeScale { all: 1.40, simon: 1.25, ambush: 1.50 },
    }
}

// pomoc: miksujemy mnożnik z karą (np. 0.80 = 20% kary), ważoną risk*mode
fn mix_mult(base_mult: f32, rf: f32, ms: f32) -> f32 {
    // kara = 1 - mult (np. 1 - 0.80 = 0.20); skaluje się z rf*ms
    let penalty = (1.0 - base_mult).max(0.0);
    let scaled  = (penalty * rf * ms).clamp(0.0, 0.95);
    (1.0 - scaled).clamp(0.05, 1.25)
}

// główna funkcja do użytku zew.: HEAT + risk + mode => efekty
pub fn heat_effects(mode: CrimeMode, risk: Risk, heat: u32) -> HeatEffects {
    let base = base_heat_effects(heat);
    let rf = risk_factor(risk);
    let ms = mode_scale(mode);

    HeatEffects {
        chance_mult:      mix_mult(base.chance_mult,     rf, ms.all),
        reward_mult:      mix_mult(base.reward_mult,     rf, ms.all),
        qte_window_mult:  mix_mult(base.qte_window_mult, rf, ms.all),
        simon_seq_delta:  ((base.simon_seq_delta as f32) * rf * ms.simon).round() as i32,
        extra_cooldown_secs: (((base.extra_cooldown_secs as f32) * rf * ms.all).round() as u64)
                                .min(60),
        ambush_chance_pct: (((base.ambush_chance_pct as f32) * rf * ms.ambush).round() as u32)
                                .min(100) as u8,
    }
}

// ——— krótkie podsumowanie do UI (embed)
pub fn format_heat_summary(e: HeatEffects) -> String {
    let mut parts = vec![
        format!("Szansa ×{:.2}", e.chance_mult),
        format!("Łup ×{:.2}", e.reward_mult),
        format!("QTE okno ×{:.2}", e.qte_window_mult),
    ];
    if e.simon_seq_delta != 0 { parts.push(format!("Simon +{}", e.simon_seq_delta)); }
    if e.extra_cooldown_secs > 0 { parts.push(format!("+{}s CD", e.extra_cooldown_secs)); }
    if e.ambush_chance_pct > 0 { parts.push(format!("Zasadzka {}%", e.ambush_chance_pct)); }
    parts.join(" • ")
}

