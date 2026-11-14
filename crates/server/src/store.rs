//! SQLite persistence: roles and overrides per Spotify account, bans and
//! room settings survive server restarts.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use protocol::{Overrides, Role};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedMember {
    pub role: Role,
    pub overrides: Overrides,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS members (
        spotify_id TEXT PRIMARY KEY,
        role       TEXT NOT NULL,
        overrides  TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS bans (
        key       TEXT PRIMARY KEY,
        name      TEXT NOT NULL,
        banned_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn members(&self) -> rusqlite::Result<HashMap<String, SavedMember>> {
        let mut stmt = self.conn.prepare("SELECT spotify_id, role, overrides FROM members")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, role, overrides) = row?;
            // Rows written by another version are skipped rather than fatal.
            if let (Ok(role), Ok(overrides)) = (serde_json::from_str(&role), serde_json::from_str(&overrides)) {
                out.insert(id, SavedMember { role, overrides });
            }
        }
        Ok(out)
    }

    pub fn save_member(&self, spotify_id: &str, member: &SavedMember) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO members (spotify_id, role, overrides) VALUES (?1, ?2, ?3)
             ON CONFLICT(spotify_id) DO UPDATE SET role = excluded.role, overrides = excluded.overrides",
            params![
                spotify_id,
                serde_json::to_string(&member.role).expect("serializable"),
                serde_json::to_string(&member.overrides).expect("serializable")
            ],
        )?;
        Ok(())
    }

    /// Ban keys (`spotify:<id>` or `ip:<addr>`) with the member's name.
    pub fn bans(&self) -> rusqlite::Result<BTreeMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT key, name FROM bans")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    pub fn add_ban(&self, key: &str, name: &str, at_ms: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO bans (key, name, banned_at) VALUES (?1, ?2, ?3)",
            params![key, name, at_ms as i64],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Perm;

    #[test]
    fn round_trips_members_bans_and_settings() {
        let s = Store::in_memory().unwrap();
        let mut overrides = Overrides::new();
        overrides.insert(Perm::QueueAdd, false);
        let m = SavedMember { role: Role::Dj, overrides };
        s.save_member("sam", &m).unwrap();
        s.save_member("sam", &SavedMember { role: Role::Listener, ..m.clone() }).unwrap();
        assert_eq!(s.members().unwrap()["sam"].role, Role::Listener);
        assert_eq!(s.members().unwrap()["sam"].overrides, m.overrides);

        s.add_ban("spotify:lou", "Lou", 1).unwrap();
        assert_eq!(s.bans().unwrap()["spotify:lou"], "Lou");

        assert_eq!(s.setting("waiting_room").unwrap(), None);
        s.set_setting("waiting_room", "true").unwrap();
        assert_eq!(s.setting("waiting_room").unwrap().as_deref(), Some("true"));
    }
}
