use crate::model::{Author, Fields, Kind, Message};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub type VersionVector = BTreeMap<String, i64>;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PrMeta {
    pub title: Option<String>,
    pub head_sha: Option<String>,
    pub state: Option<String>,
    pub author: Option<String>,
    pub fetched_at: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkTable {
    Read,
    Delivered,
}

impl MarkTable {
    fn table(self) -> &'static str {
        match self {
            MarkTable::Read => "reads",
            MarkTable::Delivered => "delivered",
        }
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    origin TEXT NOT NULL,
    seq INTEGER NOT NULL,
    hlc INTEGER NOT NULL,
    thread TEXT NOT NULL,
    kind TEXT NOT NULL,
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    fields TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    UNIQUE (origin, seq)
);
CREATE INDEX IF NOT EXISTS messages_thread ON messages (thread, hlc, id);
CREATE INDEX IF NOT EXISTS messages_hlc ON messages (hlc, id);
CREATE TABLE IF NOT EXISTS reads (agent TEXT NOT NULL, msg TEXT NOT NULL, PRIMARY KEY (agent, msg));
CREATE TABLE IF NOT EXISTS delivered (agent TEXT NOT NULL, msg TEXT NOT NULL, PRIMARY KEY (agent, msg));
CREATE TABLE IF NOT EXISTS pr_meta (thread TEXT PRIMARY KEY, data TEXT NOT NULL);
";

fn row_to_message(row: &rusqlite::Row) -> rusqlite::Result<Message> {
    let kind: String = row.get(5)?;
    let author: String = row.get(6)?;
    let fields: String = row.get(8)?;
    let bad = |e: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    Ok(Message {
        id: row.get(0)?,
        origin: row.get(1)?,
        seq: row.get(2)?,
        hlc: row.get(3)?,
        thread: row.get(4)?,
        kind: serde_json::from_value(serde_json::Value::String(kind)).map_err(bad)?,
        author: serde_json::from_str::<Author>(&author).map_err(bad)?,
        body: row.get(7)?,
        fields: serde_json::from_str::<Fields>(&fields).map_err(bad)?,
    })
}

const COLS: &str = "id, origin, seq, hlc, thread, kind, author, body, fields";

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn node_id(&self) -> Result<String> {
        if let Some(id) = self.meta("node_id")? {
            return Ok(id);
        }
        let id = crate::model::new_id();
        self.set_meta("node_id", &id)?;
        Ok(id)
    }

    pub fn max_hlc(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COALESCE(MAX(hlc), 0) FROM messages", [], |r| r.get(0))?)
    }

    /// Assigns the next sequence for `origin` and inserts atomically, so local sequences stay gapless.
    pub fn insert_local(&self, origin: &str, build: impl FnOnce(i64) -> Message) -> Result<Message> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE origin = ?",
            [origin],
            |r| r.get(0),
        )?;
        let msg = build(seq);
        anyhow::ensure!(msg.origin == origin, "origin mismatch");
        msg.validate().map_err(anyhow::Error::msg)?;
        insert_tx(&tx, &msg)?;
        tx.commit()?;
        Ok(msg)
    }

    /// Idempotent. Returns the messages that were new. Callers must supply each origin's
    /// messages in ascending `seq` without gaps above what we hold; others are refused so the
    /// version vector stays a gapless prefix.
    pub fn insert_remote(&self, msgs: &[Message]) -> Result<Vec<Message>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut vv = vv_tx(&tx)?;
        let mut sorted: Vec<&Message> = msgs.iter().collect();
        sorted.sort_by(|a, b| a.origin.cmp(&b.origin).then(a.seq.cmp(&b.seq)));
        let mut new = Vec::new();
        for m in sorted {
            if let Err(e) = m.validate() {
                tracing::warn!("refusing message {} from {}: {e}", m.id, m.origin);
                continue;
            }
            let have = vv.get(&m.origin).copied().unwrap_or(0);
            if m.seq <= have {
                continue;
            }
            if m.seq != have + 1 {
                tracing::warn!("gap for origin {}: have {have}, got {}", m.origin, m.seq);
                continue;
            }
            if insert_tx(&tx, m)? {
                vv.insert(m.origin.clone(), m.seq);
                new.push(m.clone());
            }
        }
        tx.commit()?;
        Ok(new)
    }

    pub fn version_vector(&self) -> Result<VersionVector> {
        vv_tx(&self.conn())
    }

    /// Messages the holder of `theirs` lacks, ordered by origin then seq.
    pub fn missing_for(&self, theirs: &VersionVector, limit: usize) -> Result<(Vec<Message>, bool)> {
        let ours = self.version_vector()?;
        let conn = self.conn();
        let mut out = Vec::new();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM messages WHERE origin = ? AND seq > ? ORDER BY seq LIMIT ?"
        ))?;
        for (origin, &max) in &ours {
            let have = theirs.get(origin).copied().unwrap_or(0);
            if have >= max {
                continue;
            }
            let room = limit.saturating_sub(out.len());
            if room == 0 {
                return Ok((out, true));
            }
            let rows = stmt.query_map(params![origin, have, room as i64], row_to_message)?;
            for r in rows {
                out.push(r?);
            }
        }
        let more = out.len() >= limit;
        Ok((out, more))
    }

    pub fn get(&self, id: &str) -> Result<Option<Message>> {
        Ok(self
            .conn()
            .query_row(&format!("SELECT {COLS} FROM messages WHERE id = ?"), [id], row_to_message)
            .optional()?)
    }

    pub fn thread(&self, thread: &str) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM messages WHERE thread = ? ORDER BY hlc, id"
        ))?;
        let rows = stmt.query_map([thread], row_to_message)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn all(&self) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM messages ORDER BY hlc, id"))?;
        let rows = stmt.query_map([], row_to_message)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.conn().query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?)
    }

    pub fn marks(&self, table: MarkTable, agent: &str) -> Result<HashSet<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT msg FROM {} WHERE agent = ?", table.table()))?;
        let rows = stmt.query_map([agent], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark(&self, table: MarkTable, agent: &str, ids: &[String]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(&format!(
                "INSERT OR IGNORE INTO {} (agent, msg) SELECT ?1, id FROM messages WHERE id = ?2",
                table.table()
            ))?;
            for id in ids {
                n += stmt.execute(params![agent, id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn pr_meta(&self, thread: &str) -> Result<Option<PrMeta>> {
        let data: Option<String> = self
            .conn()
            .query_row("SELECT data FROM pr_meta WHERE thread = ?", [thread], |r| r.get(0))
            .optional()?;
        Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
    }

    pub fn set_pr_meta(&self, thread: &str, meta: &PrMeta) -> Result<()> {
        self.conn().execute(
            "INSERT INTO pr_meta (thread, data) VALUES (?, ?) ON CONFLICT(thread) DO UPDATE SET data = excluded.data",
            params![thread, serde_json::to_string(meta)?],
        )?;
        Ok(())
    }
}

fn vv_tx(conn: &Connection) -> Result<VersionVector> {
    let mut stmt = conn.prepare("SELECT origin, MAX(seq) FROM messages GROUP BY origin")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn insert_tx(conn: &Connection, m: &Message) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO messages (id, origin, seq, hlc, thread, kind, author, body, fields, received_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            m.id,
            m.origin,
            m.seq,
            m.hlc,
            m.thread,
            m.kind.as_str(),
            serde_json::to_string(&m.author)?,
            m.body,
            serde_json::to_string(&m.fields)?,
            crate::model::now_ms(),
        ],
    )?;
    Ok(n == 1)
}

pub fn kind_from_str(s: &str) -> Option<Kind> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, new_id};

    fn mk(origin: &str, seq: i64) -> Message {
        Message {
            id: new_id(),
            origin: origin.into(),
            seq,
            hlc: seq,
            thread: "https://github.com/a/b/pull/1".into(),
            kind: Kind::Reply,
            author: Author::agent("x", "m"),
            body: format!("{origin}{seq}"),
            fields: Fields::default(),
        }
    }

    #[test]
    fn remote_inserts_are_idempotent_and_gapless() {
        let s = Store::open_memory().unwrap();
        let a: Vec<_> = (1..=3).map(|i| mk("A", i)).collect();
        assert_eq!(s.insert_remote(&a).unwrap().len(), 3);
        assert_eq!(s.insert_remote(&a).unwrap().len(), 0);
        assert_eq!(s.count().unwrap(), 3);
        let gap = mk("A", 5);
        assert_eq!(s.insert_remote(&[gap]).unwrap().len(), 0);
        let mut reordered = vec![mk("A", 5), mk("A", 4)];
        reordered.reverse();
        assert_eq!(s.insert_remote(&reordered).unwrap().len(), 2);
        assert_eq!(s.version_vector().unwrap().get("A"), Some(&5));
    }

    #[test]
    fn missing_for_respects_vector_and_limit() {
        let s = Store::open_memory().unwrap();
        let msgs: Vec<_> = (1..=4).map(|i| mk("A", i)).chain((1..=2).map(|i| mk("B", i))).collect();
        s.insert_remote(&msgs).unwrap();
        let theirs = VersionVector::from([("A".to_string(), 2)]);
        let (m, more) = s.missing_for(&theirs, 100).unwrap();
        assert_eq!(m.iter().map(|m| (m.origin.as_str(), m.seq)).collect::<Vec<_>>(),
            vec![("A", 3), ("A", 4), ("B", 1), ("B", 2)]);
        assert!(!more);
        let (m, more) = s.missing_for(&theirs, 3).unwrap();
        assert_eq!(m.len(), 3);
        assert!(more);
    }

    #[test]
    fn local_sequence_increments() {
        let s = Store::open_memory().unwrap();
        for _ in 0..3 {
            s.insert_local("L", |seq| mk("L", seq)).unwrap();
        }
        assert_eq!(s.version_vector().unwrap().get("L"), Some(&3));
    }
}
