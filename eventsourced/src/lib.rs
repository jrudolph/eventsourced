#![cfg_attr(docsrs, feature(doc_cfg))]

//! Event sourced entities.
//!
//! EventSourced is inspired to a large degree by the amazing
//! [Akka Persistence](https://doc.akka.io/docs/akka/current/typed/index-persistence.html) library.
//! It provides a framework for implementing
//! [Event Sourcing](https://martinfowler.com/eaaDev/EventSourcing.html) and
//! [CQRS](https://www.martinfowler.com/bliki/CQRS.html).
//!
//! The [EvtLog] and [SnapshotStore] traits define a pluggable event log and a pluggable snapshot
//! store respectively. For [NATS](https://nats.io/) and [Postgres](https://www.postgresql.org/)
//! these are implemented in the respective crates.
//!
//! The [spawn](EventSourcedExt::spawn) function provides for creating event sourced entities,
//! identifiable by an ID, for some event log and  some snapshot store. Conversion of events and
//! snapshot state to and from bytes happens via the given [Binarize] implementation; for
//! [prost](https://github.com/tokio-rs/prost) and
//! [serde_json](https://github.com/serde-rs/json) these are already provided.
//!
//! Calling [spawn](EventSourcedExt::spawn) results in a cloneable [EntityRef] which can be used to
//! pass commands to the spawned entity by invoking [handle_cmd](EntityRef::handle_cmd). Commands
//! are handled by the command handler of the spawned entity. They can be rejected by returning an
//! error. Valid commands produce an event which gets persisted to the [EvtLog] and then applied to
//! the event handler of the respective entity. Snapshots can be taken to speed up future spawning.
//!
//! Events can be queried from the event log by ID or by entity type. These queries can be used to
//! build read side projections. There is early support for projections in the
//! `eventsourced-projection` crate.

pub mod binarize;

mod evt_log;
mod snapshot_store;

pub use evt_log::*;
pub use snapshot_store::*;

use crate::binarize::Binarize;
use error_ext::{BoxError, StdErrorExt};
use futures::{future::ok, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{
    error::Error as StdError,
    fmt::Debug,
    num::{NonZeroU64, NonZeroUsize},
};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    task,
};
use tracing::{debug, error, instrument};

/// Command and event handling for an event sourced entity.
pub trait EventSourced {
    /// Id type.
    type Id: Debug + Send + 'static;

    /// Command type.
    type Cmd: Debug + Send + Sync + 'static;

    /// Event type.
    type Evt: Debug + Send + Sync;

    /// State type.
    type State: Debug + Default + Send + Sync + 'static;

    /// Error type for rejected (a.k.a. invalid) commands.
    type Error: StdError + Send + Sync + 'static;

    const TYPE_NAME: &'static str;

    /// Command handler, returning the to be persisted event or an error.
    fn handle_cmd(
        id: &Self::Id,
        state: &Self::State,
        cmd: Self::Cmd,
    ) -> Result<Self::Evt, Self::Error>;

    /// Event handler.
    fn handle_evt(state: Self::State, evt: Self::Evt) -> Self::State;
}

/// Extension methods for types implementing [EventSourced].
pub trait EventSourcedExt: Sized {
    /// Spawns an event sourced entity and creates an [EntityRef] as a handle for it.
    ///
    /// First the given [SnapshotStore] is used to find and possibly load a snapshot. Then the
    /// [EvtLog] is used to find the last sequence number and then to load any remaining events.
    ///
    /// Commands can be passed to the spawned entity by invoking `handle_cmd` on the returned
    /// [EntityRef] which uses a buffered channel with the given size.
    ///
    /// Commands are handled by the command handler of the spawned entity. They can be rejected by
    /// returning an error. Valid commands produce an event which gets persisted to the [EvtLog]
    /// and then applied to the event handler of the respective entity. Snapshots can be taken
    /// to speed up future spawning.
    #[allow(async_fn_in_trait)]
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(evt_log, snapshot_store, binarize))]
    async fn spawn<L, S, B>(
        id: Self::Id,
        snapshot_after: Option<NonZeroU64>,
        cmd_buffer: NonZeroUsize,
        mut evt_log: L,
        mut snapshot_store: S,
        binarize: B,
    ) -> Result<EntityRef<Self>, SpawnError>
    where
        Self: EventSourced,
        L: EvtLog<Id = Self::Id>,
        S: SnapshotStore<Id = Self::Id>,
        B: Binarize<Self::Evt, Self::State>,
    {
        // Restore snapshot.
        let (snapshot_seq_no, state) = snapshot_store
            .load::<Self::State, _, _>(&id, |bytes| binarize.state_from_bytes(bytes))
            .await
            .map_err(|error| SpawnError::LoadSnapshot(error.into()))?
            .map(|Snapshot { seq_no, state }| {
                debug!(?id, seq_no, ?state, "restored snapshot");
                (seq_no, state)
            })
            .unzip();

        // Get and validate last sequence number.
        let mut last_seq_no = evt_log
            .last_seq_no::<Self>(&id)
            .await
            .map_err(|error| SpawnError::LastNonZeroU64(error.into()))?;
        if last_seq_no < snapshot_seq_no {
            return Err(SpawnError::InvalidLastSeqNo(last_seq_no, snapshot_seq_no));
        };

        // Replay latest events.
        let mut state = state.unwrap_or_default();
        if snapshot_seq_no < last_seq_no {
            let from_seq_no = snapshot_seq_no
                .map(|n| n.saturating_add(1))
                .unwrap_or(NonZeroU64::MIN);
            let to_seq_no = last_seq_no.unwrap(); // This is safe because of the above relation!
            debug!(?id, from_seq_no, to_seq_no, "replaying evts");

            let evts = evt_log
                .evts_by_id::<Self, _, _>(&id, from_seq_no, move |bytes| {
                    binarize.evt_from_bytes(bytes)
                })
                .await
                .map_err(|error| SpawnError::EvtsById(error.into()))?;

            state = evts
                .map_err(|error| SpawnError::NextEvt(error.into()))
                .try_take_while(|(seq_no, _)| ok(*seq_no <= to_seq_no))
                .try_fold(state, |state, (_, evt)| ok(Self::handle_evt(state, evt)))
                .await?;

            debug!(?id, ?state, "replayed evts");
        }

        // Spawn handler loop.
        let (cmd_in, mut cmd_out) = mpsc::channel::<(
            Self::Cmd,
            oneshot::Sender<Result<(), Self::Error>>,
        )>(cmd_buffer.get());
        task::spawn({
            let mut evt_count = 0u64;

            async move {
                while let Some((cmd, result_sender)) = cmd_out.recv().await {
                    debug!(?id, ?cmd, "handling command");

                    match Self::handle_cmd(&id, &state, cmd) {
                        Ok(evt) => {
                            debug!(?id, ?evt, "persisting event");

                            match evt_log
                                .persist::<Self, _, _>(&evt, &id, last_seq_no, &|evt| {
                                    binarize.evt_to_bytes(evt)
                                })
                                .await
                            {
                                Ok(seq_no) => {
                                    debug!(?id, ?evt, seq_no, "persited event");

                                    last_seq_no = Some(seq_no);
                                    state = Self::handle_evt(state, evt);

                                    evt_count += 1;
                                    if snapshot_after
                                        .map(|a| evt_count % a == 0)
                                        .unwrap_or_default()
                                    {
                                        debug!(?id, seq_no, evt_count, "saving snapshot");

                                        if let Err(error) = snapshot_store
                                            .save(&id, seq_no, &state, &|state| {
                                                binarize.state_to_bytes(state)
                                            })
                                            .await
                                        {
                                            error!(
                                                error = error.as_chain(),
                                                ?id,
                                                "cannot save snapshot"
                                            );
                                        };
                                    }

                                    if result_sender.send(Ok(())).is_err() {
                                        error!(?id, "cannot send command handler OK");
                                    };
                                }

                                Err(error) => {
                                    error!(error = error.as_chain(), ?id, "cannot persist event");
                                    // This is fatal, we must terminate the entity!
                                    break;
                                }
                            }
                        }

                        Err(error) => {
                            if result_sender.send(Err(error)).is_err() {
                                error!(?id, "cannot send command handler error");
                            }
                        }
                    };
                }

                debug!(?id, "entity terminated");
            }
        });

        Ok(EntityRef { cmd_in })
    }
}

/// Error from spawning an event sourced entity.
#[derive(Debug, Error)]
pub enum SpawnError {
    /// A snapshot cannot be loaded from the snapshot store.
    #[error("cannot load snapshot from snapshot store")]
    LoadSnapshot(#[source] BoxError),

    /// The last sequence number is less than the snapshot sequence number.
    #[error("last sequence number {0:?} less than snapshot sequence number {0:?}")]
    InvalidLastSeqNo(Option<NonZeroU64>, Option<NonZeroU64>),

    /// The last seqence number cannot be obtained from the event log.
    #[error("cannot get last seqence number from event log")]
    LastNonZeroU64(#[source] BoxError),

    /// Events by ID cannot be obtained from the event log.
    #[error("cannot get events by ID from event log")]
    EvtsById(#[source] BoxError),

    /// The next event cannot be obtained from the event log.
    #[error("cannot get next event from event log")]
    NextEvt(#[source] BoxError),
}

impl<E> EventSourcedExt for E where E: EventSourced {}

/// A handle for a spawned event sourced entity which can be used to invoke its command handler.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct EntityRef<E>
where
    E: EventSourced,
{
    cmd_in: mpsc::Sender<(E::Cmd, oneshot::Sender<Result<(), E::Error>>)>,
}

impl<E> EntityRef<E>
where
    E: EventSourced,
{
    /// Invoke the command handler of the entity.
    #[instrument(skip(self))]
    pub async fn handle_cmd(&self, cmd: E::Cmd) -> Result<Result<(), E::Error>, HandleCmdError> {
        let (result_in, result_out) = oneshot::channel();
        self.cmd_in
            .send((cmd, result_in))
            .await
            .map_err(|_| HandleCmdError("cannot send command".to_string()))?;
        result_out
            .await
            .map_err(|_| HandleCmdError("cannot receive command handler result".to_string()))
    }
}

/// A command cannot be sent from an [EntityRef] to its entity or the result cannot be received
/// from its entity.
#[derive(Debug, Error, Serialize, Deserialize)]
#[error("{0}")]
pub struct HandleCmdError(String);

#[cfg(all(test, feature = "serde_json"))]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::{stream, Stream, StreamExt};
    use std::{convert::Infallible, iter};
    use tracing_test::traced_test;
    use uuid::Uuid;

    #[derive(Debug)]
    struct Simple;

    impl EventSourced for Simple {
        type Id = Uuid;
        type Cmd = ();
        type Evt = ();
        type State = u64;
        type Error = Infallible;

        const TYPE_NAME: &'static str = "simple";

        fn handle_cmd(
            _id: &Self::Id,
            _state: &Self::State,
            _cmd: Self::Cmd,
        ) -> Result<Self::Evt, Self::Error> {
            Ok(())
        }

        fn handle_evt(mut state: Self::State, _evt: Self::Evt) -> Self::State {
            state += 1;
            state
        }
    }

    #[derive(Debug, Clone)]
    struct TestEvtLog;

    impl EvtLog for TestEvtLog {
        type Id = Uuid;
        type Error = TestEvtLogError;

        async fn persist<E, ToBytes, ToBytesError>(
            &mut self,
            _evt: &E::Evt,
            _id: &Self::Id,
            last_seq_no: Option<NonZeroU64>,
            _to_bytes: &ToBytes,
        ) -> Result<NonZeroU64, Self::Error>
        where
            E: EventSourced,
            ToBytes: Fn(&E::Evt) -> Result<Bytes, ToBytesError> + Sync,
            ToBytesError: StdError + Send + Sync + 'static,
        {
            let seq_no = last_seq_no.unwrap_or(NonZeroU64::MIN);
            Ok(seq_no)
        }

        async fn last_seq_no<E>(
            &self,
            _entity_id: &Self::Id,
        ) -> Result<Option<NonZeroU64>, Self::Error>
        where
            E: EventSourced,
        {
            Ok(Some(42.try_into().unwrap()))
        }

        async fn evts_by_id<E, FromBytes, FromBytesError>(
            &self,
            _id: &Self::Id,
            seq_no: NonZeroU64,
            evt_from_bytes: FromBytes,
        ) -> Result<impl Stream<Item = Result<(NonZeroU64, E::Evt), Self::Error>> + Send, Self::Error>
        where
            E: EventSourced,
            FromBytes: Fn(Bytes) -> Result<E::Evt, FromBytesError> + Copy + Send + Sync,
            FromBytesError: StdError + Send + Sync + 'static,
        {
            let successors = iter::successors(Some(seq_no), |n| n.checked_add(1));
            let evts = stream::iter(successors).map(move |n| {
                let evt = evt_from_bytes(serde_json::to_vec(&()).unwrap().into()).unwrap();
                Ok((n, evt))
            });

            Ok(evts)
        }

        async fn evts_by_type<E, FromBytes, FromBytesError>(
            &self,
            _seq_no: NonZeroU64,
            _evt_from_bytes: FromBytes,
        ) -> Result<impl Stream<Item = Result<(NonZeroU64, E::Evt), Self::Error>> + Send, Self::Error>
        where
            E: EventSourced,
            FromBytes: Fn(Bytes) -> Result<E::Evt, FromBytesError> + Copy + Send,
            FromBytesError: StdError + Send + Sync + 'static,
        {
            Ok(stream::empty())
        }
    }

    #[derive(Debug, Error)]
    #[error("TestEvtLogError")]
    struct TestEvtLogError(#[source] BoxError);

    #[derive(Debug, Clone)]
    struct TestSnapshotStore;

    impl SnapshotStore for TestSnapshotStore {
        type Id = Uuid;
        type Error = TestSnapshotStoreError;

        async fn save<S, ToBytes, ToBytesError>(
            &mut self,
            _id: &Self::Id,
            _seq_no: NonZeroU64,
            _state: &S,
            _state_to_bytes: &ToBytes,
        ) -> Result<(), Self::Error>
        where
            S: Send,
            ToBytes: Fn(&S) -> Result<Bytes, ToBytesError> + Sync,
            ToBytesError: StdError,
        {
            Ok(())
        }

        async fn load<S, FromBytes, FromBytesError>(
            &self,
            _id: &Self::Id,
            state_from_bytes: FromBytes,
        ) -> Result<Option<Snapshot<S>>, Self::Error>
        where
            FromBytes: Fn(Bytes) -> Result<S, FromBytesError>,
            FromBytesError: StdError,
        {
            let bytes = serde_json::to_vec(&21).unwrap();
            let state = state_from_bytes(bytes.into()).unwrap();
            Ok(Some(Snapshot {
                seq_no: 21.try_into().unwrap(),
                state,
            }))
        }
    }

    #[derive(Debug, Error)]
    #[error("TestSnapshotStoreError")]
    struct TestSnapshotStoreError;

    #[tokio::test]
    #[traced_test]
    async fn test_spawn_handle_cmd() -> Result<(), BoxError> {
        let evt_log = TestEvtLog;
        let snapshot_store = TestSnapshotStore;

        let entity = Simple::spawn(
            Uuid::from_u128(1),
            None,
            unsafe { NonZeroUsize::new_unchecked(1) },
            evt_log,
            snapshot_store,
            binarize::serde_json::SerdeJsonBinarize,
        )
        .await?;

        entity.handle_cmd(()).await??;

        assert!(logs_contain("state=42"));

        Ok(())
    }
}
