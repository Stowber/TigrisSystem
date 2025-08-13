use super::types::ItemKey;

/// Skumulowany efekt przedmiotów.
#[derive(Debug, Clone, Copy, Default)]
pub struct ItemEffects {
    // używane w UI/minigrach
    pub qte_window_mult: f32,   // 1.10 => +10% tolerancji w QTE
    pub qte_grace_ms: i32,      // stała tolerancja w ms (okno „grace”)
    pub simon_seq_delta: i32,   // -1 => krótsza sekwencja
    pub simon_time_mult: f32,   // 1.10 => +10% czasu na Simon

    pub timer_extend_pct: f32,  // +X% na timery heistu
    pub heat_reduce_pct: f32,   // -X% końcowego HEAT
    pub payout_bonus_pct: f32,  // +X% do wypłaty

    // *** nowe pola, żeby core.rs się kompilował ***
    pub success_pp_bonus: f32,  // +X% do szansy /PP (jeśli wykorzystywane)
    pub heat_mult: f32,         // mnożnik HEAT (1.0 = brak zmiany)
    pub fail_penalty_mult: f32, // mnożnik kary przy failu (1.0 = brak zmiany)
}

/// Progi odblokowań i nazwy
#[derive(Debug, Clone, Copy)]
pub struct ItemMeta {
    pub name: &'static str,
    pub required_pp: u32,
}

pub const ITEM_META: &[(ItemKey, ItemMeta)] = &[
    (ItemKey::LockpickSet, ItemMeta { name: "Zestaw wytrychów", required_pp: 0  }),
    (ItemKey::ProGloves,   ItemMeta { name: "Rękawice PRO",     required_pp: 5  }),
    (ItemKey::Toolkit,     ItemMeta { name: "Zestaw narzędzi",  required_pp: 10 }),
    (ItemKey::SmokeGrenade,ItemMeta { name: "Granat dymny",     required_pp: 15 }),
    (ItemKey::HackerLaptop,ItemMeta { name: "Laptop hakera",    required_pp: 22 }),
    (ItemKey::Adrenaline,  ItemMeta { name: "Adrenalina",       required_pp: 30 }),
];

#[inline]
pub fn item_name(k: ItemKey) -> &'static str {
    ITEM_META.iter().find(|(kk, _)| *kk == k).map(|(_, m)| m.name).unwrap_or("Przedmiot")
}

#[inline]
pub fn required_pp(k: ItemKey) -> u32 {
    ITEM_META.iter().find(|(kk, _)| *kk == k).map(|(_, m)| m.required_pp).unwrap_or(0)
}

#[inline]
pub fn available_items(pp: u32) -> Vec<ItemKey> {
    ITEM_META.iter().filter(|(_, m)| pp >= m.required_pp).map(|(k, _)| *k).collect()
}

/// Agregacja efektów
pub fn aggregate(items: &[ItemKey]) -> ItemEffects {
    let mut eff = ItemEffects {
        qte_window_mult: 1.0,
        qte_grace_ms: 0,
        simon_seq_delta: 0,
        simon_time_mult: 1.0,
        timer_extend_pct: 0.0,
        heat_reduce_pct: 0.0,
        payout_bonus_pct: 0.0,

        success_pp_bonus: 0.0,
        heat_mult: 1.0,
        fail_penalty_mult: 1.0,
    };

    for it in items {
        match it {
            ItemKey::HackerLaptop => {
                eff.qte_grace_ms += 40;
                eff.qte_window_mult *= 1.10;
            }
            ItemKey::ProGloves => {
                eff.simon_seq_delta -= 1;      // precyzja
                eff.simon_time_mult *= 1.05;   // trochę więcej czasu
            }
            ItemKey::Toolkit => {
                eff.payout_bonus_pct += 0.05;  // „czyściej” = lepszy łup
            }
            ItemKey::Adrenaline => {
                eff.qte_window_mult *= 1.05;
                eff.simon_time_mult *= 1.08;
                eff.fail_penalty_mult *= 0.9;  // mniejszy „tilt” na failu
                eff.heat_mult *= 1.05;         // ale lekko bardziej ryzykowne
            }
            ItemKey::SmokeGrenade => {
                eff.heat_reduce_pct += 0.08;   // mniej HEAT
                eff.timer_extend_pct += 0.05;  // łatwiejsza ewakuacja
            }
            ItemKey::LockpickSet => {
                eff.simon_seq_delta -= 1;
            }
        }
    }

    clamp_effects(&mut eff);
    eff
}

fn clamp_effects(e: &mut ItemEffects) {
    e.qte_window_mult = e.qte_window_mult.clamp(0.9, 1.5);
    e.qte_grace_ms = e.qte_grace_ms.clamp(0, 120);
    e.simon_seq_delta = e.simon_seq_delta.clamp(-2, 0);
    e.simon_time_mult = e.simon_time_mult.clamp(1.0, 1.3);

    e.timer_extend_pct = e.timer_extend_pct.clamp(0.0, 0.25);
    e.heat_reduce_pct = e.heat_reduce_pct.clamp(0.0, 0.15);
    e.payout_bonus_pct = e.payout_bonus_pct.clamp(0.0, 0.15);

    e.success_pp_bonus = e.success_pp_bonus.clamp(0.0, 0.15);
    e.heat_mult = e.heat_mult.clamp(0.8, 1.2);
    e.fail_penalty_mult = e.fail_penalty_mult.clamp(0.7, 1.2);
}
