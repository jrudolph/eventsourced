//! [EvtLog](eventsourced::EvtLog) and [SnapshotStore](eventsourced::SnapshotStore) implementations
//! based upon [PostgreSQL](https://www.postgresql.org/).

mod evt_log;
mod snapshot_store;

pub use evt_log::{Config as PostgresEvtLogConfig, PostgresEvtLog};
pub use snapshot_store::{Config as PostgresSnapshotStoreConfig, PostgresSnapshotStore};

use bb8_postgres::{
    bb8::{Pool, PooledConnection},
    PostgresConnectionManager,
};
use eventsourced::ZeroSeqNoError;
use thiserror::Error;

type CnnPool<T> = Pool<PostgresConnectionManager<T>>;

type Cnn<'a, T> = PooledConnection<'a, PostgresConnectionManager<T>>;

/// Errors from the [PostgresEvtLog] or [PostgresSnapshotStore].
#[derive(Debug, Error)]
pub enum Error {
    /// Cannot create connection manager.
    #[error("cannot create connection manager")]
    ConnectionManager(#[source] tokio_postgres::Error),

    /// Cannot create connection pool.
    #[error("cannot create connection pool")]
    ConnectionPool(#[source] tokio_postgres::Error),

    /// Cannot get connection from pool.
    #[error("cannot get connection from pool")]
    GetConnection(#[source] bb8_postgres::bb8::RunError<tokio_postgres::Error>),

    /// Cannot execute query.
    #[error("cannot execute query")]
    ExecuteQuery(#[source] tokio_postgres::Error),

    /// Cannot convert an event to bytes.
    #[error("cannot convert an event to bytes")]
    ToBytes(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Cannot convert bytes to an event.
    #[error("cannot convert bytes to an event")]
    FromBytes(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Cannot get next row.
    #[error("cannot get next row")]
    NextRow(#[source] tokio_postgres::Error),

    /// Cannot get column as Uuid.
    #[error("cannot get column as Uuid")]
    ColumnAsUuid(#[source] tokio_postgres::Error),

    /// Invalid sequence number.
    #[error("invalid sequence number")]
    InvalidSeqNo(#[source] ZeroSeqNoError),
}
