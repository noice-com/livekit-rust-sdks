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

use super::TrackInner;
use crate::prelude::*;
use crate::rtc_engine::lk_runtime::LkRuntime;
use core::panic;
use libwebrtc::prelude::*;
use livekit_protocol as proto;
use std::fmt::Debug;
use std::sync::Arc;

#[derive(Clone)]
pub struct LocalAudioTrack {
    inner: Arc<TrackInner>,
    source: RtcAudioSource,
}

impl Debug for LocalAudioTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAudioTrack")
            .field("sid", &self.sid())
            .field("name", &self.name())
            .field("source", &self.source())
            .finish()
    }
}

impl LocalAudioTrack {
    pub(crate) fn new(name: String, rtc_track: RtcAudioTrack, source: RtcAudioSource) -> Self {
        Self {
            inner: Arc::new(super::new_inner(
                "TR_unknown".to_owned().try_into().unwrap(),
                name,
                TrackKind::Audio,
                MediaStreamTrack::Audio(rtc_track),
            )),
            source,
        }
    }

    pub fn create_audio_track(name: &str, source: RtcAudioSource) -> LocalAudioTrack {
        let rtc_track = match source.clone() {
            #[cfg(not(target_arch = "wasm32"))]
            RtcAudioSource::Native(native_source) => {
                use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
                LkRuntime::instance()
                    .pc_factory()
                    .create_audio_track(&libwebrtc::native::create_random_uuid(), native_source)
            }
            _ => panic!("unsupported audio source"),
        };
        Self::new(name.to_string(), rtc_track, source)
    }

    pub fn sid(&self) -> TrackSid {
        self.inner.info.read().sid.clone()
    }

    pub fn name(&self) -> String {
        self.inner.info.read().name.clone()
    }

    pub fn kind(&self) -> TrackKind {
        self.inner.info.read().kind
    }

    pub fn source(&self) -> TrackSource {
        self.inner.info.read().source
    }

    pub fn stream_state(&self) -> StreamState {
        self.inner.info.read().stream_state
    }

    pub fn enable(&self) {
        self.inner.rtc_track.set_enabled(true);
    }

    pub fn disable(&self) {
        self.inner.rtc_track.set_enabled(false);
    }

    pub fn is_muted(&self) -> bool {
        self.inner.info.read().muted
    }

    pub fn mute(&self) {
        super::set_muted(&self.inner, &Track::LocalAudio(self.clone()), true);
    }

    pub fn unmute(&self) {
        super::set_muted(&self.inner, &Track::LocalAudio(self.clone()), false);
    }

    pub fn rtc_track(&self) -> RtcAudioTrack {
        if let MediaStreamTrack::Audio(audio) = self.inner.rtc_track.clone() {
            return audio;
        }
        unreachable!();
    }

    pub fn rtc_source(&self) -> RtcAudioSource {
        self.source.clone()
    }

    pub fn is_remote(&self) -> bool {
        false
    }

    pub fn on_muted(&self, f: impl Fn(Track) + Send + 'static) {
        *self.inner.events.muted.lock() = Some(Box::new(f));
    }

    pub fn on_unmuted(&self, f: impl Fn(Track) + Send + 'static) {
        *self.inner.events.unmuted.lock() = Some(Box::new(f));
    }

    pub(crate) fn transceiver(&self) -> Option<RtpTransceiver> {
        self.inner.info.read().transceiver.clone()
    }

    pub(crate) fn set_transceiver(&self, transceiver: Option<RtpTransceiver>) {
        self.inner.info.write().transceiver = transceiver;
    }

    pub(crate) fn update_info(&self, info: proto::TrackInfo) {
        super::update_info(&self.inner, &Track::LocalAudio(self.clone()), info);
    }
}
