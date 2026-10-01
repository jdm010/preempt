//! Encrypted local history and privacy-preserving prediction feedback.
//!
//! The encryption key is never written to the database or a sidecar file. The
//! default app path manages it through the platform credential store; callers
//! can also supply a key directly to `HistoryStore::open`. Command redaction is
//! best-effort and removes common credential forms before history is persisted.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use preempt_predict::{history::HistoryEntry, Tier};
use hmac::{Hmac, Mac};
use keyring::{v1::Error as KeyringError, Entry};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::Sha256;

const SCHEMA_VERSION: i64 = 3;
const REDACTED: &str = "'[REDACTED]'";
// Keep these identifiers stable so existing encrypted history and feedback
// fingerprints remain readable after the product rename.
const KEYRING_SERVICE: &str = "com.auto-terminal.history";
const KEYRING_USER: &str = "sqlcipher-master-key";
const FINGERPRINT_DOMAIN: &[u8] = b"auto-terminal/suggestion-feedback/v1";
const PERSONALIZATION_STRENGTH: f64 = 0.35;

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Database(rusqlite::Error),
    EmptyKey,
    EncryptionUnavailable,
    WrongKeyOrCorruptDatabase,
    MissingDatabasePath,
    MissingCredential,
    CredentialStore(String),
    Randomness(String),
    UnsupportedSchema(i64),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "store filesystem error: {error}"),
            Self::Database(error) => write!(formatter, "store database error: {error}"),
            Self::EmptyKey => formatter.write_str("the database key must not be empty"),
            Self::EncryptionUnavailable => {
                formatter.write_str("the SQLite library does not provide SQLCipher encryption")
            },
            Self::WrongKeyOrCorruptDatabase => {
                formatter.write_str("the database key is incorrect or the database is corrupt")
            },
            Self::MissingDatabasePath => {
                formatter.write_str("no application data directory is available")
            },
            Self::MissingCredential => formatter.write_str(
                "the encrypted database exists but its OS credential is missing; refusing to replace the key",
            ),
            Self::CredentialStore(error) => write!(formatter, "OS credential store error: {error}"),
            Self::Randomness(error) => {
                write!(formatter, "could not generate a database key: {error}")
            },
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported history database schema version {version}")
            },
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCommand {
    pub id: i64,
    pub command: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub exit_code: Option<i32>,
    pub timestamp: i64,
}

pub struct NewCommand<'a> {
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub git_branch: Option<&'a str>,
    pub exit_code: Option<i32>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedbackCounts {
    pub t0_accepted: u64,
    pub t0_rejected: u64,
    pub t1_accepted: u64,
    pub t1_rejected: u64,
    pub t2_accepted: u64,
    pub t2_rejected: u64,
    pub t3_accepted: u64,
    pub t3_rejected: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuggestionFeedback {
    pub accepted: u64,
    pub rejected: u64,
}

/// In-memory per-suggestion preferences loaded from encrypted feedback.
pub struct SuggestionPersonalizer {
    fingerprint_key: Arc<[u8]>,
    feedback: HashMap<String, SuggestionFeedback>,
}

impl SuggestionPersonalizer {
    /// Return a bounded score multiplier based on prior outcomes for this suggestion.
    pub fn score_multiplier(&self, command: &str, tier: Tier) -> f64 {
        1.0 + PERSONALIZATION_STRENGTH * self.preference(command, tier)
    }

    /// Return the smoothed acceptance preference in the range -1.0..=1.0.
    pub fn preference(&self, command: &str, tier: Tier) -> f64 {
        let fingerprint = suggestion_fingerprint(&self.fingerprint_key, command, tier);
        let Some(feedback) = self.feedback.get(&fingerprint) else {
            return 0.0;
        };
        let total = feedback.accepted.saturating_add(feedback.rejected);
        (feedback.accepted as f64 - feedback.rejected as f64) / (total as f64 + 2.0)
    }

    /// Update the session's preference immediately; persistence is handled separately.
    pub fn record(&mut self, command: &str, tier: Tier, accepted: bool) {
        let fingerprint = suggestion_fingerprint(&self.fingerprint_key, command, tier);
        let feedback = self.feedback.entry(fingerprint).or_default();
        let count = if accepted {
            &mut feedback.accepted
        } else {
            &mut feedback.rejected
        };
        *count = count.saturating_add(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordReceipt {
    pub id: i64,
    /// Number of fields or tokens changed by the common-secret redactor.
    pub redactions: usize,
}

/// A SQLCipher-backed history store. The connection and encryption key are
/// kept only in memory for this handle's lifetime.
pub struct HistoryStore {
    connection: Connection,
    fingerprint_key: Arc<[u8]>,
}

/// Nonblocking sender for persisting aggregate and per-suggestion feedback.
pub struct FeedbackWriter {
    sender: Option<mpsc::Sender<FeedbackRecord>>,
    worker: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
    fingerprint_key: Arc<[u8]>,
}

enum FeedbackRecord {
    Tier(Tier, bool),
    Suggestion(String, bool),
}

impl FeedbackWriter {
    pub fn record(&self, tier: Tier, accepted: bool) {
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.send(FeedbackRecord::Tier(tier, accepted)).is_err())
        {
            self.set_error("feedback persistence worker stopped".to_owned());
        }
    }

    /// Queue a keyed fingerprint of a redacted command without sending the command to the worker.
    pub fn record_suggestion(&self, command: &str, tier: Tier, accepted: bool) {
        let fingerprint = suggestion_fingerprint(&self.fingerprint_key, command, tier);
        if self.sender.as_ref().is_none_or(|sender| {
            sender
                .send(FeedbackRecord::Suggestion(fingerprint, accepted))
                .is_err()
        }) {
            self.set_error("feedback persistence worker stopped".to_owned());
        }
    }

    /// Take the first persistence error, if one has occurred.
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().ok()?.take()
    }

    fn set_error(&self, error: String) {
        if let Ok(mut slot) = self.error.lock() {
            if slot.is_none() {
                *slot = Some(error);
            }
        }
    }
}

impl Drop for FeedbackWriter {
    fn drop(&mut self) {
        // Closing the channel lets the worker drain queued writes before exit.
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl HistoryStore {
    /// Open the default encrypted database, obtaining its key from the OS keychain.
    /// A fresh key is generated only when this application's credential is absent.
    pub fn open_default_with_keyring() -> Result<Self, StoreError> {
        let path = default_database_path().ok_or(StoreError::MissingDatabasePath)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            secure_database_directory(parent)?;
        }

        let entry = Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .map_err(|error| StoreError::CredentialStore(error.to_string()))?;
        let key = match entry.get_password() {
            Ok(key) => key,
            Err(KeyringError::NoEntry) if path.exists() => {
                return Err(StoreError::MissingCredential);
            }
            Err(KeyringError::NoEntry) => {
                let mut bytes = [0_u8; 32];
                getrandom::fill(&mut bytes)
                    .map_err(|error| StoreError::Randomness(error.to_string()))?;
                let key = bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                entry
                    .set_password(&key)
                    .map_err(|error| StoreError::CredentialStore(error.to_string()))?;
                // Read back the credential selected by the platform store.
                entry
                    .get_password()
                    .map_err(|error| StoreError::CredentialStore(error.to_string()))?
            }
            Err(error) => return Err(StoreError::CredentialStore(error.to_string())),
        };

        Self::open(path, &key)
    }

    /// Open or create an encrypted history database.
    ///
    /// SQLCipher's key pragma is applied before the first database read. A
    /// normal SQLite library is rejected instead of silently creating a
    /// plaintext database.
    pub fn open(path: impl AsRef<Path>, key: &str) -> Result<Self, StoreError> {
        if key.is_empty() || key.as_bytes().contains(&0) {
            return Err(StoreError::EmptyKey);
        }
        let fingerprint_key = derive_fingerprint_key(key.as_bytes());

        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let connection = Connection::open(path)?;
        connection.pragma_update(None, "key", key)?;

        let cipher_version: Option<String> = connection
            .pragma_query_value(None, "cipher_version", |row| row.get(0))
            .optional()?;
        if cipher_version.as_deref().is_none_or(str::is_empty) {
            return Err(StoreError::EncryptionUnavailable);
        }

        // Force SQLCipher to read a page now so a bad key or damaged database
        // fails before any schema or history operation is attempted.
        let validation: rusqlite::Result<i64> =
            connection.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0));
        if validation.is_err() {
            return Err(StoreError::WrongKeyOrCorruptDatabase);
        }

        secure_database_file(path)?;
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "secure_delete", "ON")?;
        initialize_schema(&connection)?;

        Ok(Self {
            connection,
            fingerprint_key,
        })
    }

    /// Insert a command after redacting common secret patterns.
    pub fn record_command(&self, command: NewCommand<'_>) -> Result<RecordReceipt, StoreError> {
        let (command_text, mut redactions) = redact_text(command.command);
        let cwd = command.cwd.map(redact_text);
        let branch = command.git_branch.map(redact_text);
        redactions += cwd.as_ref().map_or(0, |(_, count)| *count);
        redactions += branch.as_ref().map_or(0, |(_, count)| *count);

        self.connection.execute(
            "INSERT INTO command_history (command, cwd, git_branch, exit_code, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                command_text,
                cwd.map(|(text, _)| text),
                branch.map(|(text, _)| text),
                command.exit_code,
                command.timestamp,
            ],
        )?;

        Ok(RecordReceipt {
            id: self.connection.last_insert_rowid(),
            redactions,
        })
    }

    /// Refresh the imported shell-history rows while preserving runtime rows.
    /// All imported commands pass through the same redactor as runtime records.
    pub fn replace_shell_history(&mut self, entries: &[HistoryEntry]) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM command_history WHERE source = 'shell'", [])?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO command_history (command, timestamp, source) VALUES (?1, ?2, 'shell')",
            )?;
            for entry in entries {
                let (command, _) = redact_text(&entry.command);
                statement.execute(params![command, entry.timestamp.unwrap_or(0)])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Read recent commands in chronological order for predictor construction.
    /// Entries containing a redaction marker are kept encrypted but never offered
    /// as executable suggestions.
    pub fn prediction_history(&self, limit: usize) -> Result<Vec<HistoryEntry>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT command, timestamp FROM (
                 SELECT command, timestamp, id FROM command_history
                 ORDER BY timestamp DESC, id DESC LIMIT ?1
             ) ORDER BY timestamp ASC, id ASC",
        )?;
        let rows = statement.query_map([limit.min(i64::MAX as usize) as i64], |row| {
            let timestamp: i64 = row.get(1)?;
            Ok(HistoryEntry {
                command: row.get(0)?,
                timestamp: (timestamp != 0).then_some(timestamp),
            })
        })?;
        let mut entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        entries.retain(|entry| !entry.command.contains("[REDACTED"));
        Ok(entries)
    }

    /// Return the newest stored commands, with all persisted text already redacted.
    pub fn recent_commands(&self, limit: usize) -> Result<Vec<StoredCommand>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, command, cwd, git_branch, exit_code, timestamp
             FROM command_history ORDER BY timestamp DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(i64::MAX as usize) as i64], |row| {
            Ok(StoredCommand {
                id: row.get(0)?,
                command: row.get(1)?,
                cwd: row.get(2)?,
                git_branch: row.get(3)?,
                exit_code: row.get(4)?,
                timestamp: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// Persist aggregate tier counts without associating them with command text.
    pub fn record_feedback(&self, tier: Tier, accepted: bool) -> Result<(), StoreError> {
        let tier_id = tier_id(tier);
        let column = if accepted { "accepted" } else { "rejected" };
        let sql = format!(
            "INSERT INTO prediction_feedback (tier, {column}) VALUES (?1, 1)
             ON CONFLICT(tier) DO UPDATE SET {column} = {column} + 1"
        );
        self.connection.execute(&sql, [tier_id])?;
        Ok(())
    }

    /// Persist one outcome under a keyed fingerprint of the redacted command.
    fn record_suggestion_feedback(
        &self,
        fingerprint: &str,
        accepted: bool,
    ) -> Result<(), StoreError> {
        let column = if accepted { "accepted" } else { "rejected" };
        let sql = format!(
            "INSERT INTO suggestion_feedback (fingerprint, {column}) VALUES (?1, 1)
             ON CONFLICT(fingerprint) DO UPDATE SET {column} = {column} + 1"
        );
        self.connection.execute(&sql, [fingerprint])?;
        Ok(())
    }

    /// Load the local feedback model without exposing suggestion text.
    pub fn suggestion_personalizer(&self) -> Result<SuggestionPersonalizer, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT fingerprint, accepted, rejected FROM suggestion_feedback")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut feedback = HashMap::new();
        for row in rows {
            let (fingerprint, accepted, rejected) = row?;
            feedback.insert(
                fingerprint,
                SuggestionFeedback {
                    accepted: accepted as u64,
                    rejected: rejected as u64,
                },
            );
        }
        Ok(SuggestionPersonalizer {
            fingerprint_key: Arc::clone(&self.fingerprint_key),
            feedback,
        })
    }

    pub fn feedback_counts(&self) -> Result<FeedbackCounts, StoreError> {
        let mut counts = FeedbackCounts::default();
        let mut statement = self
            .connection
            .prepare("SELECT tier, accepted, rejected FROM prediction_feedback ORDER BY tier")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        for row in rows {
            let (tier, accepted, rejected) = row?;
            match tier {
                0 => (counts.t0_accepted, counts.t0_rejected) = (accepted as u64, rejected as u64),
                1 => (counts.t1_accepted, counts.t1_rejected) = (accepted as u64, rejected as u64),
                2 => (counts.t2_accepted, counts.t2_rejected) = (accepted as u64, rejected as u64),
                3 => (counts.t3_accepted, counts.t3_rejected) = (accepted as u64, rejected as u64),
                _ => (),
            }
        }
        Ok(counts)
    }

    pub fn clear_history(&self) -> Result<(), StoreError> {
        self.connection.execute("DELETE FROM command_history", [])?;
        Ok(())
    }

    /// Transfer ownership of the database connection to a background writer.
    pub fn into_feedback_writer(self) -> Result<FeedbackWriter, StoreError> {
        let (sender, receiver) = mpsc::channel();
        let error = Arc::new(Mutex::new(None));
        let worker_error = Arc::clone(&error);
        let fingerprint_key = Arc::clone(&self.fingerprint_key);
        let worker = thread::Builder::new()
            .name("preempt-feedback-store".to_owned())
            .spawn(move || {
                while let Ok(record) = receiver.recv() {
                    let result = match record {
                        FeedbackRecord::Tier(tier, accepted) => {
                            self.record_feedback(tier, accepted)
                        }
                        FeedbackRecord::Suggestion(fingerprint, accepted) => {
                            self.record_suggestion_feedback(&fingerprint, accepted)
                        }
                    };
                    if let Err(store_error) = result {
                        if let Ok(mut slot) = worker_error.lock() {
                            if slot.is_none() {
                                *slot = Some(store_error.to_string());
                            }
                        }
                    }
                }
            })
            .map_err(StoreError::Io)?;
        Ok(FeedbackWriter {
            sender: Some(sender),
            worker: Some(worker),
            error,
            fingerprint_key,
        })
    }
}

/// Return the platform's conventional application-data location for this DB.
pub fn default_database_path() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("auto-terminal")
                .join("history.db")
        });
    }
    if cfg!(target_os = "windows") {
        return std::env::var_os("APPDATA").map(|app_data| {
            PathBuf::from(app_data)
                .join("auto-terminal")
                .join("history.db")
        });
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .map(|data| data.join("auto-terminal").join("history.db"))
}

fn derive_fingerprint_key(database_key: &[u8]) -> Arc<[u8]> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(database_key).expect("HMAC accepts keys of any length");
    mac.update(FINGERPRINT_DOMAIN);
    Arc::from(mac.finalize().into_bytes().to_vec())
}

fn suggestion_fingerprint(fingerprint_key: &[u8], command: &str, tier: Tier) -> String {
    let (redacted, _) = redact_text(command);
    let mut mac =
        Hmac::<Sha256>::new_from_slice(fingerprint_key).expect("HMAC accepts keys of any length");
    mac.update(&tier_id(tier).to_be_bytes());
    mac.update(&[0]);
    mac.update(redacted.as_bytes());
    let digest = mac.finalize().into_bytes();

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        fingerprint.push(char::from(HEX[(byte >> 4) as usize]));
        fingerprint.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    fingerprint
}

fn initialize_schema(connection: &Connection) -> Result<(), StoreError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            connection.execute_batch(
                "BEGIN;
                 CREATE TABLE command_history (
                     id INTEGER PRIMARY KEY,
                     command TEXT NOT NULL,
                     cwd TEXT,
                     git_branch TEXT,
                     exit_code INTEGER,
                     timestamp INTEGER NOT NULL,
                     source TEXT NOT NULL DEFAULT 'runtime'
                 );
                 CREATE INDEX command_history_timestamp ON command_history(timestamp DESC, id DESC);
                 CREATE TABLE prediction_feedback (
                     tier INTEGER PRIMARY KEY CHECK (tier BETWEEN 0 AND 3),
                     accepted INTEGER NOT NULL DEFAULT 0 CHECK (accepted >= 0),
                     rejected INTEGER NOT NULL DEFAULT 0 CHECK (rejected >= 0)
                 );
                 INSERT INTO prediction_feedback (tier) VALUES (0), (1), (2), (3);
                 CREATE TABLE suggestion_feedback (
                     fingerprint TEXT PRIMARY KEY NOT NULL CHECK (length(fingerprint) = 64),
                     accepted INTEGER NOT NULL DEFAULT 0 CHECK (accepted >= 0),
                     rejected INTEGER NOT NULL DEFAULT 0 CHECK (rejected >= 0)
                 );
                 PRAGMA user_version = 3;
                 COMMIT;",
            )?;
        }
        1 => {
            connection.execute_batch(
                "BEGIN;
                 ALTER TABLE command_history
                     ADD COLUMN source TEXT NOT NULL DEFAULT 'runtime';
                 CREATE TABLE IF NOT EXISTS prediction_feedback (
                     tier INTEGER PRIMARY KEY CHECK (tier BETWEEN 0 AND 3),
                     accepted INTEGER NOT NULL DEFAULT 0 CHECK (accepted >= 0),
                     rejected INTEGER NOT NULL DEFAULT 0 CHECK (rejected >= 0)
                 );
                 INSERT OR IGNORE INTO prediction_feedback (tier) VALUES (0), (1), (2), (3);
                 CREATE TABLE suggestion_feedback (
                     fingerprint TEXT PRIMARY KEY NOT NULL CHECK (length(fingerprint) = 64),
                     accepted INTEGER NOT NULL DEFAULT 0 CHECK (accepted >= 0),
                     rejected INTEGER NOT NULL DEFAULT 0 CHECK (rejected >= 0)
                 );
                 PRAGMA user_version = 3;
                 COMMIT;",
            )?;
        }
        2 => {
            connection.execute_batch(
                "BEGIN;
                 CREATE TABLE suggestion_feedback (
                     fingerprint TEXT PRIMARY KEY NOT NULL CHECK (length(fingerprint) = 64),
                     accepted INTEGER NOT NULL DEFAULT 0 CHECK (accepted >= 0),
                     rejected INTEGER NOT NULL DEFAULT 0 CHECK (rejected >= 0)
                 );
                 PRAGMA user_version = 3;
                 COMMIT;",
            )?;
        }
        SCHEMA_VERSION => (),
        other => return Err(StoreError::UnsupportedSchema(other)),
    }
    Ok(())
}

#[cfg(unix)]
fn secure_database_directory(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_database_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn tier_id(tier: Tier) -> i64 {
    match tier {
        Tier::T0Prefix => 0,
        Tier::T1Ngram => 1,
        Tier::T2LocalLlm => 2,
        Tier::T3Cloud => 3,
    }
}

#[cfg(unix)]
fn secure_database_file(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;

    if path != Path::new(":memory:") && path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_database_file(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[derive(Debug)]
struct Replacement {
    start: usize,
    end: usize,
    text: String,
}

fn redact_text(input: &str) -> (String, usize) {
    let lower = input.to_ascii_lowercase();
    if lower.contains("private key-----") {
        return ("[REDACTED COMMAND]".to_owned(), 1);
    }

    let spans = word_spans(input);
    let mut replacements = Vec::new();
    let mut redactions = 0;
    let mut redact_next = false;

    for (start, end) in spans {
        let word = &input[start..end];
        if redact_next {
            replacements.push(Replacement {
                start,
                end,
                text: REDACTED.to_owned(),
            });
            redactions += 1;
            redact_next = false;
            continue;
        }

        if let Some(equals) = word.find('=') {
            if is_sensitive_name(&word[..equals]) && equals + 1 < word.len() {
                let mut value_start = equals + 1;
                let mut value_end = word.len();
                let value = &word[value_start..value_end];
                let value_quote = value
                    .chars()
                    .next()
                    .filter(|quote| *quote == '\'' || *quote == '"');
                let replacement = if value.len() >= 2
                    && value_quote.is_some()
                    && value.ends_with(value_quote.unwrap())
                {
                    value_start += 1;
                    value_end -= 1;
                    "[REDACTED]"
                } else if (word.starts_with('\'') && word.ends_with('\''))
                    || (word.starts_with('"') && word.ends_with('"'))
                {
                    value_end -= 1;
                    "[REDACTED]"
                } else {
                    REDACTED
                };
                replacements.push(Replacement {
                    start: start + value_start,
                    end: start + value_end,
                    text: replacement.to_owned(),
                });
                redactions += 1;
                continue;
            }
        }

        if is_sensitive_option(word) {
            redact_next = true;
            continue;
        }

        if let Some((secret_start, secret_end)) =
            bearer_value_range(word).or_else(|| url_password_range(word))
        {
            replacements.push(Replacement {
                start: start + secret_start,
                end: start + secret_end,
                text: "[REDACTED]".to_owned(),
            });
            redactions += 1;
            continue;
        }

        if looks_like_token(word) {
            replacements.push(Replacement {
                start,
                end,
                text: REDACTED.to_owned(),
            });
            redactions += 1;
        }
    }

    if replacements.is_empty() {
        return (input.to_owned(), 0);
    }
    replacements.sort_by_key(|replacement| replacement.start);
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    for replacement in replacements {
        if replacement.start < cursor {
            continue;
        }
        output.push_str(&input[cursor..replacement.start]);
        output.push_str(&replacement.text);
        cursor = replacement.end;
    }
    output.push_str(&input[cursor..]);
    (output, redactions)
}

/// Return a command only when the common-secret redactor leaves it unchanged.
/// Fine-tuning skips partially redacted commands to avoid teaching the model
/// the surrounding structure of a secret-bearing invocation.
pub fn training_safe_command(input: &str) -> Option<String> {
    let command = input.trim();
    if command.is_empty() || command.chars().count() > 256 || command.chars().any(char::is_control)
    {
        return None;
    }

    let lower = command.to_ascii_lowercase();
    let sensitive_markers = [
        "[redacted",
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "access_token",
        "authorization",
        "credential",
        "private key",
        ".env",
        ".netrc",
        ".npmrc",
        ".pypirc",
        "id_rsa",
        "id_ed25519",
    ];
    if sensitive_markers
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return None;
    }

    let (redacted, redactions) = redact_text(command);
    (redactions == 0 && redacted == command).then(|| command.to_owned())
}

fn word_spans(input: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if start.is_none() {
            if ch.is_whitespace() {
                continue;
            }
            start = Some(index);
        }

        if escaped {
            escaped = false;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else if ch == '\\' && active_quote == '"' {
                escaped = true;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => escaped = true,
            c if c.is_whitespace() => {
                if let Some(word_start) = start.take() {
                    spans.push((word_start, index));
                }
            }
            _ => (),
        }
    }
    if let Some(word_start) = start {
        spans.push((word_start, input.len()));
    }
    spans
}

fn is_sensitive_name(name: &str) -> bool {
    let normalized: String = name
        .trim_matches(|ch| ch == '\'' || ch == '"')
        .trim_start_matches('-')
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    [
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "apikey",
        "accesskey",
        "privatekey",
        "clientsecret",
        "authorization",
        "credential",
        "cookie",
        "sessionkey",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn is_sensitive_option(word: &str) -> bool {
    let option = word.trim_matches(|ch| ch == '\'' || ch == '"');
    let lower = option.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "-u" | "--user" | "--proxy-user" | "--http-user" | "--ftp-user" | "--auth"
    ) || (option.starts_with('-') && !option.contains('=') && is_sensitive_name(option))
}

fn bearer_value_range(word: &str) -> Option<(usize, usize)> {
    let lower = word.to_ascii_lowercase();
    let (marker, marker_len) = ["bearer ", "basic "]
        .iter()
        .find_map(|marker| lower.find(marker).map(|index| (index, marker.len())))?;
    let start = marker + marker_len;
    let mut end = start;
    while end < word.len() {
        let byte = word.as_bytes()[end];
        if byte.is_ascii_whitespace() || byte == b'\'' || byte == b'"' {
            break;
        }
        end += 1;
    }
    (end > start).then_some((start, end))
}

fn url_password_range(word: &str) -> Option<(usize, usize)> {
    let scheme_end = word.find("://")? + 3;
    let authority_end = word[scheme_end..]
        .find(['/', '?', '#'])
        .map_or(word.len(), |offset| scheme_end + offset);
    let at = word[scheme_end..authority_end].rfind('@')? + scheme_end;
    let colon = word[scheme_end..at].rfind(':')? + scheme_end;
    (colon + 1 < at).then_some((colon + 1, at))
}

fn looks_like_token(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    for prefix in [
        "ghp_",
        "github_pat_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "xoxb-",
        "xoxp-",
        "sk_live_",
    ] {
        if let Some(index) = lower.find(prefix) {
            if word.len().saturating_sub(index + prefix.len()) >= 12 {
                return true;
            }
        }
    }

    if let Some(index) = lower.find("akia") {
        if word[index + 4..].chars().take(16).count() == 16
            && word[index + 4..]
                .chars()
                .take(16)
                .all(|ch| ch.is_ascii_alphanumeric())
        {
            return true;
        }
    }

    let jwt_parts: Vec<_> = word.split('.').collect();
    jwt_parts.len() == 3 && jwt_parts.iter().all(|part| part.len() >= 12)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_database(label: &str) -> (PathBuf, PathBuf) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "preempt-store-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&directory).unwrap();
        (directory.clone(), directory.join("history.db"))
    }

    #[test]
    fn database_is_encrypted_and_rejects_a_wrong_key() {
        let (directory, path) = temporary_database("cipher");
        {
            let store = HistoryStore::open(&path, "correct test key").unwrap();
            store
                .record_command(NewCommand {
                    command: "printf ciphertext-probe",
                    cwd: None,
                    git_branch: None,
                    exit_code: Some(0),
                    timestamp: 1_700_000_000,
                })
                .unwrap();
        }

        let database = fs::read(&path).unwrap();
        assert!(!database.starts_with(b"SQLite format 3\0"));
        assert!(!database
            .windows(b"ciphertext-probe".len())
            .any(|window| window == b"ciphertext-probe"));
        assert!(matches!(
            HistoryStore::open(&path, "wrong test key"),
            Err(StoreError::WrongKeyOrCorruptDatabase)
        ));

        let store = HistoryStore::open(&path, "correct test key").unwrap();
        assert_eq!(
            store.recent_commands(1).unwrap()[0].command,
            "printf ciphertext-probe"
        );
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn imported_history_is_redacted_and_redacted_rows_are_not_suggested() {
        let (directory, path) = temporary_database("redaction");
        let mut store = HistoryStore::open(&path, "test key").unwrap();
        store
            .replace_shell_history(&[
                HistoryEntry {
                    command: "export AWS_SECRET_ACCESS_KEY=do-not-store-this".to_owned(),
                    timestamp: Some(1_700_000_000),
                },
                HistoryEntry {
                    command: "git status".to_owned(),
                    timestamp: Some(1_700_000_001),
                },
            ])
            .unwrap();

        let stored = store.recent_commands(10).unwrap();
        assert!(stored
            .iter()
            .any(|entry| entry.command.contains("[REDACTED]")));
        assert!(stored
            .iter()
            .all(|entry| !entry.command.contains("do-not-store-this")));
        let suggestions = store.prediction_history(10).unwrap();
        let suggested_commands: Vec<_> = suggestions
            .iter()
            .map(|entry| entry.command.as_str())
            .collect();
        assert_eq!(suggested_commands, ["git status"]);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn background_feedback_writer_drains_before_drop_returns() {
        let (directory, path) = temporary_database("feedback");
        {
            let store = HistoryStore::open(&path, "test key").unwrap();
            let writer = store.into_feedback_writer().unwrap();
            writer.record(Tier::T1Ngram, true);
            writer.record(Tier::T0Prefix, false);
        }

        let store = HistoryStore::open(&path, "test key").unwrap();
        let counts = store.feedback_counts().unwrap();
        assert_eq!(counts.t1_accepted, 1);
        assert_eq!(counts.t0_rejected, 1);
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }
}
