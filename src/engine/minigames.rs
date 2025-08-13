use rand::Rng;

use super::types::{MinigameResult, QteSpec, Risk, SimonSpec};

pub fn qte_spec_for(risk: Risk, window_bonus_ms: i32) -> QteSpec {
    // target ok. 1.2s, okno zależne od ryzyka
    let base_window = match risk {
        Risk::Low => 220,
        Risk::Medium => 150,
        Risk::High => 100,
        Risk::Hardcore => 70,
    };
    QteSpec {
        target_ms: 1200,
        window_ms: (base_window + window_bonus_ms).max(40),
    }
}

pub fn score_qte(elapsed_ms: i32, spec: &QteSpec) -> MinigameResult {
    let diff = (elapsed_ms - spec.target_ms).abs();
    if diff <= spec.window_ms {
        MinigameResult::Success
    } else if diff <= spec.window_ms * 2 {
        MinigameResult::Partial(diff)
    } else {
        MinigameResult::Fail
    }
}

pub fn simon_spec_for(risk: Risk, len_delta: i32) -> SimonSpec {
    let base_len = match risk {
        Risk::Low => 4,
        Risk::Medium => 5,
        Risk::High => 6,
        Risk::Hardcore => 7,
    };
    SimonSpec {
        length: (base_len as i32 + len_delta).clamp(3, 8) as usize,
        alphabet: &['A', 'B', 'C', 'D'],
    }
}

pub fn gen_simon_seq(spec: &SimonSpec) -> Vec<char> {
    let mut rng = rand::rng();
    (0..spec.length)
        .map(|_| {
            let i = rng.random_range(0..spec.alphabet.len());
            spec.alphabet[i]
        })
        .collect()
}

pub fn check_simon_step(expected: char, got: char) -> bool {
    expected == got
}
