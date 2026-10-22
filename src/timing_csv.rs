//! Shared CSV format for matching publisher and subscriber timing events.

use anyhow::Result;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

/// Buffered per-frame timing event log.
pub(crate) struct TimingCsv(BufWriter<File>);

impl TimingCsv {
    /// Create a timing log; existing files are rejected to preserve previous runs.
    pub(crate) fn create(path: &Path) -> Result<Self> {
        let file = File::options().write(true).create_new(true).open(path)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "track_sid,stage,timestamp_us,capture_timestamp_us,frame_id,measured")?;
        Ok(Self(writer))
    }

    /// Record a stage using its native observation timestamp, not log-write time.
    pub(crate) fn record(
        &mut self,
        sid: &str,
        stage: &str,
        timestamp: u64,
        capture: u64,
        frame_id: Option<u32>,
        measured: bool,
    ) -> Result<()> {
        let id = frame_id.map(|id| id.to_string()).unwrap_or_default();
        writeln!(self.0, "{sid},{stage},{timestamp},{capture},{id},{measured}")?;
        Ok(())
    }

    /// Flush buffered events before shutdown.
    pub(crate) fn flush(&mut self) -> Result<()> {
        self.0.flush()?;
        Ok(())
    }
}
