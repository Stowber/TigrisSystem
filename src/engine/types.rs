use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrimeMode {
    Standard,
    Szybki,
    Ostrozny,
    Shadow,
    Hardcore,
    Ryzykowny,
    Planowany,
    Szalony,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    Low,
    Medium,
    High,
    Hardcore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MinigameKind {
    Qte,
    Simon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MinigameResult {
    NotPlayed,
    Success,
    Partial(i32), // „ile ms od ideału” (dla QTE), mniejsze = lepsze
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemKey {
    HackerLaptop,     // + okno QTE
    ProGloves,        // - heat
    Toolkit,          // + szansa sukcesu
    Adrenaline,       // - kara za fail
    SmokeGrenade,     // - heat więcej
    LockpickSet,      // + szansa sukcesu
}

#[derive(Debug, Clone, Default)]
pub struct ItemEffects {
    pub qte_window_bonus_ms: i32,
    pub success_pp_bonus: f32,   // w punktach procentowych
    pub heat_mult: f32,          // mnożnik heat (np. 0.85 = -15%)
    pub fail_penalty_mult: f32,  // mnożnik kary przy porażce
    pub simon_len_delta: i32,    // zmiana długości sekwencji
}

#[derive(Debug, Clone)]
pub struct SoloHeistConfig {
    pub mode: Option<CrimeMode>,
    pub risk: Option<Risk>,
    pub minigame: MinigameKind,
    pub items: Vec<ItemKey>,
}

impl Default for SoloHeistConfig {
    fn default() -> Self {
        Self {
            mode: None,
            risk: None,
            minigame: MinigameKind::Qte,
            items: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlayerProfile {
    pub user_id: u64,
    pub balance: i64,
    pub heat: i64,
    pub thief_skill: u32, // 0..50
    pub pp: u32,          // punkty progresu / odblokowania przedmiotów
}

impl Default for PlayerProfile {
    fn default() -> Self {
        Self {
            user_id: 0,
            balance: 0,
            heat: 0,
            thief_skill: 5,
            pp: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeistOutcome {
    pub success: bool,
    pub amount_base: i64,
    pub amount_final: i64,
    pub heat_delta: i64,
}

#[derive(Debug, Clone)]
pub struct QteSpec {
    pub target_ms: i32,
    pub window_ms: i32,
}

#[derive(Debug, Clone)]
pub struct SimonSpec {
    pub length: usize,
    pub alphabet: &'static [char], // np. ['A','B','C','D']
}

#[derive(Debug, Clone)]
pub enum SoloState {
    Config(SoloHeistConfig),
    InQte {
        spec: QteSpec,
        started_at: Option<Instant>,
        result: Option<MinigameResult>,
    },
    InSimon {
        spec: SimonSpec,
        seq: Vec<char>,
        cursor: usize,
        result: Option<MinigameResult>,
    },
    Resolved(HeistOutcome),
}
