//! Window-free video consumption and capture-to-decoded latency measurements.

use crate::{subscriber::current_timestamp_us, timing_csv::TimingCsv};
use anyhow::{bail, Result};
use futures::{Stream, StreamExt};
use livekit::{
    prelude::*, track::SubscribeTimingEventStream, webrtc::video_stream::native::NativeVideoStream,
};
use std::{
    collections::VecDeque,
    future::pending,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedReceiver;

/// Settings for a headless subscriber run.
pub(super) struct Options {
    pub participant: Option<String>,
    pub duration: Option<Duration>,
    pub warmup: Duration,
    pub startup_timeout: Duration,
    pub timing_output: Option<PathBuf>,
}

/// Keep memory bounded even for an unlimited run; percentiles use recent samples.
#[derive(Default)]
struct Measurements {
    frames: u64,
    missing_timestamp: u64,
    future_timestamp: u64,
    frame_id_gaps: u64,
    reordered_ids: u64,
    last_id: Option<u32>,
    latency_us: VecDeque<u64>,
}

impl Measurements {
    /// Record a decoded frame without treating clock errors as zero latency.
    fn record(&mut self, now: u64, capture: Option<u64>, id: Option<u32>) {
        self.frames += 1;
        if let Some(id) = id {
            if let Some(last) = self.last_id {
                let advance = id.wrapping_sub(last);
                if advance > 0 && advance < u32::MAX / 2 {
                    self.frame_id_gaps += u64::from(advance - 1);
                    self.last_id = Some(id);
                } else {
                    self.reordered_ids += 1;
                }
            } else {
                self.last_id = Some(id);
            }
        }
        match capture.filter(|capture| *capture != 0) {
            None => self.missing_timestamp += 1,
            Some(capture) => match now.checked_sub(capture) {
                None => self.future_timestamp += 1,
                Some(latency) => {
                    if self.latency_us.len() == 100_000 {
                        self.latency_us.pop_front();
                    }
                    self.latency_us.push_back(latency);
                }
            },
        }
    }

    /// Print a summary without claiming capture-to-decoded time is network latency.
    fn report(&self, elapsed: Duration) {
        println!("headless summary: frames={} fps={:.2} measured_seconds={:.3} frame_id_gaps={} duplicate_or_reordered_ids={} missing_timestamp={} future_timestamp={}",
            self.frames, self.frames as f64 / elapsed.as_secs_f64().max(0.001), elapsed.as_secs_f64(), self.frame_id_gaps, self.reordered_ids, self.missing_timestamp, self.future_timestamp);
        let mut samples: Vec<_> = self.latency_us.iter().copied().collect();
        samples.sort_unstable();
        if !samples.is_empty() {
            let percentile = |p: usize| {
                samples[(samples.len() * p).div_ceil(100).saturating_sub(1)] as f64 / 1000.0
            };
            println!("capture_to_decoded_ms: samples={} p50={:.3} p95={:.3} p99={:.3} max={:.3} (last 100000 valid samples; includes encode/transport/buffering/decode)",
                samples.len(), percentile(50), percentile(95), percentile(99), samples[samples.len()-1] as f64 / 1000.0);
        }
    }
}

/// Wait without spinning when there is no currently subscribed video track.
async fn next_optional<S: Stream + Unpin>(stream: &mut Option<S>) -> Option<S::Item> {
    match stream {
        Some(stream) => stream.next().await,
        None => pending().await,
    }
}

/// Consume one matching video track at a time, automatically reattaching on unpublish.
pub(super) async fn run(
    room: &Room,
    mut events: UnboundedReceiver<RoomEvent>,
    stop: &AtomicBool,
    options: Options,
) -> Result<()> {
    let mut csv = options.timing_output.as_deref().map(TimingCsv::create).transpose()?;
    let mut frames: Option<NativeVideoStream> = None;
    let mut timings: Option<SubscribeTimingEventStream> = None;
    let mut active_sid: Option<TrackSid> = None;
    let mut first_frame = None::<Instant>;
    let mut measurement_wall_start = None::<u64>;
    let mut measurement_start = None::<Instant>;
    let mut measurements = Measurements::default();
    let mut total_frames = 0_u64;
    let mut last_report = Instant::now();
    let startup = Instant::now();
    let mut connection = crate::connection_info::ConnectionReporter::default();
    let mut connection_tick = tokio::time::interval(Duration::from_secs(1));
    connection_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    println!(
        "Headless subscriber ready; waiting for video (startup timeout {}s)",
        options.startup_timeout.as_secs()
    );
    let result = async {
        loop {
            tokio::select! {
                _ = connection_tick.tick() => connection.sample(room).await,
                _ = tick.tick() => {
                    if stop.load(Ordering::Acquire) { break; }
                    if first_frame.is_none() && startup.elapsed() >= options.startup_timeout {
                        bail!("No video decoded before startup timeout");
                    }
                    if let Some(first) = first_frame {
                        if first.elapsed() >= options.warmup {
                            measurement_start = Some(first + options.warmup);
                        }
                    }
                    if let Some(start) = measurement_start {
                        if options.duration.is_some_and(|duration| start.elapsed() >= duration) { break; }
                        if last_report.elapsed() >= Duration::from_secs(5) {
                            println!("headless progress: frames={} measured_seconds={:.1}", measurements.frames, start.elapsed().as_secs_f64());
                            last_report = Instant::now();
                        }
                    }
                }
                event = events.recv() => match event {
                    Some(RoomEvent::TrackSubscribed { track: RemoteTrack::Video(track), participant, .. })
                        if active_sid.is_none() && options.participant.as_ref().is_none_or(|allowed| participant.identity().as_str() == allowed) => {
                            println!("Headless subscribed: participant={} track={}", participant.identity(), track.sid());
                            timings = Some(track.subscribe_timing_events());
                            frames = Some(NativeVideoStream::new(track.rtc_track()));
                            active_sid = Some(track.sid());
                            measurements.last_id = None;
                        }
                    Some(RoomEvent::TrackUnsubscribed { publication, .. } | RoomEvent::TrackUnpublished { publication, .. })
                        if active_sid.as_ref() == Some(&publication.sid()) => {
                            frames = None;
                            timings = None;
                            active_sid = None;
                            measurements.last_id = None;
                        }
                    Some(RoomEvent::Disconnected { reason }) => bail!("Room disconnected: {reason:?}"),
                    None => bail!("Room event stream closed"),
                    _ => {}
                },
                frame = next_optional(&mut frames) => {
                    if let Some(frame) = frame {
                        let now = current_timestamp_us();
                        total_frames += 1;
                        let first = *first_frame.get_or_insert_with(Instant::now);
                        measurement_wall_start.get_or_insert(now.saturating_add(options.warmup.as_micros().min(u64::MAX as u128) as u64));
                        let start = first + options.warmup;
                        let measured = Instant::now() >= start;
                        if measured { measurement_start = Some(start); }
                        let metadata = frame.frame_metadata.as_ref();
                        let capture = metadata.and_then(|m| m.user_timestamp);
                        let id = metadata.and_then(|m| m.frame_id);
                        if measured { measurements.record(now, capture, id); }
                        if let (Some(csv), Some(sid)) = (&mut csv, &active_sid) {
                            csv.record(sid.as_str(), "FrameSink", now, capture.unwrap_or_default(), id, measured)?;
                        }
                    } else {
                        frames = None;
                        timings = None;
                        active_sid = None;
                    }
                }
                event = next_optional(&mut timings) => {
                    if let Some(event) = event {
                        if let (Some(csv), Some(sid)) = (&mut csv, &active_sid) {
                            let measured = measurement_wall_start.is_some_and(|start| event.timestamp_us >= start);
                            csv.record(sid.as_str(), &format!("{:?}", event.stage), event.timestamp_us, event.capture_timestamp_us, event.frame_id, measured)?;
                        }
                    } else { timings = None; }
                }
            }
        }
        Ok(())
    }.await;
    let elapsed = measurement_start.map(|start| start.elapsed()).unwrap_or_default();
    // Drop the native sink and timing observer before disconnecting the room.
    drop(frames);
    drop(timings);
    if let Some(csv) = &mut csv {
        csv.flush()?;
    }
    measurements.report(elapsed);
    result?;
    if total_frames == 0 {
        bail!("No video frames received");
    }
    if measurements.frames == 0 {
        bail!("No video frames received after warm-up");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_wraparound_gaps_and_rejects_invalid_clock_samples() {
        let mut stats = Measurements::default();
        stats.record(100, Some(90), Some(u32::MAX));
        stats.record(100, Some(110), Some(1));
        stats.record(100, None, Some(1));
        stats.record(100, Some(0), Some(0));
        assert_eq!(stats.frame_id_gaps, 1);
        assert_eq!(stats.reordered_ids, 2);
        assert_eq!(stats.missing_timestamp, 2);
        assert_eq!(stats.future_timestamp, 1);
        assert_eq!(stats.latency_us, [10]);
    }
}
