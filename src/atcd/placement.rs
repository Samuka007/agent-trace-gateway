//! 放置调度：新会话选号，放置后由绑定表粘住，永不迁移。
//!
//! 选择标准不是"谁最闲"，而是"这个账号还能自然地多背一个对话"：
//! 健康（active）、容量（绑定数 < 上限）、最久未被使用（摊开负载）。
//! trait 留给策略迭代，默认实现是最简单的 LRU。

use crate::atcd::store::Store;

pub trait Placement: Send + Sync {
    fn place(&self, store: &Store) -> Option<String>;
}

pub struct LruPlacement {
    /// 单账号同时承载的对话数上限——"一个人同时开的终端数"。
    pub max_sessions_per_account: i64,
    /// 5h 窗口用量百分比天花板：临期的号不接新会话。
    pub quota_ceiling_percent: f64,
}

impl Placement for LruPlacement {
    fn place(&self, store: &Store) -> Option<String> {
        let conn_store = store;
        let _ = conn_store;
        store.place_lru(self.max_sessions_per_account, self.quota_ceiling_percent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atcd::persona::Persona;
    use crate::atcd::store::{AccountRow, BindingRow, Store, ACCOUNT_ACTIVE, ACCOUNT_COOLING};

    fn account(id: &str, state: &str, last_turn: i64) -> AccountRow {
        let p = Persona::mint(id, None, None, None, None, None, None);
        AccountRow {
            account_id: id.into(),
            label: id.into(),
            refresh_token: format!("rt-{id}"),
            access_token: None,
            expires_at: None,
            installation_id: p.installation_id.clone(),
            version_pin: p.version_pin.clone(),
            user_agent: p.user_agent(),
            originator: p.originator.clone(),
            proxy_url: None,
            state: state.into(),
            last_turn_at: last_turn,
            primary_used_percent: None,
            secondary_used_percent: None,
            quota_updated_at: None,
        }
    }

    #[test]
    fn skips_cooling_and_capped_picks_lru() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_account(&account("a-recent", ACCOUNT_ACTIVE, 500), 1).unwrap();
        s.upsert_account(&account("a-idle", ACCOUNT_ACTIVE, 100), 1).unwrap();
        s.upsert_account(&account("a-cool", ACCOUNT_COOLING, 0), 1).unwrap();
        s.upsert_account(&account("a-full", ACCOUNT_ACTIVE, 0), 1).unwrap();

        let p = LruPlacement { max_sessions_per_account: 1, quota_ceiling_percent: 85.0 };
        // a-full 已有一个绑定，容量满；a-cool 冷却；先轮到最久未用的 a-idle
        s.insert_binding(&BindingRow {
            session_key: "sk".into(),
            account_id: "a-full".into(),
            thread_id: "t".into(),
            session_id: "s".into(),
            root_turn_id: "rt".into(),
            context_window_id: "cw".into(),
            turns: 0,
            last_seen: 0,
        })
        .unwrap();
        assert_eq!(p.place(&s), Some("a-idle".into()));

        // a-idle 被用过之后（last_turn_at 更新），下一轮轮到 a-recent
        s.touch_account_turn("a-idle", 900).unwrap();
        assert_eq!(p.place(&s), Some("a-recent".into()));

        // 全部不可用 → None
        s.set_state("a-idle", ACCOUNT_COOLING).unwrap();
        s.set_state("a-recent", ACCOUNT_COOLING).unwrap();
        assert_eq!(p.place(&s), None);
    }
}
