//! Encoded packets and the bus that fans them out to the replay buffer and the recorder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::Sender;
use parking_lot::Mutex;

use crate::replay::ReplayBuffer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
}

/// A compressed packet. All timestamps are microseconds on the session clock.
#[derive(Debug)]
pub struct EncodedPacket {
    pub kind: StreamKind,
    pub data: Vec<u8>,
    pub pts_us: i64,
    pub dts_us: i64,
    pub duration_us: i64,
    pub key: bool,
}

/// Codec parameters (incl. extradata/global headers) needed to mux each stream.
#[derive(Clone)]
pub struct SendParams(pub ffmpeg_next::codec::Parameters);
// SAFETY: Parameters is a heap-owned AVCodecParameters with no thread affinity; we only ever
// read it (clone) from other threads, always behind a Mutex.
unsafe impl Send for SendParams {}
unsafe impl Sync for SendParams {}

#[derive(Default)]
pub struct StreamParams {
    pub video: Mutex<Option<SendParams>>,
    pub audio: Mutex<Option<SendParams>>,
}

impl StreamParams {
    pub fn snapshot(&self) -> (Option<SendParams>, Option<SendParams>) {
        (self.video.lock().clone(), self.audio.lock().clone())
    }

    pub fn clear(&self) {
        *self.video.lock() = None;
        *self.audio.lock() = None;
    }
}

pub struct PacketBus {
    pub replay: Mutex<ReplayBuffer>,
    replay_enabled: AtomicBool,
    recorder: Mutex<Option<Sender<Arc<EncodedPacket>>>>,
    /// The live stream takes the audio packets (it encodes its own video).
    streamer: Mutex<Option<Sender<Arc<EncodedPacket>>>>,
}

impl PacketBus {
    pub fn new(replay_seconds: u32, replay_enabled: bool) -> Self {
        Self {
            replay: Mutex::new(ReplayBuffer::new(replay_seconds as i64 * 1_000_000)),
            replay_enabled: AtomicBool::new(replay_enabled),
            recorder: Mutex::new(None),
            streamer: Mutex::new(None),
        }
    }

    pub fn publish(&self, pkt: EncodedPacket) {
        let pkt = Arc::new(pkt);
        if self.replay_enabled.load(Ordering::Relaxed) {
            self.replay.lock().push(pkt.clone());
        }
        if pkt.kind == StreamKind::Audio {
            if let Some(tx) = self.streamer.lock().as_ref() {
                let _ = tx.try_send(pkt.clone());
            }
        }
        if let Some(tx) = self.recorder.lock().as_ref() {
            let _ = tx.send(pkt);
        }
    }

    pub fn set_streamer(&self, tx: Option<Sender<Arc<EncodedPacket>>>) {
        *self.streamer.lock() = tx;
    }

    pub fn set_recorder(&self, tx: Option<Sender<Arc<EncodedPacket>>>) {
        *self.recorder.lock() = tx;
    }

    pub fn clear(&self) {
        self.replay.lock().clear();
    }
}
