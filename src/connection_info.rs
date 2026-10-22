//! Report the selected ICE path, rather than inferring it from signaling or DTLS.

use livekit::{
    prelude::Room,
    webrtc::stats::{
        dictionaries::IceCandidateStats, IceCandidateType, IceServerTransportProtocol, RtcStats,
    },
};
use std::collections::BTreeMap;

/// Remembers selected paths so only initial selections and changes are printed.
#[derive(Default)]
pub(crate) struct ConnectionReporter {
    previous: BTreeMap<String, String>,
}

impl ConnectionReporter {
    /// Inspect all peer connections; failures leave the previous report intact.
    pub(crate) async fn sample(&mut self, room: &Room) {
        match room.get_stats().await {
            Ok(stats) => {
                self.report("publisher", &stats.publisher_stats);
                self.report("subscriber", &stats.subscriber_stats);
            }
            Err(error) => log::debug!("Connection stats unavailable: {error}"),
        }
    }

    /// Follow transport.selectedCandidatePairId so unused candidates are never reported as active.
    fn report(&mut self, peer: &str, stats: &[RtcStats]) {
        let mut current = BTreeMap::new();
        for stat in stats {
            let RtcStats::Transport(transport) = stat else {
                continue;
            };
            let selected = &transport.transport.selected_candidate_pair_id;
            if selected.is_empty() {
                continue;
            }
            let Some(pair) = stats.iter().find_map(|stat| match stat {
                RtcStats::CandidatePair(pair) if &pair.rtc.id == selected => {
                    Some(&pair.candidate_pair)
                }
                _ => None,
            }) else {
                continue;
            };
            let local = stats.iter().find_map(|stat| match stat {
                RtcStats::LocalCandidate(candidate)
                    if candidate.rtc.id == pair.local_candidate_id =>
                {
                    Some(&candidate.local_candidate)
                }
                _ => None,
            });
            let remote = stats.iter().find_map(|stat| match stat {
                RtcStats::RemoteCandidate(candidate)
                    if candidate.rtc.id == pair.remote_candidate_id =>
                {
                    Some(&candidate.remote_candidate)
                }
                _ => None,
            });
            let (Some(local), Some(remote)) = (local, remote) else {
                continue;
            };
            let key = format!("{peer}/{}", transport.rtc.id);
            let details = format!(
                "pair={} path={} local={} remote={} relay_protocol={}",
                selected,
                path_name(local, remote),
                endpoint(local),
                endpoint(remote),
                relay_protocol(local)
            );
            if self.previous.get(&key) != Some(&details) {
                println!(
                    "WebRTC connection: peer={peer} transport={} {details} ice_rtt_ms={:.2}",
                    transport.rtc.id,
                    pair.current_round_trip_time * 1000.0
                );
            }
            current.insert(key, details);
        }
        self.previous
            .retain(|key, _| !key.starts_with(&format!("{peer}/")) || current.contains_key(key));
        self.previous.extend(current);
    }
}

/// TURN's client-to-relay transport is independent of the relay candidate's protocol.
fn relay_protocol(candidate: &IceCandidateStats) -> &'static str {
    match candidate.relay_protocol {
        Some(IceServerTransportProtocol::Udp) => "udp",
        Some(IceServerTransportProtocol::Tcp) => "tcp",
        Some(IceServerTransportProtocol::Tls) => "tls",
        None => "unknown",
    }
}

/// Classify only from the selected candidates; never mistake media DTLS for TURN TLS.
fn path_name(local: &IceCandidateStats, remote: &IceCandidateStats) -> String {
    // Libwebrtc can expose a peer-reflexive selected candidate derived from a
    // TURN allocation. relayProtocol still describes its client-to-TURN leg.
    if local.relay_protocol.is_some() || local.candidate_type == Some(IceCandidateType::Relay) {
        format!("TURN/{}", relay_protocol(local).to_uppercase())
    } else if remote.candidate_type == Some(IceCandidateType::Relay) {
        "TURN/remote-relay".into()
    } else {
        format!("ICE/{}", local.protocol.to_uppercase())
    }
}

/// Describe the candidate endpoint without including TURN URLs or credentials.
fn endpoint(candidate: &IceCandidateStats) -> String {
    let kind = match candidate.candidate_type {
        Some(IceCandidateType::Host) => "host",
        Some(IceCandidateType::Srflx) => "srflx",
        Some(IceCandidateType::Prflx) => "prflx",
        Some(IceCandidateType::Relay) => "relay",
        None => "unknown",
    };
    format!("{kind}/{}/[{}]:{}", candidate.protocol, candidate.address, candidate.port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use livekit::webrtc::stats::{
        CandidatePairStats, LocalCandidateStats, RemoteCandidateStats, TransportStats,
    };

    #[test]
    fn distinguishes_turn_tls_from_udp_relay_candidate() {
        let mut local = IceCandidateStats::default();
        local.protocol = "udp".into();
        local.candidate_type = Some(IceCandidateType::Relay);
        local.relay_protocol = Some(IceServerTransportProtocol::Tls);
        assert_eq!(path_name(&local, &IceCandidateStats::default()), "TURN/TLS");
        local.candidate_type = Some(IceCandidateType::Prflx);
        assert_eq!(path_name(&local, &IceCandidateStats::default()), "TURN/TLS");
        local.candidate_type = Some(IceCandidateType::Relay);
        local.relay_protocol = None;
        assert_eq!(path_name(&local, &IceCandidateStats::default()), "TURN/UNKNOWN");
        local.candidate_type = Some(IceCandidateType::Host);
        assert_eq!(path_name(&local, &IceCandidateStats::default()), "ICE/UDP");
        local.protocol = "tcp".into();
        assert_eq!(path_name(&local, &IceCandidateStats::default()), "ICE/TCP");
    }

    #[test]
    fn follows_selected_pair_and_detects_path_change() {
        let mut transport = TransportStats::default();
        transport.rtc.id = "transport".into();
        transport.transport.selected_candidate_pair_id = "selected".into();
        let mut pair = CandidatePairStats::default();
        pair.rtc.id = "selected".into();
        pair.candidate_pair.local_candidate_id = "local".into();
        pair.candidate_pair.remote_candidate_id = "remote".into();
        let mut local = LocalCandidateStats::default();
        local.rtc.id = "local".into();
        local.local_candidate.protocol = "udp".into();
        local.local_candidate.candidate_type = Some(IceCandidateType::Host);
        let mut remote = RemoteCandidateStats::default();
        remote.rtc.id = "remote".into();
        let mut unused = pair.clone();
        unused.rtc.id = "unused".into();
        let mut stats = vec![
            RtcStats::Transport(transport),
            RtcStats::CandidatePair(unused),
            RtcStats::CandidatePair(pair),
            RtcStats::LocalCandidate(local),
            RtcStats::RemoteCandidate(remote),
        ];
        let mut reporter = ConnectionReporter::default();
        reporter.report("publisher", &stats);
        assert_eq!(reporter.previous.len(), 1);
        assert!(reporter.previous["publisher/transport"].contains("pair=selected path=ICE/UDP"));
        if let RtcStats::LocalCandidate(local) = &mut stats[3] {
            local.local_candidate.candidate_type = Some(IceCandidateType::Relay);
            local.local_candidate.relay_protocol = Some(IceServerTransportProtocol::Tls);
        }
        reporter.report("publisher", &stats);
        assert!(reporter.previous["publisher/transport"].contains("TURN/TLS"));
        reporter.report("publisher", &[]);
        assert!(reporter.previous.is_empty());
    }
}
