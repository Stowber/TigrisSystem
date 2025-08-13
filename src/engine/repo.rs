use dashmap::DashMap;

use super::types::PlayerProfile;

pub trait SoloRepo {
    fn get_or_create(&self, user_id: u64) -> PlayerProfile;
    fn save(&self, profile: &PlayerProfile);
}

#[derive(Default)]
pub struct MemorySoloRepo {
    users: DashMap<u64, PlayerProfile>,
}

impl MemorySoloRepo {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SoloRepo for MemorySoloRepo {
    fn get_or_create(&self, user_id: u64) -> PlayerProfile {
        if let Some(v) = self.users.get(&user_id) {
            return v.clone();
        }
        let mut p = PlayerProfile::default();
        p.user_id = user_id;
        self.users.insert(user_id, p.clone());
        p
    }

    fn save(&self, profile: &PlayerProfile) {
        self.users.insert(profile.user_id, profile.clone());
    }
}
