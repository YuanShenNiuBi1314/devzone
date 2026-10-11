//! SQLite storage: `%LOCALAPPDATA%\devzone\data\devzone.db`.
//!
//! The schema is exactly the one in the build guide (section 6.1.2); migrations
//! run through `PRAGMA user_version`.

use std::path::Path;

use anyhow::{anyhow, Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::util::Paths;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS zones (
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  path TEXT NOT NULL,
  drive TEXT,
  alert_free_gb REAL NOT NULL DEFAULT 10,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS zone_snapshots (
  id INTEGER PRIMARY KEY,
  zone_id INTEGER NOT NULL REFERENCES zones(id),
  ts TEXT NOT NULL,
  total_bytes INTEGER NOT NULL,
  file_count INTEGER NOT NULL,
  dir_count INTEGER NOT NULL,
  free_bytes INTEGER
);
CREATE TABLE IF NOT EXISTS big_files (
  id INTEGER PRIMARY KEY,
  snapshot_id INTEGER NOT NULL REFERENCES zone_snapshots(id),
  path TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  mtime TEXT
);
CREATE INDEX IF NOT EXISTS idx_snap_zone_ts ON zone_snapshots(zone_id, ts);
CREATE INDEX IF NOT EXISTS idx_big_snap ON big_files(snapshot_id, bytes DESC);
CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  status TEXT NOT NULL,          -- running|paused|clean_exit|dirty_exit
  app_name TEXT, app_path TEXT,
  clean_exit INTEGER,            -- 1/0/NULL
  checkpoint_data TEXT,          -- JSON
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS battery_events (
  id INTEGER PRIMARY KEY,
  ts TEXT NOT NULL,
  level INTEGER, ac INTEGER, action TEXT
);
"#;

#[derive(Clone, Debug)]
pub struct ZoneRow {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub drive: Option<String>,
    pub alert_free_gb: f64,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct SnapshotRow {
    pub id: i64,
    pub zone_id: i64,
    pub ts: String,
    pub total_bytes: i64,
    pub file_count: i64,
    pub dir_count: i64,
    pub free_bytes: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct BigFileRow {
    pub path: String,
    pub bytes: i64,
    pub mtime: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TaskRow {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub app_name: Option<String>,
    pub app_path: Option<String>,
    pub clean_exit: Option<i64>,
    pub checkpoint_data: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct BatteryEventRow {
    pub id: i64,
    pub ts: String,
    pub level: Option<i64>,
    pub ac: Option<i64>,
    pub action: Option<String>,
}

pub struct Db {
    pub conn: Connection,
}

impl Db {
    pub fn open(paths: &Paths) -> Result<Db> {
        paths.ensure_dirs()?;
        Db::open_path(&paths.db)
    }

    pub fn open_path(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Sequential migrations driven by `PRAGMA user_version`.
    pub fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("read user_version")?;
        if version < 1 {
            self.conn
                .execute_batch(SCHEMA_V1)
                .context("apply schema v1")?;
            self.conn
                .pragma_update(None, "user_version", 1)
                .context("stamp user_version=1")?;
        }
        if version < 0 {
            return Err(anyhow!("unexpected user_version {version}"));
        }
        Ok(())
    }

    pub fn in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory().context("open in-memory db")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    // ---- zones -------------------------------------------------------------

    /// Insert a zone. Duplicate names fail unless `force` replaces the row
    /// (history is dropped together with the old zone id).
    pub fn add_zone(
        &self,
        name: &str,
        path: &str,
        drive: Option<&str>,
        alert_free_gb: f64,
        created_at: &str,
        force: bool,
    ) -> Result<ZoneRow> {
        let existing = self.zone_by_name(name)?;
        if let Some(old) = &existing {
            if !force {
                return Err(anyhow!(
                    "zone `{name}` 已存在（指向 {}），覆盖请用 --force",
                    old.path
                ));
            }
            // Cascading delete: zones has a UNIQUE name and child rows reference
            // it, so a bare DELETE fails once snapshots exist (foreign_keys=ON).
            self.delete_zone_history(old.id)?;
            self.conn
                .execute("DELETE FROM zones WHERE id = ?1", params![old.id])?;
        }
        self.conn.execute(
            "INSERT INTO zones (name, path, drive, alert_free_gb, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, path, drive, alert_free_gb, created_at],
        )?;
        self.zone_by_id(self.conn.last_insert_rowid())?
            .ok_or_else(|| anyhow!("zone insert did not persist"))
    }

    pub fn zone_by_name(&self, name: &str) -> Result<Option<ZoneRow>> {
        self.conn
            .query_row(ZONE_SELECT, params![name], ZoneRow::from_row)
            .optional()
            .context("query zone by name")
    }

    pub fn zone_by_id(&self, id: i64) -> Result<Option<ZoneRow>> {
        self.conn
            .query_row(
                &ZONE_SELECT.replace("WHERE name = ?1", "WHERE id = ?1"),
                params![id],
                ZoneRow::from_row,
            )
            .optional()
            .context("query zone by id")
    }

    pub fn list_zones(&self) -> Result<Vec<ZoneRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{ZONE_COLUMNS} ORDER BY name"))?;
        let rows = stmt.query_map([], ZoneRow::from_row)?;
        rows.collect::<Result<Vec<_>, _>>().context("list zones")
    }

    pub fn update_zone_alert(&self, name: &str, alert_free_gb: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE zones SET alert_free_gb = ?1 WHERE name = ?2",
            params![alert_free_gb, name],
        )?;
        Ok(())
    }

    pub fn remove_zone(&self, name: &str) -> Result<bool> {
        let Some(zone) = self.zone_by_name(name)? else {
            return Ok(false);
        };
        self.delete_zone_history(zone.id)?;
        self.conn
            .execute("DELETE FROM zones WHERE id = ?1", params![zone.id])?;
        Ok(true)
    }

    /// Remove every snapshot and big-file row owned by a zone.
    pub fn delete_zone_history(&self, zone_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM big_files WHERE snapshot_id IN (SELECT id FROM zone_snapshots WHERE zone_id = ?1)",
            params![zone_id],
        )?;
        self.conn.execute(
            "DELETE FROM zone_snapshots WHERE zone_id = ?1",
            params![zone_id],
        )?;
        Ok(())
    }

    // ---- snapshots ---------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn insert_snapshot(
        &self,
        zone_id: i64,
        ts: &str,
        total_bytes: i64,
        file_count: i64,
        dir_count: i64,
        free_bytes: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO zone_snapshots (zone_id, ts, total_bytes, file_count, dir_count, free_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![zone_id, ts, total_bytes, file_count, dir_count, free_bytes],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Most recent snapshots for a zone, newest first.
    pub fn snapshots(&self, zone_id: i64, limit: usize) -> Result<Vec<SnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, zone_id, ts, total_bytes, file_count, dir_count, free_bytes
             FROM zone_snapshots WHERE zone_id = ?1 ORDER BY ts DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![zone_id, limit as i64], SnapshotRow::from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("list snapshots")
    }

    pub fn insert_big_files(
        &self,
        snapshot_id: i64,
        files: &[(String, i64, Option<String>)],
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO big_files (snapshot_id, path, bytes, mtime) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (path, bytes, mtime) in files {
            stmt.execute(params![snapshot_id, path, bytes, mtime])?;
        }
        Ok(())
    }

    pub fn big_files(&self, snapshot_id: i64, top_n: usize) -> Result<Vec<BigFileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, bytes, mtime FROM big_files WHERE snapshot_id = ?1
             ORDER BY bytes DESC, path ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![snapshot_id, top_n as i64], BigFileRow::from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("list big files")
    }

    // ---- tasks / checkpoints ----------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn insert_task(
        &self,
        name: &str,
        status: &str,
        app_name: Option<&str>,
        app_path: Option<&str>,
        clean_exit: Option<i64>,
        checkpoint_data: Option<&str>,
        updated_at: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO tasks (name, status, app_name, app_path, clean_exit, checkpoint_data, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![name, status, app_name, app_path, clean_exit, checkpoint_data, updated_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The newest manifest row (written with `app_name IS NULL`).
    pub fn latest_checkpoint(&self) -> Result<Option<TaskRow>> {
        self.conn
            .query_row(
                "SELECT id, name, status, app_name, app_path, clean_exit, checkpoint_data, updated_at
                 FROM tasks WHERE app_name IS NULL ORDER BY id DESC LIMIT 1",
                [],
                TaskRow::from_row,
            )
            .optional()
            .context("latest checkpoint")
    }

    pub fn checkpoint_items(&self, updated_at: &str) -> Result<Vec<TaskRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, status, app_name, app_path, clean_exit, checkpoint_data, updated_at
             FROM tasks WHERE app_name IS NOT NULL AND updated_at = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![updated_at], TaskRow::from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("checkpoint items")
    }

    pub fn list_tasks(&self, limit: usize) -> Result<Vec<TaskRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, status, app_name, app_path, clean_exit, checkpoint_data, updated_at
             FROM tasks ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], TaskRow::from_row)?;
        rows.collect::<Result<Vec<_>, _>>().context("list tasks")
    }

    // ---- battery -----------------------------------------------------------

    pub fn insert_battery_event(
        &self,
        ts: &str,
        level: Option<i64>,
        ac: Option<i64>,
        action: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO battery_events (ts, level, ac, action) VALUES (?1, ?2, ?3, ?4)",
            params![ts, level, ac, action],
        )?;
        Ok(())
    }

    pub fn battery_events(&self, limit: usize) -> Result<Vec<BatteryEventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, level, ac, action FROM battery_events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], BatteryEventRow::from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("list battery events")
    }

    pub fn last_battery_action(&self, action: &str) -> Result<Option<BatteryEventRow>> {
        self.conn
            .query_row(
                "SELECT id, ts, level, ac, action FROM battery_events WHERE action = ?1 ORDER BY id DESC LIMIT 1",
                params![action],
                BatteryEventRow::from_row,
            )
            .optional()
            .context("last battery action")
    }

    /// Counts per table, used by `devzone status`.
    pub fn table_counts(&self) -> Result<serde_json::Value> {
        let mut out = serde_json::Map::new();
        for table in [
            "zones",
            "zone_snapshots",
            "big_files",
            "tasks",
            "battery_events",
        ] {
            let count: i64 = self
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .with_context(|| format!("count {table}"))?;
            out.insert(table.to_string(), serde_json::json!(count));
        }
        Ok(serde_json::Value::Object(out))
    }
}

const ZONE_COLUMNS: &str = "SELECT id, name, path, drive, alert_free_gb, created_at FROM zones";
const ZONE_SELECT: &str =
    "SELECT id, name, path, drive, alert_free_gb, created_at FROM zones WHERE name = ?1";

impl ZoneRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ZoneRow> {
        Ok(ZoneRow {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            drive: row.get(3)?,
            alert_free_gb: row.get(4)?,
            created_at: row.get(5)?,
        })
    }
}

impl SnapshotRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotRow> {
        Ok(SnapshotRow {
            id: row.get(0)?,
            zone_id: row.get(1)?,
            ts: row.get(2)?,
            total_bytes: row.get(3)?,
            file_count: row.get(4)?,
            dir_count: row.get(5)?,
            free_bytes: row.get(6)?,
        })
    }

    pub fn free_gb(&self) -> Option<f64> {
        self.free_bytes.map(crate::util::gb)
    }
}

impl BigFileRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BigFileRow> {
        Ok(BigFileRow {
            path: row.get(0)?,
            bytes: row.get(1)?,
            mtime: row.get(2)?,
        })
    }
}

impl TaskRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
        Ok(TaskRow {
            id: row.get(0)?,
            name: row.get(1)?,
            status: row.get(2)?,
            app_name: row.get(3)?,
            app_path: row.get(4)?,
            clean_exit: row.get(5)?,
            checkpoint_data: row.get(6)?,
            updated_at: row.get(7)?,
        })
    }

    /// Parsed `checkpoint_data` JSON, empty object when absent or broken.
    pub fn checkpoint_json(&self) -> serde_json::Value {
        self.checkpoint_data
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    }
}

impl BatteryEventRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BatteryEventRow> {
        Ok(BatteryEventRow {
            id: row.get(0)?,
            ts: row.get(1)?,
            level: row.get(2)?,
            ac: row.get(3)?,
            action: row.get(4)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent_and_stamps_version() {
        let db = Db::in_memory().unwrap();
        db.migrate().unwrap();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert_eq!(db.table_counts().unwrap()["zones"], 0);
    }

    #[test]
    fn zone_crud_and_duplicate_guard() {
        let db = Db::in_memory().unwrap();
        let added = db
            .add_zone(
                "work",
                "D:\\dev",
                Some("D:"),
                5.0,
                "2026-10-10T10:00:00+08:00",
                false,
            )
            .unwrap();
        assert_eq!(added.name, "work");
        let err = db
            .add_zone(
                "work",
                "E:\\other",
                None,
                1.0,
                "2026-10-10T10:01:00+08:00",
                false,
            )
            .unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        let forced = db
            .add_zone(
                "work",
                "E:\\other",
                Some("E:"),
                1.0,
                "2026-10-10T10:02:00+08:00",
                true,
            )
            .unwrap();
        assert_eq!(forced.path, "E:\\other");
        assert_eq!(db.list_zones().unwrap().len(), 1);
        db.update_zone_alert("work", 7.5).unwrap();
        assert!((db.zone_by_name("work").unwrap().unwrap().alert_free_gb - 7.5).abs() < 1e-9);
        assert!(db.remove_zone("work").unwrap());
        assert!(!db.remove_zone("work").unwrap());
        assert!(db.list_zones().unwrap().is_empty());
    }

    #[test]
    fn force_replace_keeps_history_consistent() {
        // Regression: `--force` used to hit a FOREIGN KEY constraint because the
        // old zone row still owned snapshots.
        let db = Db::in_memory().unwrap();
        let zone = db
            .add_zone("work", "D:\\dev", Some("D:"), 5.0, "t0", false)
            .unwrap();
        let snap = db
            .insert_snapshot(zone.id, "t1", 10, 1, 1, Some(1))
            .unwrap();
        db.insert_big_files(snap, &[("D:\\dev\\a.bin".to_string(), 10, None)])
            .unwrap();
        let replaced = db
            .add_zone("work", "E:\\dev", Some("E:"), 2.0, "t2", true)
            .unwrap();
        assert_eq!(replaced.path, "E:\\dev");
        assert_eq!(db.table_counts().unwrap()["zone_snapshots"], 0);
        assert_eq!(db.table_counts().unwrap()["big_files"], 0);
        assert_eq!(db.list_zones().unwrap().len(), 1);
    }

    #[test]
    fn snapshots_and_big_files_order_newest_first() {
        let db = Db::in_memory().unwrap();
        let zone = db
            .add_zone("a", "C:\\a", None, 10.0, "2026-10-10T10:00:00+08:00", false)
            .unwrap();
        let s1 = db
            .insert_snapshot(zone.id, "2026-10-10T10:00:00+08:00", 100, 2, 1, Some(900))
            .unwrap();
        let s2 = db
            .insert_snapshot(zone.id, "2026-10-10T11:00:00+08:00", 160, 3, 1, Some(800))
            .unwrap();
        db.insert_big_files(
            s2,
            &[
                (
                    "C:\\a\\big.bin".to_string(),
                    60,
                    Some("2026-10-10".to_string()),
                ),
                ("C:\\a\\small.txt".to_string(), 3, None),
            ],
        )
        .unwrap();
        let snaps = db.snapshots(zone.id, 5).unwrap();
        assert_eq!(snaps[0].id, s2);
        assert_eq!(snaps[1].id, s1);
        assert_eq!(snaps[0].total_bytes, 160);
        let big = db.big_files(s2, 10).unwrap();
        assert_eq!(big[0].path, "C:\\a\\big.bin");
        assert_eq!(big.len(), 2);
        assert_eq!(db.table_counts().unwrap()["big_files"], 2);
        assert_eq!(db.snapshots(zone.id, 1).unwrap().len(), 1);
    }

    #[test]
    fn checkpoint_manifest_and_items() {
        let db = Db::in_memory().unwrap();
        let ts = "2026-10-10T12:00:00+08:00";
        let summary = db
            .insert_task(
                "checkpoint",
                "paused",
                None,
                None,
                None,
                Some("{\"level\":14}"),
                ts,
            )
            .unwrap();
        db.insert_task(
            "doubao.exe",
            "clean_exit",
            Some("doubao.exe"),
            Some("C:\\x\\doubao.exe"),
            Some(1),
            Some("{}"),
            ts,
        )
        .unwrap();
        db.insert_task(
            "trae.exe",
            "dirty_exit",
            Some("trae.exe"),
            Some("C:\\y\\trae.exe"),
            Some(0),
            Some("{}"),
            ts,
        )
        .unwrap();
        let latest = db.latest_checkpoint().unwrap().unwrap();
        assert_eq!(latest.id, summary);
        assert_eq!(latest.checkpoint_json()["level"], 14);
        let items = db.checkpoint_items(ts).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].app_name.as_deref(), Some("doubao.exe"));
        assert_eq!(items[1].clean_exit, Some(0));
        assert_eq!(db.list_tasks(10).unwrap().len(), 3);
    }

    #[test]
    fn battery_events_history() {
        let db = Db::in_memory().unwrap();
        db.insert_battery_event(
            "2026-10-10T12:00:00+08:00",
            Some(19),
            Some(0),
            "battery_low",
        )
        .unwrap();
        db.insert_battery_event(
            "2026-10-10T12:05:00+08:00",
            Some(14),
            Some(0),
            "pause_start",
        )
        .unwrap();
        db.insert_battery_event(
            "2026-10-10T12:40:00+08:00",
            Some(22),
            Some(1),
            "power_restored",
        )
        .unwrap();
        let events = db.battery_events(2).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].action.as_deref(), Some("power_restored"));
        let found = db.last_battery_action("pause_start").unwrap().unwrap();
        assert_eq!(found.level, Some(14));
        assert!(db.last_battery_action("never").unwrap().is_none());
    }

    #[test]
    fn remove_zone_cascades_snapshots() {
        let db = Db::in_memory().unwrap();
        let zone = db.add_zone("a", "C:\\a", None, 10.0, "t", false).unwrap();
        let snap = db.insert_snapshot(zone.id, "t", 1, 1, 1, Some(1)).unwrap();
        db.insert_big_files(snap, &[("f".to_string(), 1, None)])
            .unwrap();
        db.remove_zone("a").unwrap();
        assert_eq!(db.table_counts().unwrap()["zone_snapshots"], 0);
        assert_eq!(db.table_counts().unwrap()["big_files"], 0);
    }
}
