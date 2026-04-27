//! SQLite persistence for saved SSH connections and restore state.
//!
//! The iPad client needs a richer notion of saved remote workspaces than the
//! initial thin-client prototype. We keep the original `saved_connections`
//! table name for compatibility and migrate it forward in place.

use anyhow::{Context, Result, anyhow};
use remote::SshPortForwardOption;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlez::{bindable::Column, connection::Connection, statement::Statement};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    Prompt,
    KeychainSecret,
    KeyBased,
}

impl AuthMode {
    fn as_db_value(&self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::KeychainSecret => "keychain_secret",
            Self::KeyBased => "key_based",
        }
    }

    fn from_db_value(value: &str) -> Self {
        match value {
            "keychain_secret" => Self::KeychainSecret,
            "key_based" => Self::KeyBased,
            _ => Self::Prompt,
        }
    }
}

/// Mutable profile data collected from the iPad connection form.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfileInput {
    pub hostname: String,
    pub username: String,
    pub port: u16,
    pub nickname: Option<String>,
    pub default_path: Option<String>,
    pub ssh_args: Vec<String>,
    pub port_forwards: Vec<SshPortForwardOption>,
    pub auth_mode: AuthMode,
    pub upload_binary_over_ssh: bool,
    pub private_key_name: Option<String>,
    pub private_key_fingerprint: Option<String>,
    pub last_successful_server_version: Option<String>,
    pub last_opened_worktree: Option<String>,
}

/// A persisted SSH connection profile for the iPad app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: i64,
    pub hostname: String,
    pub username: String,
    pub port: u16,
    pub nickname: Option<String>,
    pub default_path: Option<String>,
    pub ssh_args: Vec<String>,
    pub port_forwards: Vec<SshPortForwardOption>,
    pub auth_mode: AuthMode,
    pub upload_binary_over_ssh: bool,
    pub private_key_name: Option<String>,
    pub private_key_fingerprint: Option<String>,
    pub last_successful_server_version: Option<String>,
    pub last_opened_worktree: Option<String>,
    pub last_session_timestamp: Option<i64>,
    pub host_key_fingerprint: Option<String>,
    pub host_key_verified_at: Option<i64>,
    pub created_at: i64,
    pub last_used_at: i64,
}

impl ConnectionProfile {
    /// Returns a friendly label for UI lists.
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

impl Column for ConnectionProfile {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (id, next) = i64::column(statement, start_index)?;
        let (hostname, next) = String::column(statement, next)?;
        let (username, next) = String::column(statement, next)?;
        let (port, next) = i64::column(statement, next)?;
        let (nickname, next) = Option::<String>::column(statement, next)?;
        let (default_path, next) = Option::<String>::column(statement, next)?;
        let (ssh_args_json, next) = Option::<String>::column(statement, next)?;
        let (port_forwards_json, next) = Option::<String>::column(statement, next)?;
        let (upload_binary_over_ssh, next) = i64::column(statement, next)?;
        let (auth_mode, next) = Option::<String>::column(statement, next)?;
        let (private_key_name, next) = Option::<String>::column(statement, next)?;
        let (private_key_fingerprint, next) = Option::<String>::column(statement, next)?;
        let (last_successful_server_version, next) = Option::<String>::column(statement, next)?;
        let (last_opened_worktree, next) = Option::<String>::column(statement, next)?;
        let (last_session_timestamp, next) = Option::<i64>::column(statement, next)?;
        let (host_key_fingerprint, next) = Option::<String>::column(statement, next)?;
        let (host_key_verified_at, next) = Option::<i64>::column(statement, next)?;
        let (created_at, next) = i64::column(statement, next)?;
        let (last_used_at, next) = i64::column(statement, next)?;

        Ok((
            ConnectionProfile {
                id,
                hostname,
                username,
                port: port as u16,
                nickname,
                default_path,
                ssh_args: deserialize_json_vec(ssh_args_json)?,
                port_forwards: deserialize_json_vec(port_forwards_json)?,
                auth_mode: auth_mode
                    .as_deref()
                    .map(AuthMode::from_db_value)
                    .unwrap_or_default(),
                upload_binary_over_ssh: upload_binary_over_ssh != 0,
                private_key_name,
                private_key_fingerprint,
                last_successful_server_version,
                last_opened_worktree,
                last_session_timestamp,
                host_key_fingerprint,
                host_key_verified_at,
                created_at,
                last_used_at,
            },
            next,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRestoreState {
    pub connection_profile_id: i64,
    pub remote_path: Option<String>,
    pub last_opened_worktree: Option<String>,
    pub workspace_state_json: Option<String>,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileWorkspaceSnapshotV1 {
    pub version: u8,
    pub remote_path: Option<String>,
    pub last_opened_worktree: Option<String>,
    pub active_tool: Option<String>,
    pub left_sidebar_visible: bool,
    pub terminal_visible: bool,
    pub git_panel_visible: bool,
    pub agent_visible: bool,
}

impl MobileWorkspaceSnapshotV1 {
    pub const VERSION: u8 = 1;

    pub fn new(remote_path: Option<String>, last_opened_worktree: Option<String>) -> Self {
        Self {
            version: Self::VERSION,
            remote_path,
            last_opened_worktree,
            active_tool: None,
            left_sidebar_visible: true,
            terminal_visible: false,
            git_panel_visible: false,
            agent_visible: false,
        }
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize mobile workspace snapshot")
    }

    pub fn from_json(raw: &str) -> Result<Self> {
        serde_json::from_str(raw).context("Failed to deserialize mobile workspace snapshot")
    }
}

impl Column for SessionRestoreState {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (connection_profile_id, next) = i64::column(statement, start_index)?;
        let (remote_path, next) = Option::<String>::column(statement, next)?;
        let (last_opened_worktree, next) = Option::<String>::column(statement, next)?;
        let (workspace_state_json, next) = Option::<String>::column(statement, next)?;
        let (updated_at, next) = i64::column(statement, next)?;

        Ok((
            SessionRestoreState {
                connection_profile_id,
                remote_path,
                last_opened_worktree,
                workspace_state_json,
                updated_at,
            },
            next,
        ))
    }
}

/// Database handle for connection/profile persistence.
pub struct ConnectionDb {
    connection: Arc<Mutex<Connection>>,
}

impl ConnectionDb {
    pub fn open() -> Result<Self> {
        let db_path = Self::default_db_path()?;
        Self::open_at(db_path)
    }

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

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        let connection = Connection::open_memory(None);
        let db = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        db.initialize()?;
        Ok(db)
    }

    fn default_db_path() -> Result<PathBuf> {
        Ok(paths::data_dir().join("zed_ios_connections.sqlite"))
    }

    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("Connection database mutex poisoned"))
    }

    fn initialize(&self) -> Result<()> {
        let conn = self.lock_connection()?;

        conn.exec(
            "CREATE TABLE IF NOT EXISTS saved_connections (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                hostname TEXT NOT NULL,
                username TEXT NOT NULL,
                port INTEGER NOT NULL DEFAULT 22,
                nickname TEXT,
                path TEXT,
                ssh_args_json TEXT,
                port_forwards_json TEXT,
                auth_mode TEXT NOT NULL DEFAULT 'prompt',
                upload_binary_over_ssh INTEGER NOT NULL DEFAULT 0,
                private_key_name TEXT,
                private_key_fingerprint TEXT,
                last_successful_server_version TEXT,
                last_opened_worktree TEXT,
                last_session_timestamp INTEGER,
                host_key_fingerprint TEXT,
                host_key_verified_at INTEGER,
                created_at INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL,
                UNIQUE(hostname, username, port)
            )",
        )?()
        .context("Failed to create saved_connections table")?;

        ensure_column(&conn, "saved_connections", "path", "TEXT")?;
        ensure_column(&conn, "saved_connections", "ssh_args_json", "TEXT")?;
        ensure_column(&conn, "saved_connections", "port_forwards_json", "TEXT")?;
        ensure_column(
            &conn,
            "saved_connections",
            "auth_mode",
            "TEXT NOT NULL DEFAULT 'prompt'",
        )?;
        ensure_column(
            &conn,
            "saved_connections",
            "upload_binary_over_ssh",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&conn, "saved_connections", "private_key_name", "TEXT")?;
        ensure_column(
            &conn,
            "saved_connections",
            "private_key_fingerprint",
            "TEXT",
        )?;
        ensure_column(
            &conn,
            "saved_connections",
            "last_successful_server_version",
            "TEXT",
        )?;
        ensure_column(&conn, "saved_connections", "last_opened_worktree", "TEXT")?;
        ensure_column(
            &conn,
            "saved_connections",
            "last_session_timestamp",
            "INTEGER",
        )?;
        ensure_column(&conn, "saved_connections", "host_key_fingerprint", "TEXT")?;
        ensure_column(
            &conn,
            "saved_connections",
            "host_key_verified_at",
            "INTEGER",
        )?;

        conn.exec(
            "CREATE INDEX IF NOT EXISTS idx_last_used ON saved_connections(last_used_at DESC)",
        )?()
        .context("Failed to create last_used index")?;

        conn.exec(
            "CREATE TABLE IF NOT EXISTS session_restore_state (
                singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
                connection_profile_id INTEGER NOT NULL,
                remote_path TEXT,
                last_opened_worktree TEXT,
                workspace_state_json TEXT,
                updated_at INTEGER NOT NULL,
                FOREIGN KEY(connection_profile_id) REFERENCES saved_connections(id) ON DELETE CASCADE
            )",
        )?()
        .context("Failed to create session_restore_state table")?;

        Ok(())
    }

    pub fn upsert_connection_profile(
        &self,
        input: &ConnectionProfileInput,
    ) -> Result<ConnectionProfile> {
        {
            let conn = self.lock_connection()?;
            let now = current_timestamp();

            conn.exec_bound::<(
                (
                    String,
                    String,
                    i64,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    String,
                ),
                (
                    bool,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                ),
                (i64, i64),
            )>(
                "INSERT INTO saved_connections (
                    hostname,
                    username,
                    port,
                    nickname,
                    path,
                    ssh_args_json,
                    port_forwards_json,
                    auth_mode,
                    upload_binary_over_ssh,
                    private_key_name,
                    private_key_fingerprint,
                    last_successful_server_version,
                    last_opened_worktree,
                    created_at,
                    last_used_at
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(hostname, username, port) DO UPDATE SET
                    nickname = excluded.nickname,
                    path = excluded.path,
                    ssh_args_json = excluded.ssh_args_json,
                    port_forwards_json = excluded.port_forwards_json,
                    auth_mode = excluded.auth_mode,
                    upload_binary_over_ssh = excluded.upload_binary_over_ssh,
                    private_key_name = excluded.private_key_name,
                    private_key_fingerprint = excluded.private_key_fingerprint,
                    last_successful_server_version = COALESCE(excluded.last_successful_server_version, saved_connections.last_successful_server_version),
                    last_opened_worktree = COALESCE(excluded.last_opened_worktree, saved_connections.last_opened_worktree),
                    last_used_at = excluded.last_used_at"
            )?((
                (
                    input.hostname.clone(),
                    input.username.clone(),
                    input.port as i64,
                    input.nickname.clone(),
                    input.default_path.clone(),
                    serialize_json_vec(&input.ssh_args)?,
                    serialize_json_vec(&input.port_forwards)?,
                    input.auth_mode.as_db_value().to_string(),
                ),
                (
                    input.upload_binary_over_ssh,
                    input.private_key_name.clone(),
                    input.private_key_fingerprint.clone(),
                    input.last_successful_server_version.clone(),
                    input.last_opened_worktree.clone(),
                ),
                (now, now),
            ))
            .context("Failed to upsert connection profile")?;
        }

        self.find_connection_profile(&input.hostname, &input.username, input.port)?
            .context("Inserted connection profile could not be reloaded")
    }

    pub fn recent_connection_profiles(&self, limit: usize) -> Result<Vec<ConnectionProfile>> {
        let conn = self.lock_connection()?;

        conn.select_bound::<i64, ConnectionProfile>(
            "SELECT
                id,
                hostname,
                username,
                port,
                nickname,
                path,
                ssh_args_json,
                port_forwards_json,
                upload_binary_over_ssh,
                auth_mode,
                private_key_name,
                private_key_fingerprint,
                last_successful_server_version,
                last_opened_worktree,
                last_session_timestamp,
                host_key_fingerprint,
                host_key_verified_at,
                created_at,
                last_used_at
             FROM saved_connections
             ORDER BY last_used_at DESC
             LIMIT ?",
        )?(limit as i64)
        .context("Failed to fetch recent connection profiles")
    }

    pub fn find_connection_profile(
        &self,
        hostname: &str,
        username: &str,
        port: u16,
    ) -> Result<Option<ConnectionProfile>> {
        let conn = self.lock_connection()?;

        let profiles = conn.select_bound::<(String, String, i64), ConnectionProfile>(
            "SELECT
                id,
                hostname,
                username,
                port,
                nickname,
                path,
                ssh_args_json,
                port_forwards_json,
                upload_binary_over_ssh,
                auth_mode,
                private_key_name,
                private_key_fingerprint,
                last_successful_server_version,
                last_opened_worktree,
                last_session_timestamp,
                host_key_fingerprint,
                host_key_verified_at,
                created_at,
                last_used_at
             FROM saved_connections
             WHERE hostname = ? AND username = ? AND port = ?
             LIMIT 1",
        )?((hostname.to_string(), username.to_string(), port as i64))
        .context("Failed to find connection profile")?;

        Ok(profiles.into_iter().next())
    }

    pub fn connection_profile_by_id(&self, id: i64) -> Result<Option<ConnectionProfile>> {
        let conn = self.lock_connection()?;

        let profiles = conn.select_bound::<i64, ConnectionProfile>(
            "SELECT
                id,
                hostname,
                username,
                port,
                nickname,
                path,
                ssh_args_json,
                port_forwards_json,
                upload_binary_over_ssh,
                auth_mode,
                private_key_name,
                private_key_fingerprint,
                last_successful_server_version,
                last_opened_worktree,
                last_session_timestamp,
                host_key_fingerprint,
                host_key_verified_at,
                created_at,
                last_used_at
             FROM saved_connections
             WHERE id = ?
             LIMIT 1",
        )?(id)
        .context("Failed to load connection profile by id")?;

        Ok(profiles.into_iter().next())
    }

    pub fn update_connection_profile_port_forwards(
        &self,
        id: i64,
        port_forwards: &[SshPortForwardOption],
    ) -> Result<Option<ConnectionProfile>> {
        let conn = self.lock_connection()?;
        conn.exec_bound::<(Option<String>, i64, i64)>(
            "UPDATE saved_connections
             SET port_forwards_json = ?, last_used_at = ?
             WHERE id = ?",
        )?((serialize_json_vec(port_forwards)?, current_timestamp(), id))
        .context("Failed to update saved port forwards")?;
        drop(conn);
        self.connection_profile_by_id(id)
    }

    pub fn touch_connection_profile(&self, id: i64) -> Result<()> {
        let conn = self.lock_connection()?;
        let now = current_timestamp();

        conn.exec_bound::<(i64, i64)>(
            "UPDATE saved_connections SET last_used_at = ? WHERE id = ?",
        )?((now, id))
        .context("Failed to touch connection profile")?;

        Ok(())
    }

    pub fn record_host_key_verification(&self, id: i64, fingerprint: &str) -> Result<()> {
        let conn = self.lock_connection()?;
        let now = current_timestamp();

        conn.exec_bound::<(String, i64, i64)>(
            "UPDATE saved_connections
             SET host_key_fingerprint = ?, host_key_verified_at = ?
             WHERE id = ?",
        )?((fingerprint.to_string(), now, id))
        .context("Failed to record verified host key fingerprint")?;

        Ok(())
    }

    pub fn load_session_restore_state(&self) -> Result<Option<SessionRestoreState>> {
        let conn = self.lock_connection()?;

        let states = conn.select::<SessionRestoreState>(
            "SELECT
                connection_profile_id,
                remote_path,
                last_opened_worktree,
                workspace_state_json,
                updated_at
             FROM session_restore_state
             WHERE singleton_id = 1
             LIMIT 1",
        )?()
        .context("Failed to load session restore state")?;

        Ok(states.into_iter().next())
    }

    pub fn save_session_restore_state(&self, state: &SessionRestoreState) -> Result<()> {
        let conn = self.lock_connection()?;

        conn.exec_bound::<(
            i64,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        )>(
            "INSERT INTO session_restore_state (
                singleton_id,
                connection_profile_id,
                remote_path,
                last_opened_worktree,
                workspace_state_json,
                updated_at
             )
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(singleton_id) DO UPDATE SET
                connection_profile_id = excluded.connection_profile_id,
                remote_path = excluded.remote_path,
                last_opened_worktree = excluded.last_opened_worktree,
                workspace_state_json = excluded.workspace_state_json,
                updated_at = excluded.updated_at",
        )?((
            1,
            state.connection_profile_id,
            state.remote_path.clone(),
            state.last_opened_worktree.clone(),
            state.workspace_state_json.clone(),
            state.updated_at,
        ))
        .context("Failed to save session restore state")?;

        conn.exec_bound::<(i64, Option<String>, i64)>(
            "UPDATE saved_connections
             SET last_session_timestamp = ?, last_opened_worktree = COALESCE(?, last_opened_worktree)
             WHERE id = ?",
        )?((state.updated_at, state.last_opened_worktree.clone(), state.connection_profile_id))
        .context("Failed to update profile restore metadata")?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn clear_session_restore_state(&self) -> Result<()> {
        let conn = self.lock_connection()?;
        conn.exec("DELETE FROM session_restore_state WHERE singleton_id = 1")?()
            .context("Failed to clear session restore state")?;
        Ok(())
    }

    pub fn last_session_connection_profile(&self) -> Result<Option<ConnectionProfile>> {
        let Some(state) = self.load_session_restore_state()? else {
            return Ok(None);
        };

        self.connection_profile_by_id(state.connection_profile_id)
    }

    #[allow(dead_code)]
    pub fn delete_connection_profile(&self, id: i64) -> Result<()> {
        let conn = self.lock_connection()?;
        conn.exec_bound::<i64>("DELETE FROM saved_connections WHERE id = ?")?(id)
            .context("Failed to delete connection profile")?;
        Ok(())
    }
}

pub fn credential_url(hostname: &str, username: &str, port: u16) -> String {
    format!("ssh://{}@{}:{}", username, hostname, port)
}

pub fn private_key_credential_url(hostname: &str, username: &str, port: u16) -> String {
    format!("ssh-key://{}@{}:{}/private-key", username, hostname, port)
}

pub fn private_key_passphrase_url(hostname: &str, username: &str, port: u16) -> String {
    format!("ssh-key://{}@{}:{}/passphrase", username, hostname, port)
}

pub fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let exists = conn.select_row::<i64>(&format!(
        "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}' LIMIT 1"
    ))?()
    .with_context(|| format!("Failed to query schema for {table}.{column}"))?
    .is_some();

    if !exists {
        conn.exec(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?()
        .with_context(|| format!("Failed to add column {table}.{column}"))?;
    }

    Ok(())
}

fn serialize_json_vec<T: Serialize>(value: &[T]) -> Result<Option<String>> {
    if value.is_empty() {
        Ok(None)
    } else {
        serde_json::to_string(value)
            .map(Some)
            .context("Failed to serialize JSON column")
    }
}

fn deserialize_json_vec<T: DeserializeOwned>(raw: Option<String>) -> Result<Vec<T>> {
    raw.map(|raw| serde_json::from_str(&raw))
        .transpose()
        .context("Failed to deserialize JSON column")
        .map(|value| value.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upsert_and_reload_connection_profile() {
        let db = ConnectionDb::open_memory().unwrap();

        let profile = db
            .upsert_connection_profile(&ConnectionProfileInput {
                hostname: "example.com".into(),
                username: "user".into(),
                port: 22,
                nickname: Some("My Server".into()),
                default_path: Some("~/code".into()),
                ssh_args: vec!["-i".into(), "~/.ssh/id_ed25519".into()],
                port_forwards: vec![SshPortForwardOption {
                    local_host: None,
                    local_port: 3000,
                    remote_host: Some("localhost".into()),
                    remote_port: 3000,
                }],
                auth_mode: AuthMode::KeychainSecret,
                upload_binary_over_ssh: true,
                private_key_name: None,
                private_key_fingerprint: None,
                last_successful_server_version: Some("0.1.0".into()),
                last_opened_worktree: Some("~/code".into()),
            })
            .unwrap();

        assert_eq!(profile.nickname.as_deref(), Some("My Server"));
        assert_eq!(profile.ssh_args, vec!["-i", "~/.ssh/id_ed25519"]);
        assert_eq!(profile.port_forwards.len(), 1);
        assert!(profile.upload_binary_over_ssh);
        assert_eq!(profile.auth_mode, AuthMode::KeychainSecret);
    }

    #[test]
    fn test_session_restore_round_trip() {
        let db = ConnectionDb::open_memory().unwrap();
        let profile = db
            .upsert_connection_profile(&ConnectionProfileInput {
                hostname: "example.com".into(),
                username: "user".into(),
                port: 22,
                ..ConnectionProfileInput::default()
            })
            .unwrap();

        db.save_session_restore_state(&SessionRestoreState {
            connection_profile_id: profile.id,
            remote_path: Some("~/zed".into()),
            last_opened_worktree: Some("~/zed".into()),
            workspace_state_json: Some(
                MobileWorkspaceSnapshotV1::new(Some("~/zed".into()), Some("~/zed".into()))
                    .to_json()
                    .unwrap(),
            ),
            updated_at: current_timestamp(),
        })
        .unwrap();

        let state = db.load_session_restore_state().unwrap().unwrap();
        assert_eq!(state.connection_profile_id, profile.id);
        assert_eq!(state.remote_path.as_deref(), Some("~/zed"));

        let restored_profile = db.last_session_connection_profile().unwrap().unwrap();
        assert_eq!(restored_profile.hostname, "example.com");
    }

    #[test]
    fn mobile_workspace_snapshot_round_trips() {
        let snapshot = MobileWorkspaceSnapshotV1::new(Some("~/zed".into()), Some("~/zed".into()));
        let raw = snapshot.to_json().unwrap();
        let decoded = MobileWorkspaceSnapshotV1::from_json(&raw).unwrap();
        assert_eq!(decoded.version, MobileWorkspaceSnapshotV1::VERSION);
        assert_eq!(decoded.remote_path.as_deref(), Some("~/zed"));
    }
}
