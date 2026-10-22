//! Owned publisher timing recorder with deterministic shutdown.

use crate::timing_csv::TimingCsv;
use anyhow::Result;
use futures::StreamExt;
use livekit::prelude::LocalVideoTrack;
use std::path::Path;
use tokio::{sync::oneshot, task::JoinHandle};

/// Owns the task and cancellation signal for one timing log.
pub(super) struct Recorder {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
}

impl Recorder {
    /// Start instrumentation before publication so the first frames are observable.
    pub(super) fn start(track: &LocalVideoTrack, path: &Path) -> Result<Self> {
        let mut csv = TimingCsv::create(path)?;
        let mut events = track.publish_timing_events();
        let track = track.clone();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    event = events.next() => match event {
                        Some(event) => csv.record(track.sid().as_str(), &format!("{:?}", event.stage), event.timestamp_us, event.capture_timestamp_us, event.frame_id, true)?,
                        None => break,
                    }
                }
            }
            csv.flush()
        });
        Ok(Self { stop: Some(stop), task: Some(task) })
    }

    /// Stop, flush, and join the recorder after capture ends.
    pub(super) async fn finish(mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Error paths must not leave a task retaining the local track.
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
