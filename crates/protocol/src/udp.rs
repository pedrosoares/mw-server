//! UDP datagram conventions.
//!
//! The server never parses relayed payloads; these are the conventions the
//! bundled clients use on top of it.
//!
//! - client → server: `[channel: u8][payload]` (after `UdpJoin`)
//! - server → v2 client: `[sender client_id: i32 BE][channel: u8][payload]`.
//!   The server writes the sender id, so receivers can trust it.
//! - server → v1 client: the datagram exactly as sent (no sender id).
//! - The join acknowledgement is the 1-byte `Ping` datagram `[0]`. Relayed
//!   datagrams are always at least `SENDER_LEN + 1` bytes long.
//!
//! The `CHANNEL_STATE` payload is `[sequence: u32 BE][postcard Packet body]`,
//! normally a `RemoteObjectLocation`. Receivers keep only the newest sequence
//! per `(sender, object)`; see [`LatestWins`].

use std::collections::HashMap;

use crate::Packet;

/// Length of the sender id the server prepends for v2 receivers.
pub const SENDER_LEN: usize = 4;
pub const CHANNEL_VOICE: u8 = 0;
pub const CHANNEL_STATE: u8 = 1;
/// Channels from this one on are free for game use.
pub const CHANNEL_GAME: u8 = 16;
/// Sender id stamped on datagrams from legacy (unauthenticated) peers.
pub const UNKNOWN_SENDER: i32 = -1;

/// Writes `datagram` prefixed with the sender id into `out` (server side).
pub fn stamp(sender: i32, datagram: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&sender.to_be_bytes());
    out.extend_from_slice(datagram);
}

pub fn is_join_ack(datagram: &[u8]) -> bool {
    datagram == [0]
}

/// A datagram as received by a v2 client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relayed<'a> {
    pub sender: i32,
    pub channel: u8,
    pub payload: &'a [u8],
}

impl<'a> Relayed<'a> {
    pub fn parse(datagram: &'a [u8]) -> Option<Self> {
        let (sender, rest) = datagram.split_first_chunk::<SENDER_LEN>()?;
        let (&channel, payload) = rest.split_first()?;
        Some(Self {
            sender: i32::from_be_bytes(*sender),
            channel,
            payload,
        })
    }
}

pub fn encode_voice(frame: &[u8]) -> Vec<u8> {
    let mut datagram = Vec::with_capacity(1 + frame.len());
    datagram.push(CHANNEL_VOICE);
    datagram.extend_from_slice(frame);
    datagram
}

pub fn encode_state(sequence: u32, packet: &Packet) -> Vec<u8> {
    let mut datagram = vec![CHANNEL_STATE];
    datagram.extend_from_slice(&sequence.to_be_bytes());
    postcard::to_extend(packet, datagram).expect("Packet is always serializable")
}

/// Decodes a `CHANNEL_STATE` payload (without the channel byte).
pub fn decode_state(payload: &[u8]) -> Option<(u32, Packet)> {
    let (sequence, body) = payload.split_first_chunk::<4>()?;
    let packet = Packet::decode(body).ok()?;
    Some((u32::from_be_bytes(*sequence), packet))
}

/// Whether sequence `a` is newer than `b`, tolerating wrap-around.
pub fn is_newer(a: u32, b: u32) -> bool {
    a != b && (a.wrapping_sub(b) as i32) > 0
}

/// Drops out-of-order or duplicated state updates.
#[derive(Debug, Default)]
pub struct LatestWins {
    last: HashMap<(i32, i32), u32>,
}

impl LatestWins {
    /// Records `sequence` for `(sender, object)`. Returns false if it is stale.
    pub fn accept(&mut self, sender: i32, object: i32, sequence: u32) -> bool {
        match self.last.get_mut(&(sender, object)) {
            Some(last) if !is_newer(sequence, *last) => false,
            Some(last) => {
                *last = sequence;
                true
            }
            None => {
                self.last.insert((sender, object), sequence);
                true
            }
        }
    }

    /// Forgets a sender, e.g. after it left the room.
    pub fn forget(&mut self, sender: i32) {
        self.last.retain(|(s, _), _| *s != sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip_through_relay() {
        let location = Packet::RemoteObjectLocation {
            id: 3,
            object_id: 9,
            position: (1.0, 2.0, 3.0),
            rotation: (0.0, 0.0, 0.0),
        };
        let sent = encode_state(77, &location);
        let mut relayed = Vec::new();
        stamp(3, &sent, &mut relayed);

        let datagram = Relayed::parse(&relayed).unwrap();
        assert_eq!(datagram.sender, 3);
        assert_eq!(datagram.channel, CHANNEL_STATE);
        assert_eq!(decode_state(datagram.payload), Some((77, location)));
        assert!(!is_join_ack(&relayed));
        assert!(is_join_ack(&Packet::Ping.encode()));
    }

    #[test]
    fn latest_wins_handles_reordering_and_wraparound() {
        let mut latest = LatestWins::default();
        assert!(latest.accept(1, 1, 10));
        assert!(!latest.accept(1, 1, 9));
        assert!(!latest.accept(1, 1, 10));
        assert!(latest.accept(1, 2, 1), "objects are independent");
        assert!(latest.accept(2, 1, 1), "senders are independent");
        assert!(latest.accept(1, 3, u32::MAX - 1));
        assert!(latest.accept(1, 3, 1), "wraps around");
        assert!(!latest.accept(1, 3, u32::MAX), "pre-wrap is now stale");
        latest.forget(1);
        assert!(latest.accept(1, 1, 5));
    }

    #[test]
    fn parse_rejects_short_datagrams() {
        assert_eq!(Relayed::parse(&[]), None);
        assert_eq!(Relayed::parse(&[0, 0, 0, 1]), None);
        assert_eq!(decode_state(&[0, 0, 1]), None);
    }
}
