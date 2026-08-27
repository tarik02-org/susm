use std::{path::PathBuf, sync::Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::{M, Migrations};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct StoredWorkload {
    pub id: String,
    pub kind: String,
    pub definition: Option<Vec<u8>>,
    pub enabled: bool,
    pub last_outcome: String,
}

#[derive(Clone, Debug)]
pub struct StoredExecution {
    pub id: String,
    pub workload_id: String,
    pub state: String,
    pub supervisor_process_id: u32,
    pub workload_process_id: u32,
    pub attempt: u32,
    pub started_unix_ms: i64,
    pub ended_unix_ms: i64,
    pub exit_code: Option<u32>,
    pub error: String,
    pub snapshot: Vec<u8>,
    pub committed_sequence: u64,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("controller storage connection is unavailable")]
    Unavailable,
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("SQLite schema migration failed: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("stored integer is outside its domain")]
    InvalidInteger,
    #[error("observation sequence {actual} is out of order; expected {expected}")]
    ObservationOutOfOrder { expected: u64, actual: u64 },
}

pub struct TerminalObservation {
    pub state: String,
    pub ended_unix_ms: i64,
    pub exit_code: Option<u32>,
    pub error: String,
    pub attempt: u32,
}

pub struct ProgressObservation {
    pub state: String,
    pub workload_process_id: u32,
    pub attempt: u32,
    pub error: Option<String>,
}

pub enum ObservationUpdate {
    Acknowledge,
    Progress(ProgressObservation),
    Terminal(TerminalObservation),
}

pub struct Storage {
    connection: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: PathBuf) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| StorageError::Unavailable)?;
        }
        let mut connection = Connection::open(path)?;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn replace_definitions(
        &self,
        generation: Vec<u8>,
        definitions: Vec<StoredWorkload>,
    ) -> Result<bool, StorageError> {
        self.with_connection(|connection| replace_definitions(connection, &generation, definitions))
    }

    pub fn load_workloads(&self) -> Result<Vec<StoredWorkload>, StorageError> {
        self.with_connection(|connection| load_workloads(connection))
    }

    pub fn set_enabled(&self, id: String, enabled: bool) -> Result<bool, StorageError> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE workloads SET enabled = ?2 WHERE id = ?1",
                    params![id, enabled],
                )
                .map(|changed| changed != 0)
                .map_err(StorageError::from)
        })
    }

    pub fn insert_enabled_job_execution(
        &self,
        execution: StoredExecution,
        manager_session_id: String,
    ) -> Result<bool, StorageError> {
        self.with_connection(|connection| {
            insert_enabled_job_execution(connection, &execution, &manager_session_id)
        })
    }

    pub fn insert_execution(&self, execution: StoredExecution) -> Result<(), StorageError> {
        self.with_connection(|connection| insert_execution(connection, &execution))
    }

    pub fn set_execution_supervisor(
        &self,
        id: String,
        supervisor_process_id: u32,
    ) -> Result<(), StorageError> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE executions
                     SET state = 'launching', supervisor_process_id = ?2
                     WHERE id = ?1",
                    params![id, supervisor_process_id],
                )
                .map(|_| ())
                .map_err(StorageError::from)
        })
    }

    pub fn finish_execution(&self, execution: StoredExecution) -> Result<(), StorageError> {
        self.with_connection(|connection| finish_execution(connection, &execution))
    }

    pub fn commit_observation(
        &self,
        id: String,
        sequence: u64,
        update: ObservationUpdate,
    ) -> Result<bool, StorageError> {
        self.with_connection(|connection| commit_observation(connection, &id, sequence, update))
    }

    pub fn list_executions(
        &self,
        workload_id: String,
        limit: u32,
        before: Option<i64>,
    ) -> Result<Vec<StoredExecution>, StorageError> {
        self.with_connection(|connection| list_executions(connection, &workload_id, limit, before))
    }

    pub fn get_execution(&self, id: String) -> Result<Option<StoredExecution>, StorageError> {
        self.with_connection(|connection| get_execution(connection, &id))
    }

    pub fn load_active_executions(&self) -> Result<Vec<StoredExecution>, StorageError> {
        self.with_connection(|connection| load_active_executions(connection))
    }

    fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        operation(&mut connection)
    }
}

fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    migrations().to_latest(connection)?;
    Ok(())
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up_with_hook(
            "CREATE TABLE IF NOT EXISTS metadata (
           key TEXT PRIMARY KEY,
           value BLOB NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS workloads (
           id TEXT PRIMARY KEY,
           kind TEXT NOT NULL CHECK(kind IN ('service', 'job')),
           definition BLOB,
           enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)) DEFAULT 0,
           last_triggered_session TEXT NOT NULL DEFAULT '',
           last_outcome TEXT NOT NULL DEFAULT ''
         ) STRICT;
         CREATE TABLE IF NOT EXISTS executions (
           id TEXT PRIMARY KEY,
           workload_id TEXT NOT NULL REFERENCES workloads(id),
           state TEXT NOT NULL,
           process_id INTEGER NOT NULL DEFAULT 0,
           started_unix_ms INTEGER NOT NULL,
           ended_unix_ms INTEGER NOT NULL DEFAULT 0,
           exit_code INTEGER,
           error TEXT NOT NULL DEFAULT '',
           snapshot BLOB NOT NULL,
           committed_sequence INTEGER NOT NULL DEFAULT 0
         ) STRICT;
         CREATE INDEX IF NOT EXISTS executions_by_workload
           ON executions(workload_id, started_unix_ms DESC);",
            |transaction| {
                let has_last_triggered_session = transaction.query_row(
                    "SELECT EXISTS(
                   SELECT 1 FROM pragma_table_info('workloads')
                   WHERE name = 'last_triggered_session'
                 )",
                    [],
                    |row| row.get::<_, bool>(0),
                )?;
                if !has_last_triggered_session {
                    transaction.execute(
                        "ALTER TABLE workloads
                     ADD COLUMN last_triggered_session TEXT NOT NULL DEFAULT ''",
                        [],
                    )?;
                }
                Ok(())
            },
        ),
        M::up(
            "ALTER TABLE executions RENAME COLUMN process_id TO supervisor_process_id;
             ALTER TABLE executions
               ADD COLUMN workload_process_id INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE executions
               ADD COLUMN attempt INTEGER NOT NULL DEFAULT 0;",
        ),
    ])
}

fn replace_definitions(
    connection: &mut Connection,
    generation: &[u8],
    definitions: Vec<StoredWorkload>,
) -> Result<bool, StorageError> {
    let current = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'config_generation'",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    if current.as_deref() == Some(generation) {
        return Ok(false);
    }
    let transaction = connection.transaction()?;
    transaction.execute("UPDATE workloads SET definition = NULL", [])?;
    for definition in definitions {
        transaction.execute(
            "INSERT INTO workloads(id, kind, definition, enabled)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, definition = excluded.definition",
            params![
                definition.id,
                definition.kind,
                definition.definition,
                definition.enabled
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO metadata(key, value) VALUES ('config_generation', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [generation],
    )?;
    transaction.commit()?;
    Ok(true)
}

fn load_workloads(connection: &Connection) -> Result<Vec<StoredWorkload>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT id, kind, definition, enabled, last_outcome
             FROM workloads ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(StoredWorkload {
            id: row.get(0)?,
            kind: row.get(1)?,
            definition: row.get(2)?,
            enabled: row.get(3)?,
            last_outcome: row.get(4)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn insert_execution(
    connection: &Connection,
    execution: &StoredExecution,
) -> Result<(), StorageError> {
    connection.execute(
        "INSERT INTO executions(
           id, workload_id, state, supervisor_process_id, workload_process_id,
           attempt, started_unix_ms, snapshot
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            execution.id,
            execution.workload_id,
            execution.state,
            execution.supervisor_process_id,
            execution.workload_process_id,
            execution.attempt,
            execution.started_unix_ms,
            execution.snapshot
        ],
    )?;
    Ok(())
}

fn insert_enabled_job_execution(
    connection: &mut Connection,
    execution: &StoredExecution,
    manager_session_id: &str,
) -> Result<bool, StorageError> {
    let transaction = connection.transaction()?;
    let claimed = transaction.execute(
        "UPDATE workloads SET last_triggered_session = ?2
         WHERE id = ?1 AND kind = 'job' AND enabled = 1
           AND last_triggered_session <> ?2",
        params![execution.workload_id, manager_session_id],
    )?;
    if claimed == 0 {
        transaction.commit()?;
        return Ok(false);
    }
    insert_execution(&transaction, execution)?;
    transaction.commit()?;
    Ok(true)
}

fn finish_execution(
    connection: &mut Connection,
    execution: &StoredExecution,
) -> Result<(), StorageError> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "UPDATE executions
         SET state = ?2, supervisor_process_id = ?3, workload_process_id = ?4,
             attempt = ?5, ended_unix_ms = ?6, exit_code = ?7, error = ?8
         WHERE id = ?1",
        params![
            execution.id,
            execution.state,
            execution.supervisor_process_id,
            execution.workload_process_id,
            execution.attempt,
            execution.ended_unix_ms,
            execution.exit_code,
            execution.error
        ],
    )?;
    transaction.execute(
        "UPDATE workloads SET last_outcome = ?2 WHERE id = ?1",
        params![execution.workload_id, execution.state],
    )?;
    transaction.commit()?;
    Ok(())
}

fn commit_observation(
    connection: &mut Connection,
    id: &str,
    sequence: u64,
    update: ObservationUpdate,
) -> Result<bool, StorageError> {
    let sequence = i64::try_from(sequence).map_err(|_| StorageError::InvalidInteger)?;
    let transaction = connection.transaction()?;
    let committed = transaction.query_row(
        "SELECT committed_sequence FROM executions WHERE id = ?1",
        [id],
        |row| row.get::<_, i64>(0),
    )?;
    if sequence <= committed {
        transaction.commit()?;
        return Ok(false);
    }
    let expected = committed
        .checked_add(1)
        .ok_or(StorageError::InvalidInteger)?;
    if sequence != expected {
        return Err(StorageError::ObservationOutOfOrder {
            expected: u64::try_from(expected).map_err(|_| StorageError::InvalidInteger)?,
            actual: u64::try_from(sequence).map_err(|_| StorageError::InvalidInteger)?,
        });
    }
    match update {
        ObservationUpdate::Acknowledge => {
            transaction.execute(
                "UPDATE executions SET committed_sequence = ?2 WHERE id = ?1",
                params![id, sequence],
            )?;
        }
        ObservationUpdate::Progress(progress) => {
            transaction.execute(
                "UPDATE executions
                 SET state = ?2, workload_process_id = ?3, attempt = ?4,
                     error = COALESCE(?5, error), committed_sequence = ?6
                 WHERE id = ?1",
                params![
                    id,
                    progress.state,
                    progress.workload_process_id,
                    progress.attempt,
                    progress.error,
                    sequence
                ],
            )?;
        }
        ObservationUpdate::Terminal(terminal) => {
            transaction.execute(
                "UPDATE executions
                 SET state = ?2, workload_process_id = 0, attempt = ?3,
                     ended_unix_ms = ?4, exit_code = ?5, error = ?6,
                     committed_sequence = ?7
                 WHERE id = ?1",
                params![
                    id,
                    terminal.state,
                    terminal.attempt,
                    terminal.ended_unix_ms,
                    terminal.exit_code,
                    terminal.error,
                    sequence
                ],
            )?;
            transaction.execute(
                "UPDATE workloads SET last_outcome = ?2
                 WHERE id = (SELECT workload_id FROM executions WHERE id = ?1)",
                params![id, terminal.state],
            )?;
        }
    }
    transaction.commit()?;
    Ok(true)
}

fn list_executions(
    connection: &Connection,
    workload_id: &str,
    limit: u32,
    before: Option<i64>,
) -> Result<Vec<StoredExecution>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT id, workload_id, state, supervisor_process_id, workload_process_id,
                attempt, started_unix_ms, ended_unix_ms, exit_code, error, snapshot,
                committed_sequence
         FROM executions
         WHERE workload_id = ?1 AND (?2 IS NULL OR started_unix_ms < ?2)
         ORDER BY started_unix_ms DESC LIMIT ?3",
    )?;
    let rows = statement.query_map(params![workload_id, before, limit], read_execution)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn get_execution(
    connection: &Connection,
    id: &str,
) -> Result<Option<StoredExecution>, StorageError> {
    connection
        .query_row(
            "SELECT id, workload_id, state, supervisor_process_id, workload_process_id,
                    attempt, started_unix_ms, ended_unix_ms, exit_code, error, snapshot,
                    committed_sequence FROM executions WHERE id = ?1",
            [id],
            read_execution,
        )
        .optional()
        .map_err(Into::into)
}

fn read_execution(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredExecution> {
    let supervisor_process_id = u32::try_from(row.get::<_, i64>(3)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    let workload_process_id = u32::try_from(row.get::<_, i64>(4)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    let attempt = u32::try_from(row.get::<_, i64>(5)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    let exit_code = row
        .get::<_, Option<i64>>(8)?
        .map(u32::try_from)
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?;
    Ok(StoredExecution {
        id: row.get(0)?,
        workload_id: row.get(1)?,
        state: row.get(2)?,
        supervisor_process_id,
        workload_process_id,
        attempt,
        started_unix_ms: row.get(6)?,
        ended_unix_ms: row.get(7)?,
        exit_code,
        error: row.get(9)?,
        snapshot: row.get(10)?,
        committed_sequence: u64::try_from(row.get::<_, i64>(11)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
    })
}

fn load_active_executions(connection: &Connection) -> Result<Vec<StoredExecution>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT id, workload_id, state, supervisor_process_id, workload_process_id,
                attempt, started_unix_ms, ended_unix_ms, exit_code, error, snapshot,
                committed_sequence FROM executions
         WHERE ended_unix_ms = 0 ORDER BY started_unix_ms",
    )?;
    let rows = statement.query_map([], read_execution)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}
