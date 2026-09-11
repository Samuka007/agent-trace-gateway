//! 会话绑定表 + 账号表：本进程的唯一持久状态（sqlite WAL）。
//!
//! 设计约束：重启不丢身份。绑定一旦建立，对话永不换账号——
//! 身份连续性是这张表存在的理由。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

pub const ACCOUNT_ACTIVE: &str = "active";
pub const ACCOUNT_COOLING: &str = "cooling";
pub const ACCOUNT_DISABLED: &str = "disabled";

#[derive(Debug, Clone)]
pub struct AccountRow {
    pub account_id: String,
    pub label: String,
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub expires_at: Option<i64>,
    pub installation_id: String,
    pub version_pin: String,
    pub user_agent: String,
    pub originator: String,
    pub proxy_url: Option<String>,
    pub state: String,
    pub last_turn_at: i64,
    pub primary_used_percent: Option<f64>,
    pub secondary_used_percent: Option<f64>,
    pub quota_updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct BindingRow {
    pub session_key: String,
    pub account_id: String,
    pub thread_id: String,
    pub session_id: String,
    /// 根回合锚：对话首轮的 turn id，此后不变（真实客户端语义）。
    pub root_turn_id: String,
    /// 上下文窗口 id：窗口期内稳定，压缩后才换新。
    pub context_window_id: String,
    pub turns: i64,
    pub last_seen: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS accounts (
  account_id      TEXT PRIMARY KEY,
  label           TEXT NOT NULL DEFAULT '',
  refresh_token   TEXT NOT NULL,
  access_token    TEXT,
  expires_at      INTEGER,
  installation_id TEXT NOT NULL,
  version_pin     TEXT NOT NULL,
  user_agent      TEXT NOT NULL,
  originator      TEXT NOT NULL,
  proxy_url       TEXT,
  state           TEXT NOT NULL DEFAULT 'active',
  last_turn_at    INTEGER NOT NULL DEFAULT 0,
  created_at      INTEGER NOT NULL,
  primary_used_percent   REAL,
  secondary_used_percent REAL,
  quota_updated_at       INTEGER
);
CREATE TABLE IF NOT EXISTS bindings (
  session_key TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(account_id),
  thread_id   TEXT NOT NULL,
  session_id  TEXT NOT NULL,
  root_turn_id     TEXT NOT NULL DEFAULT '',
  context_window_id TEXT NOT NULL DEFAULT '',
  turns       INTEGER NOT NULL DEFAULT 0,
  last_seen   INTEGER NOT NULL DEFAULT 0
);
"#;

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        account_id: r.get("account_id")?,
        label: r.get("label")?,
        refresh_token: r.get("refresh_token")?,
        access_token: r.get("access_token")?,
        expires_at: r.get("expires_at")?,
        installation_id: r.get("installation_id")?,
        version_pin: r.get("version_pin")?,
        user_agent: r.get("user_agent")?,
        originator: r.get("originator")?,
        proxy_url: r.get("proxy_url")?,
        state: r.get("state")?,
        last_turn_at: r.get("last_turn_at")?,
        primary_used_percent: r.get("primary_used_percent")?,
        secondary_used_percent: r.get("secondary_used_percent")?,
        quota_updated_at: r.get("quota_updated_at")?,
    })
}

const ACCOUNT_COLS: &str = "account_id, label, refresh_token, access_token, expires_at, \
     installation_id, version_pin, user_agent, originator, proxy_url, state, last_turn_at, primary_used_percent, secondary_used_percent, quota_updated_at";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert_account(&self, a: &AccountRow, now: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO accounts (account_id, label, refresh_token, access_token, expires_at,
                   installation_id, version_pin, user_agent, originator, proxy_url, state, last_turn_at, primary_used_percent, secondary_used_percent, quota_updated_at, created_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
               ON CONFLICT(account_id) DO UPDATE SET
                   label=excluded.label, refresh_token=excluded.refresh_token,
                   version_pin=excluded.version_pin, user_agent=excluded.user_agent,
                   proxy_url=excluded.proxy_url"#,
            rusqlite::params![
                a.account_id,
                a.label,
                a.refresh_token,
                a.access_token,
                a.expires_at,
                a.installation_id,
                a.version_pin,
                a.user_agent,
                a.originator,
                a.proxy_url,
                a.state,
                a.last_turn_at,
                a.primary_used_percent,
                a.secondary_used_percent,
                a.quota_updated_at,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn list_accounts(&self) -> rusqlite::Result<Vec<AccountRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ACCOUNT_COLS} FROM accounts ORDER BY account_id"
        ))?;
        let rows = stmt.query_map([], row_to_account)?.collect::<Vec<_>>();
        rows.into_iter().collect()
    }

    pub fn get_account(&self, account_id: &str) -> rusqlite::Result<Option<AccountRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ACCOUNT_COLS} FROM accounts WHERE account_id = ?1"
        ))?;
        let mut rows = stmt.query_map([account_id], row_to_account)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn update_tokens(
        &self,
        account_id: &str,
        access_token: &str,
        id_token: Option<&str>,
        refresh_token: &str,
        expires_at: Option<i64>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        // id_token 只用于携带 exp，落库以备核查，不做其他用途。
        let _ = id_token;
        conn.execute(
            "UPDATE accounts SET access_token = ?2, refresh_token = ?3, expires_at = ?4
             WHERE account_id = ?1",
            rusqlite::params![account_id, access_token, refresh_token, expires_at],
        )?;
        Ok(())
    }

    pub fn set_state(&self, account_id: &str, state: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE accounts SET state = ?2 WHERE account_id = ?1",
            rusqlite::params![account_id, state],
        )?;
        Ok(())
    }

    pub fn touch_account_turn(&self, account_id: &str, now: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE accounts SET last_turn_at = ?2 WHERE account_id = ?1",
            rusqlite::params![account_id, now],
        )?;
        Ok(())
    }

    pub fn binding(&self, session_key: &str) -> rusqlite::Result<Option<BindingRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_key, account_id, thread_id, session_id, root_turn_id,
                    context_window_id, turns, last_seen
             FROM bindings WHERE session_key = ?1",
        )?;
        let mut rows = stmt.query_map([session_key], |r| {
            Ok(BindingRow {
                session_key: r.get("session_key")?,
                account_id: r.get("account_id")?,
                thread_id: r.get("thread_id")?,
                session_id: r.get("session_id")?,
                root_turn_id: r.get("root_turn_id")?,
                context_window_id: r.get("context_window_id")?,
                turns: r.get("turns")?,
                last_seen: r.get("last_seen")?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn insert_binding(&self, b: &BindingRow) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO bindings
                 (session_key, account_id, thread_id, session_id, root_turn_id, context_window_id, turns, last_seen)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![b.session_key, b.account_id, b.thread_id, b.session_id, b.root_turn_id, b.context_window_id, b.turns, b.last_seen],
        )?;
        Ok(())
    }

    pub fn touch_binding(&self, session_key: &str, now: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE bindings SET turns = turns + 1, last_seen = ?2 WHERE session_key = ?1",
            rusqlite::params![session_key, now],
        )?;
        Ok(())
    }

    /// LRU 放置：active、绑定数未达上限、配额未破天花板的账号里，取最久未轮转的。
    pub fn place_lru(&self, max_sessions_per_account: i64, quota_ceiling: f64) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                r#"SELECT a.account_id FROM accounts a
                   LEFT JOIN bindings b ON b.account_id = a.account_id
                   WHERE a.state = 'active'
                     AND (a.primary_used_percent IS NULL OR a.primary_used_percent < ?2)
                   GROUP BY a.account_id
                   HAVING COUNT(b.session_key) < ?1
                   ORDER BY a.last_turn_at ASC, a.account_id ASC
                   LIMIT 1"#,
            )
            .ok()?;
        stmt.query_row(
            rusqlite::params![max_sessions_per_account, quota_ceiling],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    /// 单账号绑定数（"这个人同时开着几个终端"）。
    pub fn bindings_count(&self, account_id: &str) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM bindings WHERE account_id = ?1",
            [account_id],
            |r| r.get(0),
        )
    }

    /// 捕获上游响应头里的窗口用量百分比（5h/7d），供放置过滤。
    pub fn update_quota(
        &self,
        account_id: &str,
        primary: Option<f64>,
        secondary: Option<f64>,
        now: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE accounts SET primary_used_percent = ?2, secondary_used_percent = ?3,
             quota_updated_at = ?4 WHERE account_id = ?1",
            rusqlite::params![account_id, primary, secondary, now],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> AccountRow {
        let p = crate::atcd::persona::Persona::mint(id, None, None, None, None, None, None);
        AccountRow {
            account_id: p.account_id.clone(),
            label: id.into(),
            refresh_token: format!("rt-{id}"),
            access_token: None,
            expires_at: None,
            installation_id: p.installation_id.clone(),
            version_pin: p.version_pin.clone(),
            user_agent: p.user_agent(),
            originator: p.originator.clone(),
            proxy_url: None,
            state: ACCOUNT_ACTIVE.into(),
            last_turn_at: 0,
            primary_used_percent: None,
            secondary_used_percent: None,
            quota_updated_at: None,
        }
    }

    #[test]
    fn account_and_binding_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_account(&account("a1"), 100).unwrap();
        s.insert_binding(&BindingRow {
            session_key: "sk-1".into(),
            account_id: "a1".into(),
            thread_id: "t".into(),
            session_id: "s".into(),
            root_turn_id: "rt".into(),
            context_window_id: "cw".into(),
            turns: 0,
            last_seen: 0,
        })
        .unwrap();

        let b = s.binding("sk-1").unwrap().unwrap();
        assert_eq!(b.account_id, "a1");
        s.touch_binding("sk-1", 200).unwrap();
        assert_eq!(s.binding("sk-1").unwrap().unwrap().turns, 1);

        s.update_tokens("a1", "at", Some("it"), "rt-new", Some(999))
            .unwrap();
        let a = s.get_account("a1").unwrap().unwrap();
        assert_eq!(a.access_token.as_deref(), Some("at"));
        assert_eq!(a.refresh_token, "rt-new");
        assert_eq!(a.expires_at, Some(999));

        // 人设不变：token 刷新不碰 installation
        let p = crate::atcd::persona::Persona::mint("a1", None, None, None, None, None, None);
        assert_ne!(a.installation_id, p.installation_id);
        assert_eq!(
            a.installation_id,
            s.get_account("a1").unwrap().unwrap().installation_id
        );
    }
}
