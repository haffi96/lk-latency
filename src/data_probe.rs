//! Bounded, explicitly stopped data-channel round trips alongside video.
use livekit::prelude::*;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle};

const REQUEST: &str = "firewall-probe-request";
const RESPONSE: &str = "firewall-probe-response";

/// Owns cancellation and joining for one room's data probe.
pub(crate) struct ProbeTask {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl ProbeTask {
    /// Echo requests, or initiate matched round trips when a target is provided.
    pub(crate) fn start(room: &Arc<Room>, target: Option<String>) -> Self {
        let mut events = room.subscribe();
        let weak = Arc::downgrade(room);
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut pending = BTreeMap::new();
            let mut sequence = 0_u64;
            let (mut reliable_count, mut lossy_count, mut errors) = (0_u64, 0_u64, 0_u64);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = tick.tick(), if target.is_some() => {
                        let Some(room) = weak.upgrade() else { break };
                        let Some(target) = target.as_ref() else { continue };
                        sequence += 1;
                        pending.retain(|_, sent: &mut Instant| sent.elapsed() < Duration::from_secs(10));
                        for reliable in [true, false] {
                            let payload = format!("{}:{sequence}:{reliable}", room.local_participant().identity()).into_bytes();
                            pending.insert(payload.clone(), Instant::now());
                            let packet = DataPacket {payload, topic: Some(REQUEST.into()), reliable,
                                destination_identities: vec![target.clone().into()]};
                            if let Err(error) = room.local_participant().publish_data(packet).await {
                                errors += 1;
                                println!("Data probe send failed: {error}");
                            }
                        }
                    }
                    event = events.recv() => {
                        let Some(event) = event else { break };
                        let RoomEvent::DataReceived {payload, topic, kind, participant: Some(participant)} = event else { continue };
                        let reliable = matches!(kind, DataPacketKind::Reliable);
                        let Some(room) = weak.upgrade() else { break };
                        if target.is_none() && topic.as_deref() == Some(REQUEST) {
                            let packet = DataPacket {payload: payload.as_ref().clone(), topic: Some(RESPONSE.into()), reliable,
                                destination_identities: vec![participant.identity()]};
                            match room.local_participant().publish_data(packet).await {
                                Ok(()) => {
                                    if reliable { reliable_count += 1; } else { lossy_count += 1; }
                                    println!("Data probe echoed: reliable={reliable} peer={}", participant.identity());
                                }
                                Err(error) => { errors += 1; println!("Data probe echo failed: {error}"); }
                            }
                        } else if target.as_deref() == Some(participant.identity().as_str()) && topic.as_deref() == Some(RESPONSE) {
                            if let Some(sent) = pending.remove(payload.as_ref()) {
                                if reliable { reliable_count += 1; } else { lossy_count += 1; }
                                println!("Data probe roundtrip: reliable={reliable} rtt_ms={:.3}", sent.elapsed().as_secs_f64()*1000.0);
                            }
                        }
                    }
                }
            }
            println!("Data probe summary: role={} reliable={reliable_count} lossy={lossy_count} errors={errors}", if target.is_some() {"initiator"} else {"echo"});
        });
        Self { stop: Some(stop), task }
    }

    /// Stop the probe and wait until it releases its subscription and room reference.
    pub(crate) async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = (&mut self.task).await;
    }
}

impl Drop for ProbeTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}
