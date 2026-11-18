//! Manual recording: a thread that writes bus packets to a fragmented MP4 until stopped.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;
use crossbeam_channel::{unbounded, Sender};

use crate::mux::LiveWriter;
use crate::packets::{EncodedPacket, SendParams};

pub struct Recorder {
    pub path: PathBuf,
    tx: Option<Sender<Arc<EncodedPacket>>>,
    join: Option<JoinHandle<Result<()>>>,
}

impl Recorder {
    /// Creates the file and starts the writer thread. Feed it via [`Recorder::sender`].
    pub fn start(path: PathBuf, video: SendParams, audio: Option<SendParams>) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut writer = LiveWriter::create(&path, &video, audio.as_ref())?;
        let (tx, rx) = unbounded::<Arc<EncodedPacket>>();
        let join = std::thread::Builder::new().name("recorder".into()).spawn(move || {
            for p in rx.iter() {
                writer.write(&p)?;
            }
            writer.finish()
        })?;
        Ok(Self { path, tx: Some(tx), join: Some(join) })
    }

    pub fn sender(&self) -> Sender<Arc<EncodedPacket>> {
        self.tx.clone().expect("recorder already stopped")
    }

    /// Stops after writing everything queued. The caller must also drop every other sender
    /// (i.e. detach it from the bus) first.
    pub fn finish(mut self) -> Result<PathBuf> {
        self.tx.take();
        match self.join.take().map(|j| j.join()) {
            Some(Ok(res)) => res.map(|_| self.path.clone()),
            Some(Err(_)) => anyhow::bail!("recorder thread panicked"),
            None => Ok(self.path.clone()),
        }
    }
}
