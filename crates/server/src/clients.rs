use std::collections::HashMap;
use std::net::SocketAddr;

use bytes::Bytes;
use mw_protocol::{Packet, Tag};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::ids::{ClientId, RoomId};

pub(crate) struct Session {
    pub addr: SocketAddr,
    pub name: String,
    pub room: Option<RoomId>,
    /// Sent `Hello`, so it understands v2 packets.
    pub modern: bool,
    /// Subscribed to `MatchList` updates.
    pub listing: bool,
    pub udp_token: u64,
    outbound: mpsc::Sender<Bytes>,
    /// Dropping this stops the connection's reader task.
    _kill: oneshot::Sender<()>,
}

impl Session {
    pub fn new(
        id: ClientId,
        addr: SocketAddr,
        udp_token: u64,
        outbound: mpsc::Sender<Bytes>,
        kill: oneshot::Sender<()>,
    ) -> Self {
        Self {
            addr,
            name: format!("Player{id}"),
            room: None,
            modern: false,
            listing: false,
            udp_token,
            outbound,
            _kill: kill,
        }
    }
}

/// All connected sessions plus the clients that must be dropped once the
/// current event is handled (sending never removes a session itself, so it
/// is safe to call while iterating rooms).
#[derive(Default)]
pub(crate) struct Clients {
    pub map: HashMap<ClientId, Session>,
    pub kicked: Vec<ClientId>,
}

pub(crate) fn frame(packet: &Packet) -> Bytes {
    Bytes::from(packet.encode_frame())
}

impl Clients {
    /// Queues an encoded frame. Never blocks: a client whose queue is full is
    /// too slow to keep up and gets dropped instead of stalling everyone.
    pub fn send_frame(&mut self, to: ClientId, tag: Tag, frame: &Bytes) {
        let Some(session) = self.map.get(&to) else {
            return;
        };
        if tag.is_v2() && !session.modern {
            return;
        }
        match session.outbound.try_send(frame.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                warn!(client = %to, "outbound queue full, dropping slow client");
                self.kicked.push(to);
            }
            Err(TrySendError::Closed(_)) => self.kicked.push(to),
        }
    }

    pub fn send(&mut self, to: ClientId, packet: &Packet) {
        self.send_frame(to, packet.tag(), &frame(packet));
    }

    pub fn name(&self, id: ClientId) -> &str {
        self.map.get(&id).map_or("", |s| s.name.as_str())
    }
}
