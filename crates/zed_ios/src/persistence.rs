//! SQLite persistence for saved SSH connections.
//!
//! This module provides storage for recent/saved connections on iOS.
//! Unlike the desktop version which uses global static connections with complex
//! migration chains, this is a simpler standalone implementation suited for iOS.

use anyhow::{Context, Result};
use sqlez::{bindable::Column, connection::Connection, statement::Statement};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A saved SSH connection
#[derive(Clone, Debug)]
pub struct SavedConnection {
    pub id: i64,
    pub hostname: String,
    pub username: String,
    pub port: u16,
    pub nickname: Option<String>,
    pub path: Option<String>,
    #[allow(dead_code)]
    pub created_at: i64,
    #[allow(dead_code)]
    pub last_used_at: i64,
}

impl SavedConnection {
    /// Returns a display name for this connection
    pub fn display_name(&self) -> String {
        if let Some(nickname) = &self.nickname {
            nickname.clone()
        } else if self.port == 22 {
            format!("{}@{}", self.username, self.hostname)
        } else {
            format!("{}@{}:{}", self.username, self.hostname, self.port)
        }
    }
}

impl Column for SavedConnection {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (id, next) = i64::column(statement, start_index)?;
        let (hostname, next) = String::column(statement, next)?;
        let (username, next) = String::column(statement, next)?;
        let (port, next) = i64::column(statement, next)?;
        let (nickname, next) = Option::<String>::column(statement, next)?;
        let (path, next) = match Option::<String>::column(statement, next) {
            Ok(v) => v,
            Err(_) => (None, next),
        };
        let (created_at, next) = i64::column(statement, next)?;
        let (last_used_at, next) = i64::column(statement, next)?;

        Ok((
            SavedConnection {
                id,
                hostname,
                username,
                port: port as u16,
                nickname,
                path,
                created_at,
                last_used_at,
            },
            next,
        ))
    }
}

/// Database handle for connection persistence
pub struct ConnectionDb {
    connection: Arc<Mutex<Connection>>,
}

impl ConnectionDb {
    /// Opens or creates the database at the default path
    pub fn open() -> Result<Self> {
        let db_path = Self::default_db_path()?;
        Self::open_at(db_path)
    }

    /// Opens or creates the database at a specific path
    pub fn open_at(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create database directory")?;
        }

        let connection = Connection::open_file(path.to_string_lossy().as_ref());
        let db = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        db.initialize()?;
        Ok(db)
    }

    /// Creates an in-memory database for testing
    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        let connection = Connection::open_memory(Some("test_connections"));
        let db = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        db.initialize()?;
        Ok(db)
    }

    /// Returns the default database path on iOS
    fn default_db_path() -> Result<PathBuf> {
        let data_dir = paths::data_dir();
        Ok(data_dir.join("zed_ios_connections.sqlite"))
    }

    /// Initializes the database schema
    fn initialize(&self) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.exec(
            "CREATE TABLE IF NOT EXISTS saved_connections (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                hostname TEXT NOT NULL,
                username TEXT NOT NULL,
                port INTEGER NOT NULL DEFAULT 22,
                nickname TEXT,
                path TEXT,
                created_at INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL,
                UNIQUE(hostname, username, port)
            )",
        )?()
        .context("Failed to create saved_connections table")?;

        // Best-effort schema upgrade for existing installs
        let _ = conn.exec("ALTER TABLE saved_connections ADD COLUMN path TEXT")?().map_err(|e| {
            // Ignore duplicate column or similar harmless errors
            log::debug!("ignored path column alter error: {e}");
            e
        });

        conn.exec(
            "CREATE INDEX IF NOT EXISTS idx_last_used ON saved_connections(last_used_at DESC)",
        )?()
        .context("Failed to create last_used index")?;

        Ok(())
    }

    /// Saves a connection (inserts or updates if exists)
    pub fn save_connection(
        &self,
        hostname: &str,
        username: &str,
        port: u16,
        nickname: Option<&str>,
        path: Option<&str>,
    ) -> Result<()> {
        let conn = self.connection.lock().unwrap();
        let now = current_timestamp();

        conn.exec_bound::<(String, String, i64, Option<String>, Option<String>, i64, i64)>(
            "INSERT INTO saved_connections (hostname, username, port, nickname, path, created_at, last_used_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(hostname, username, port) DO UPDATE SET
                 nickname = COALESCE(excluded.nickname, nickname),
                 path = COALESCE(excluded.path, path),
                 last_used_at = excluded.last_used_at"
        )?((
            hostname.to_string(),
            username.to_string(),
            port as i64,
            nickname.map(String::from),
            path.map(String::from),
            now,
            now,
        ))
        .context("Failed to save connection")?;

        Ok(())
    }

    /// Updates the last_used_at timestamp for a connection
    pub fn touch_connection(&self, id: i64) -> Result<()> {
        let conn = self.connection.lock().unwrap();
        let now = current_timestamp();

        conn.exec_bound::<(i64, i64)>(
            "UPDATE saved_connections SET last_used_at = ? WHERE id = ?",
        )?((now, id))
        .context("Failed to touch connection")?;

        Ok(())
    }

    /// Updates the nickname for a connection
    #[allow(dead_code)]
    pub fn update_nickname(&self, id: i64, nickname: Option<&str>) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.exec_bound::<(Option<String>, i64)>(
            "UPDATE saved_connections SET nickname = ? WHERE id = ?",
        )?((nickname.map(String::from), id))
        .context("Failed to update nickname")?;

        Ok(())
    }

    /// Returns recent connections, ordered by last_used_at descending
    pub fn recent_connections(&self, limit: usize) -> Result<Vec<SavedConnection>> {
        let conn = self.connection.lock().unwrap();

        let connections = conn.select_bound::<i64, SavedConnection>(
            "SELECT id, hostname, username, port, nickname, path, created_at, last_used_at
             FROM saved_connections
             ORDER BY last_used_at DESC
             LIMIT ?",
        )?(limit as i64)
        .context("Failed to fetch recent connections")?;

        Ok(connections)
    }

    /// Finds a connection by hostname, username, and port
    #[allow(dead_code)]
    pub fn find_connection(
        &self,
        hostname: &str,
        username: &str,
        port: u16,
    ) -> Result<Option<SavedConnection>> {
        let conn = self.connection.lock().unwrap();

        let connections = conn.select_bound::<(String, String, i64), SavedConnection>(
            "SELECT id, hostname, username, port, nickname, path, created_at, last_used_at
             FROM saved_connections
             WHERE hostname = ? AND username = ? AND port = ?
             LIMIT 1",
        )?((hostname.to_string(), username.to_string(), port as i64))
        .context("Failed to find connection")?;

        Ok(connections.into_iter().next())
    }

    /// Deletes a connection by ID
    #[allow(dead_code)]
    pub fn delete_connection(&self, id: i64) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.exec_bound::<i64>("DELETE FROM saved_connections WHERE id = ?")?(id)
            .context("Failed to delete connection")?;

        Ok(())
    }

    /// Deletes all saved connections
    #[allow(dead_code)]
    pub fn clear_all(&self) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.exec("DELETE FROM saved_connections")?().context("Failed to clear connections")?;

        Ok(())
    }

    /// Returns the total number of saved connections
    #[allow(dead_code)]
    pub fn count(&self) -> Result<usize> {
        let conn = self.connection.lock().unwrap();

        let count = conn.select_row::<i64>("SELECT COUNT(*) FROM saved_connections")?()
            .context("Failed to count connections")?
            .unwrap_or(0);

        Ok(count as usize)
    }
}

/// Returns the current Unix timestamp in seconds
fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_and_retrieve_connection() {
        let db = ConnectionDb::open_memory().unwrap();

        db.save_connection("example.com", "user", 22, Some("My Server"), Some("~"))
            .unwrap();

        let connections = db.recent_connections(10).unwrap();
        assert_eq!(connections.len(), 1);

        let conn = &connections[0];
        assert_eq!(conn.hostname, "example.com");
        assert_eq!(conn.username, "user");
        assert_eq!(conn.port, 22);
        assert_eq!(conn.nickname, Some("My Server".to_string()));
        assert_eq!(conn.path, Some("~".to_string()));
    }

    #[test]
    fn test_upsert_updates_nickname() {
        let db = ConnectionDb::open_memory().unwrap();

        db.save_connection("example.com", "user", 22, None, Some("~"))
            .unwrap();
        db.save_connection("example.com", "user", 22, Some("Updated"), Some("/work"))
            .unwrap();

        let connections = db.recent_connections(10).unwrap();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].nickname, Some("Updated".to_string()));
        assert_eq!(connections[0].path, Some("/work".to_string()));
    }

    #[test]
    fn test_find_connection() {
        let db = ConnectionDb::open_memory().unwrap();

        db.save_connection("example.com", "user", 22, None, None)
            .unwrap();
        db.save_connection("other.com", "admin", 2222, None, None)
            .unwrap();

        let found = db.find_connection("example.com", "user", 22).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().hostname, "example.com");

        let not_found = db.find_connection("missing.com", "user", 22).unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn test_delete_connection() {
        let db = ConnectionDb::open_memory().unwrap();

        db.save_connection("example.com", "user", 22, None, None)
            .unwrap();
        assert_eq!(db.count().unwrap(), 1);

        let connections = db.recent_connections(1).unwrap();
        let id = connections[0].id;

        db.delete_connection(id).unwrap();
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn test_recent_connections_limit() {
        let db = ConnectionDb::open_memory().unwrap();

        db.save_connection("first.com", "user", 22, None, None)
            .unwrap();
        db.save_connection("second.com", "user", 22, None, None)
            .unwrap();
        db.save_connection("third.com", "user", 22, None, None)
            .unwrap();

        let connections = db.recent_connections(2).unwrap();
        assert_eq!(connections.len(), 2);
    }

    #[test]
    fn test_display_name() {
        let conn = SavedConnection {
            id: 1,
            hostname: "example.com".to_string(),
            username: "user".to_string(),
            port: 22,
            nickname: None,
            path: None,
            created_at: 0,
            last_used_at: 0,
        };
        assert_eq!(conn.display_name(), "user@example.com");

        let conn_with_port = SavedConnection {
            port: 2222,
            ..conn.clone()
        };
        assert_eq!(conn_with_port.display_name(), "user@example.com:2222");

        let conn_with_nick = SavedConnection {
            nickname: Some("Work Server".to_string()),
            ..conn
        };
        assert_eq!(conn_with_nick.display_name(), "Work Server");
    }
}
