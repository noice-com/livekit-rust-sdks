// Copyright 2023 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::id::ParticipantSid;
use crate::options::TrackPublishOptions;
use crate::prelude::LocalTrack;
use crate::room::DisconnectReason;
use crate::rtc_engine::lk_runtime::LkRuntime;
use crate::rtc_engine::rtc_session::{RtcSession, SessionEvent, SessionEvents};
use crate::DataPacketKind;
use libwebrtc::prelude::*;
use livekit_api::signal_client::{SignalError, SignalOptions};
use livekit_protocol as proto;
use parking_lot::RwLock;
use parking_lot::RwLockReadGuard;
use std::borrow::Cow;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::sync::{RwLock as AsyncRwLock, RwLockReadGuard as AsyncRwLockReadGuard};
use tokio::task::JoinHandle;
use tokio::time::{interval, Interval};

pub mod lk_runtime;
mod peer_transport;
mod rtc_events;
mod rtc_session;

pub(crate) type EngineEmitter = mpsc::UnboundedSender<EngineEvent>;
pub(crate) type EngineEvents = mpsc::UnboundedReceiver<EngineEvent>;
pub(crate) type EngineResult<T> = Result<T, EngineError>;

pub const RECONNECT_ATTEMPTS: u32 = 10;
pub const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SimulateScenario {
    SignalReconnect,
    Speaker,
    NodeFailure,
    ServerLeave,
    Migration,
    ForceTcp,
    ForceTls,
}

#[derive(Error, Debug)]
pub enum EngineError {
    #[error("signal failure: {0}")]
    Signal(#[from] SignalError),
    #[error("internal webrtc failure")]
    Rtc(#[from] RtcError),
    #[error("connection error: {0}")]
    Connection(Cow<'static, str>), // Connectivity issues (Failed to connect/reconnect)
    #[error("internal error: {0}")]
    Internal(Cow<'static, str>), // Unexpected error, generally we can't recover
}

#[derive(Default, Debug, Clone)]
pub struct EngineOptions {
    pub rtc_config: RtcConfiguration,
    pub signal_options: SignalOptions,
}

#[derive(Debug)]
pub enum EngineEvent {
    ParticipantUpdate {
        updates: Vec<proto::ParticipantInfo>,
    },
    MediaTrack {
        track: MediaStreamTrack,
        stream: MediaStream,
        transceiver: RtpTransceiver,
    },
    Data {
        participant_sid: Option<ParticipantSid>,
        payload: Vec<u8>,
        kind: DataPacketKind,
    },
    SpeakersChanged {
        speakers: Vec<proto::SpeakerInfo>,
    },
    ConnectionQuality {
        updates: Vec<proto::ConnectionQualityInfo>,
    },
    RoomUpdate {
        room: proto::Room,
    },
    /// The following events are used to notify the room about the reconnection state
    /// Since the room needs to also sync state in a good timing with the server.
    /// We synchronize the state with a one-shot channel.
    Resuming(oneshot::Sender<()>),
    Resumed(oneshot::Sender<()>),
    SignalResumed {
        reconnect_response: proto::ReconnectResponse,
        tx: oneshot::Sender<()>,
    },
    Restarting(oneshot::Sender<()>),
    Restarted(oneshot::Sender<()>),
    SignalRestarted {
        join_response: proto::JoinResponse,
        tx: oneshot::Sender<()>,
    },
    Disconnected {
        reason: DisconnectReason,
    },
}

/// Represents a running RtcSession with the ability to close the session
/// and the engine_task
#[derive(Debug)]
struct EngineHandle {
    session: Arc<RtcSession>,
    closed: bool,
    reconnecting: bool,

    // If full_reconnect is true, the next attempt will not try to resume
    // and will instead do a full reconnect
    full_reconnect: bool,
    engine_task: Option<(JoinHandle<()>, oneshot::Sender<()>)>,
}

struct EngineInner {
    // Keep a strong reference to LkRuntime to avoid creating a new RtcRuntime or PeerConnection factory accross multiple Rtc sessions
    lk_runtime: Arc<LkRuntime>,
    engine_tx: EngineEmitter,
    options: EngineOptions,

    close_notifier: Arc<Notify>,
    running_handle: RwLock<EngineHandle>,

    // The lock is write guarded for the whole reconnection time.
    // We can simply wait for reconnection by trying to acquire a read lock.
    // (This also prevents new reconnection to happens if a read guard is still held)
    reconnecting_lock: AsyncRwLock<()>,
    reconnecting_interval: AsyncMutex<Interval>,
}

pub struct RtcEngine {
    inner: Arc<EngineInner>,
}

impl Debug for RtcEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcEngine").finish()
    }
}

impl RtcEngine {
    pub async fn connect(
        url: &str,
        token: &str,
        options: EngineOptions,
    ) -> EngineResult<(Self, proto::JoinResponse, EngineEvents)> {
        let (inner, join_response, engine_events) =
            EngineInner::connect(url, token, options).await?;
        Ok((Self { inner }, join_response, engine_events))
    }

    pub async fn close(&self) {
        self.inner.close(DisconnectReason::ClientInitiated).await
    }

    pub async fn publish_data(
        &self,
        data: &proto::DataPacket,
        kind: DataPacketKind,
    ) -> EngineResult<()> {
        let (session, _r_lock) = {
            let (handle, _r_lock) = self.inner.wait_reconnection().await?;
            (handle.session.clone(), _r_lock)
        };

        session.publish_data(data, kind).await
    }

    pub async fn simulate_scenario(&self, scenario: SimulateScenario) -> EngineResult<()> {
        let (session, _r_lock) = {
            let (handle, _r_lock) = self.inner.wait_reconnection().await?;
            (handle.session.clone(), _r_lock)
        };
        session.simulate_scenario(scenario).await
    }

    pub async fn add_track(&self, req: proto::AddTrackRequest) -> EngineResult<proto::TrackInfo> {
        let (session, _r_lock) = {
            let (handle, _r_lock) = self.inner.wait_reconnection().await?;
            (handle.session.clone(), _r_lock)
        };
        session.add_track(req).await
    }

    pub async fn remove_track(&self, sender: RtpSender) -> EngineResult<()> {
        // We don't need to wait for the reconnection
        let session = self.inner.running_handle.read().session.clone();
        session.remove_track(sender).await // TODO(theomonnom): Ignore errors where this
                                           // RtpSender is bound to the old session. (Can
                                           // happen on bad timing and it is safe to ignore)
    }

    pub async fn create_sender(
        &self,
        track: LocalTrack,
        options: TrackPublishOptions,
        encodings: Vec<RtpEncodingParameters>,
    ) -> EngineResult<RtpTransceiver> {
        // When creating a new RtpSender, make sure we're always using the latest session
        let (session, _r_lock) = {
            let (handle, _r_lock) = self.inner.wait_reconnection().await?;
            (handle.session.clone(), _r_lock)
        };

        session.create_sender(track, options, encodings).await
    }

    pub fn publisher_negotiation_needed(&self) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            if let Ok((handle, _)) = inner.wait_reconnection().await {
                handle.session.publisher_negotiation_needed()
            }
        });
    }

    pub async fn send_request(&self, msg: proto::signal_request::Message) {
        // Getting the current session is OK to do without waiting for reconnection
        // SignalClient will attempt to queue the message if the session is not connected
        // Also on full_reconnect, every message is OK to ignore (Since this is another RtcSession)
        let session = self.inner.running_handle.read().session.clone();
        session.signal_client().send(msg).await // Returns () and automatically queues the message
                                                // on fail
    }

    pub fn session(&self) -> Arc<RtcSession> {
        self.inner.running_handle.read().session.clone()
    }
}

impl EngineInner {
    async fn connect(
        url: &str,
        token: &str,
        options: EngineOptions,
    ) -> EngineResult<(Arc<Self>, proto::JoinResponse, EngineEvents)> {
        let lk_runtime = LkRuntime::instance();

        let (session, join_response, session_events) =
            RtcSession::connect(url, token, options.clone()).await?;
        let (engine_tx, engine_rx) = mpsc::unbounded_channel();

        session.wait_pc_connection().await?;

        let inner = Arc::new(Self {
            lk_runtime,
            engine_tx,
            close_notifier: Arc::new(Notify::new()),
            running_handle: RwLock::new(EngineHandle {
                session: Arc::new(session),
                closed: false,
                reconnecting: false,
                full_reconnect: false,
                engine_task: None,
            }),
            options,
            reconnecting_lock: AsyncRwLock::default(),
            reconnecting_interval: AsyncMutex::new(interval(RECONNECT_INTERVAL)),
        });

        // Start initial tasks
        let (close_tx, close_rx) = oneshot::channel();
        let session_task = tokio::spawn(Self::engine_task(inner.clone(), session_events, close_rx));
        inner.running_handle.write().engine_task = Some((session_task, close_tx));

        Ok((inner, join_response, engine_rx))
    }

    async fn engine_task(
        self: Arc<Self>,
        mut session_events: SessionEvents,
        mut close_receiver: oneshot::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                Some(event) = session_events.recv() => {
                    let debug = format!("{:?}", event);
                    let inner = self.clone();
                    let (tx, rx) = oneshot::channel();
                    let task = tokio::spawn(async move {
                        if let Err(err) = inner.on_session_event(event).await {
                            log::error!("failed to handle session event: {:?}", err);
                        }
                        let _ = tx.send(());
                    });

                    // Monitor sync/async blockings
                    tokio::select! {
                        _ = rx => {},
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {
                            log::error!("session_event is taking too much time: {}", debug);
                        }
                    }

                    task.await.unwrap();
                },
                 _ = &mut close_receiver => {
                    break;
                }
            }
        }

        log::debug!("engine task closed");
    }

    async fn on_session_event(self: &Arc<Self>, event: SessionEvent) -> EngineResult<()> {
        match event {
            SessionEvent::Close {
                source,
                reason,
                can_reconnect,
                retry_now,
                full_reconnect,
            } => {
                log::info!("received session close: {}, {:?}", source, reason);
                if can_reconnect {
                    self.reconnection_needed(retry_now, full_reconnect);
                } else {
                    // Spawning a new task because the close function wait for the engine_task to
                    // finish. (So it doesn't make sense to await it here)
                    tokio::spawn({
                        let inner = self.clone();
                        async move {
                            inner.close(reason).await;
                        }
                    });
                }
            }
            SessionEvent::Data {
                participant_sid,
                payload,
                kind,
            } => {
                let _ = self.engine_tx.send(EngineEvent::Data {
                    participant_sid,
                    payload,
                    kind,
                });
            }
            SessionEvent::MediaTrack {
                track,
                stream,
                transceiver,
            } => {
                let _ = self.engine_tx.send(EngineEvent::MediaTrack {
                    track,
                    stream,
                    transceiver,
                });
            }
            SessionEvent::ParticipantUpdate { updates } => {
                let _ = self
                    .engine_tx
                    .send(EngineEvent::ParticipantUpdate { updates });
            }
            SessionEvent::SpeakersChanged { speakers } => {
                let _ = self
                    .engine_tx
                    .send(EngineEvent::SpeakersChanged { speakers });
            }
            SessionEvent::ConnectionQuality { updates } => {
                let _ = self
                    .engine_tx
                    .send(EngineEvent::ConnectionQuality { updates });
            }
            SessionEvent::RoomUpdate { room } => {
                let _ = self.engine_tx.send(EngineEvent::RoomUpdate { room });
            }
        }
        Ok(())
    }

    /// Close the engine
    /// the RtcSession is not removed so we can still access stats for e.g
    async fn close(&self, reason: DisconnectReason) {
        let (session, engine_task) = {
            let mut running_handle = self.running_handle.write();
            running_handle.closed = true;

            let session = running_handle.session.clone();
            let engine_task = running_handle.engine_task.take();
            (session, engine_task)
        };

        if let Some((engine_task, close_tx)) = engine_task {
            session.close().await;
            let _ = close_tx.send(());
            let _ = engine_task.await;
        }
        let _ = self.engine_tx.send(EngineEvent::Disconnected { reason });
    }

    /// When waiting for reconnection, it ensures we're always using the latest session.
    async fn wait_reconnection(
        &self,
    ) -> EngineResult<(RwLockReadGuard<EngineHandle>, AsyncRwLockReadGuard<()>)> {
        let r_lock = self.reconnecting_lock.read().await;
        let running_handle = self.running_handle.read();

        if running_handle.closed {
            // Reconnection may have failed
            // TODO(theomonnom): More precise error?
            return Err(EngineError::Connection("engine is closed".into()));
        }

        Ok((running_handle, r_lock))
    }

    /// Start the reconnect task if not already started
    /// Ask to retry directly if `retry_now` is true
    /// Ask for a full reconnect if `full_reconnect` is true
    fn reconnection_needed(self: &Arc<Self>, retry_now: bool, full_reconnect: bool) {
        let mut running_handle = self.running_handle.write();
        if running_handle.reconnecting {
            // If we're already reconnecting just update the interval to restart a new attempt
            // ASAP

            running_handle.full_reconnect = full_reconnect;

            if retry_now {
                let inner = self.clone();
                tokio::spawn(async move {
                    inner.reconnecting_interval.lock().await.reset();
                });
            }

            return;
        }

        running_handle.reconnecting = true;
        running_handle.full_reconnect = full_reconnect;

        tokio::spawn({
            let inner = self.clone();
            async move {
                // Hold the reconnection lock for the whole reconnection time
                let _r_lock = inner.reconnecting_lock.write().await;
                // The close function can send a signal to cancel the reconnection

                let close_notifier = inner.close_notifier.clone();
                let close_receiver = close_notifier.notified();
                tokio::pin!(close_receiver);

                tokio::select! {
                    _ = &mut close_receiver => {
                        log::info!("reconnection cancelled");
                        return;
                    }
                    res = inner.reconnect_task() => {
                        if res.is_err() {
                            log::error!("failed to reconnect");
                            inner.close(DisconnectReason::UnknownReason).await;
                        } else {
                            log::info!("RtcEngine successfully recovered")
                        }
                    }
                }

                let mut running_handle = inner.running_handle.write();
                running_handle.reconnecting = false;

                // r_lock is now dropped
            }
        });
    }

    /// Runned every time the PeerConnection or the SignalClient is closed
    /// We first try to resume the connection, if it fails, we start a full reconnect.
    /// NOTE: The reconnect_task must be canncellation safe
    async fn reconnect_task(self: &Arc<Self>) -> EngineResult<()> {
        // Get the latest connection info from the signal_client (including the refreshed token because the initial join token may have expired)
        let (url, token) = {
            let running_handle = self.running_handle.read();
            let signal_client = running_handle.session.signal_client();
            (
                signal_client.url(),
                signal_client.token(), // Refreshed token
            )
        };

        for i in 0..RECONNECT_ATTEMPTS {
            let (is_closed, full_reconnect) = {
                let running_handle = self.running_handle.read();
                (running_handle.closed, running_handle.full_reconnect)
            };

            if is_closed {
                return Err(EngineError::Connection(
                    "attempt canncelled, engine is closed".into(),
                ));
            }

            if full_reconnect {
                if i == 0 {
                    let (tx, rx) = oneshot::channel();
                    let _ = self.engine_tx.send(EngineEvent::Restarting(tx));
                    let _ = rx.await;
                }

                log::error!("restarting connection... attempt: {}", i);
                if let Err(err) = self
                    .try_restart_connection(&url, &token, self.options.clone())
                    .await
                {
                    log::error!("restarting connection failed: {}", err);
                } else {
                    let (tx, rx) = oneshot::channel();
                    let _ = self.engine_tx.send(EngineEvent::Restarted(tx));
                    let _ = rx.await;
                    return Ok(());
                }
            } else {
                if i == 0 {
                    let (tx, rx) = oneshot::channel();
                    let _ = self.engine_tx.send(EngineEvent::Resuming(tx));
                    let _ = rx.await;
                }

                log::error!("resuming connection... attempt: {}", i);
                if let Err(err) = self.try_resume_connection().await {
                    log::error!("resuming connection failed: {}", err);
                    if let EngineError::Signal(_) = err {
                        let mut running_handle = self.running_handle.write();
                        running_handle.full_reconnect = true;
                    }
                } else {
                    let (tx, rx) = oneshot::channel();
                    let _ = self.engine_tx.send(EngineEvent::Resumed(tx));
                    let _ = rx.await;
                    return Ok(());
                }
            }

            self.reconnecting_interval.lock().await.tick().await;
        }

        Err(EngineError::Connection(
            format!("failed to reconnect after {}", RECONNECT_ATTEMPTS).into(),
        ))
    }

    /// Try to recover the connection by doing a full reconnect.
    /// It recreates a new RtcSession (new peer connection, new signal client, new data channels, etc...)
    async fn try_restart_connection(
        self: &Arc<Self>,
        url: &str,
        token: &str,
        options: EngineOptions,
    ) -> EngineResult<()> {
        // Close the current RtcSession and the current tasks
        let (session, engine_task) = {
            let mut running_handle = self.running_handle.write();
            let session = running_handle.session.clone();
            let engine_task = running_handle.engine_task.take();
            (session, engine_task)
        };

        if let Some((engine_task, close_tx)) = engine_task {
            session.close().await;
            let _ = close_tx.send(());
            let _ = engine_task.await;
        }

        let (new_session, join_response, session_events) =
            RtcSession::connect(url, token, options).await?;

        // On SignalRestarted, the room will try to unpublish the local tracks
        // NOTE: Doing operations that use rtc_session will not use the new one
        let (tx, rx) = oneshot::channel();
        let _ = self
            .engine_tx
            .send(EngineEvent::SignalRestarted { join_response, tx });
        let _ = rx.await;

        new_session.wait_pc_connection().await?;

        // Only replace the current session if the new one succeed
        // This is important so we can still use the old session if the new one failed
        // (for example, this is important if we still want to get the stats of the old session)
        // This has the drawback to not being able to use the new session on the SignalRestarted
        // event.
        let mut handle = self.running_handle.write();
        handle.session = Arc::new(new_session);

        let (close_tx, close_rx) = oneshot::channel();
        let task = tokio::spawn(self.clone().engine_task(session_events, close_rx));
        handle.engine_task = Some((task, close_tx));

        Ok(())
    }

    /// Try to restart the current session
    async fn try_resume_connection(&self) -> EngineResult<()> {
        let session = self.running_handle.read().session.clone();
        let reconnect_response = session.restart().await?;

        let (tx, rx) = oneshot::channel();
        let _ = self.engine_tx.send(EngineEvent::SignalResumed {
            reconnect_response,
            tx,
        });

        // With SignalResumed, the room will send a SyncState message to the server
        let _ = rx.await;

        // The publisher offer must be sent AFTER the SyncState message
        session.restart_publisher().await?;
        session.wait_pc_connection().await
    }
}
