//! UDP relay (voice and any other unreliable traffic).
//!
//! A single task receives datagrams and forwards them to the other members of
//! the sender's room, which keeps each sender's packets in order. Room
//! membership comes from the hub through [`UdpControl`] messages, so no state
//! is shared between tasks.
//!
//! A new address must first send `UdpJoin { token }` with the token from
//! `Welcome`. After that every datagram from that address is relayed verbatim;
//! the payload format is up to the game.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use mw_protocol::Packet;
use mw_protocol::udp::{UNKNOWN_SENDER, stamp};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, trace, warn};

use crate::config::Config;
use crate::ids::{ClientId, RoomId};
use crate::rate::RateLimit;

pub(crate) enum UdpControl {
    Register {
        client: ClientId,
        token: u64,
    },
    SetRoom {
        client: ClientId,
        room: Option<RoomId>,
    },
    Unregister {
        client: ClientId,
    },
    RoomClosed {
        room: RoomId,
    },
}

/// Pings sent back on a successful join; the client may treat one as an ack.
const JOIN_ACKS: usize = 3;

struct ClientUdp {
    token: u64,
    room: Option<RoomId>,
    addr: Option<SocketAddr>,
}

struct Peer {
    /// `None` for peers that joined with the legacy, unauthenticated packet.
    client: Option<ClientId>,
    room: Option<RoomId>,
    last_seen: Instant,
    limit: RateLimit,
}

impl Peer {
    fn new(client: Option<ClientId>, room: Option<RoomId>, config: &Config) -> Self {
        Self {
            client,
            room,
            last_seen: Instant::now(),
            limit: RateLimit::new(config.udp_max_packets_per_sec, config.udp_max_bytes_per_sec),
        }
    }
}

struct Relay {
    socket: UdpSocket,
    config: Arc<Config>,
    tokens: HashMap<u64, ClientId>,
    clients: HashMap<ClientId, ClientUdp>,
    peers: HashMap<SocketAddr, Peer>,
    rooms: HashMap<RoomId, Vec<SocketAddr>>,
    /// Reused buffer for the sender-stamped copy of a datagram.
    stamped: Vec<u8>,
    warned_legacy: bool,
}

pub(crate) async fn run(
    socket: UdpSocket,
    mut control: mpsc::UnboundedReceiver<UdpControl>,
    config: Arc<Config>,
) {
    let mut relay = Relay {
        socket,
        config,
        tokens: HashMap::new(),
        clients: HashMap::new(),
        peers: HashMap::new(),
        rooms: HashMap::new(),
        stamped: Vec::with_capacity(2048),
        warned_legacy: false,
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut sweep = tokio::time::interval(relay.config.udp_peer_timeout() / 2);
    sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Membership changes first, so a datagram sent right after a TCP
            // join/leave is routed with the new membership.
            biased;
            command = control.recv() => match command {
                Some(command) => relay.apply(command),
                None => break,
            },
            received = relay.socket.recv_from(&mut buf) => match received {
                Ok((len, src)) => relay.on_datagram(&buf[..len], src).await,
                // e.g. ICMP port unreachable from a peer that went away.
                Err(err) => debug!(%err, "udp recv error"),
            },
            _ = sweep.tick() => relay.expire_legacy_peers(),
        }
    }
}

impl Relay {
    async fn on_datagram(&mut self, data: &[u8], src: SocketAddr) {
        if let Some(peer) = self.peers.get_mut(&src) {
            peer.last_seen = Instant::now();
            if !peer.limit.try_take(data.len()) {
                trace!(%src, "udp rate limit exceeded, datagram dropped");
                return;
            }
            let client = peer.client;
            let room = peer.room;
            // A retransmitted join (its acks got lost) is answered, not relayed,
            // so the token never leaks to other players.
            if let Some(client) = client
                && let Ok(Packet::UdpJoin { token }) = Packet::decode(data)
                && self.clients.get(&client).is_some_and(|c| c.token == token)
            {
                self.ack(src).await;
                return;
            }
            if data.is_empty() {
                return;
            }
            let Some(targets) = room.and_then(|room| self.rooms.get(&room)) else {
                return;
            };
            // v2 receivers get the sender id the server vouches for; v1
            // receivers get the datagram untouched.
            let sender = client.map_or(UNKNOWN_SENDER, ClientId::wire);
            stamp(sender, data, &mut self.stamped);
            for &target in targets {
                if target == src {
                    continue;
                }
                let modern = self.peers.get(&target).is_some_and(|p| p.client.is_some());
                let datagram = if modern { &self.stamped[..] } else { data };
                if let Err(err) = self.socket.send_to(datagram, target).await {
                    debug!(%err, %target, "udp send failed");
                }
            }
            return;
        }

        match Packet::decode(data) {
            Ok(Packet::UdpJoin { token }) => match self.tokens.get(&token).copied() {
                Some(client) => {
                    self.bind(client, src);
                    self.ack(src).await;
                }
                None => debug!(%src, "udp join with unknown token"),
            },
            Ok(Packet::JoinMatch { room_id }) if self.config.legacy_udp_join => {
                let Some(room) = RoomId::from_wire(room_id) else {
                    return;
                };
                info!(%src, %room, "legacy udp peer joined");
                self.peers
                    .insert(src, Peer::new(None, Some(room), &self.config));
                self.rooms.entry(room).or_default().push(src);
                self.ack(src).await;
            }
            Ok(Packet::JoinMatch { .. }) => {
                if !self.warned_legacy {
                    self.warned_legacy = true;
                    warn!(
                        %src,
                        "rejected unauthenticated UDP JoinMatch; update the client to send UdpJoin \
                         or start the server with --legacy-udp-join"
                    );
                }
            }
            _ => debug!(%src, len = data.len(), "datagram from unknown address dropped"),
        }
    }

    /// Binds a client's address, replacing any previous one (NAT rebinding,
    /// reconnect).
    fn bind(&mut self, client: ClientId, src: SocketAddr) {
        let Some(state) = self.clients.get_mut(&client) else {
            return;
        };
        let old = state.addr.replace(src);
        let room = state.room;
        if let Some(old) = old {
            self.forget_addr(old);
        }
        info!(%client, %src, "udp peer joined");
        self.peers
            .insert(src, Peer::new(Some(client), room, &self.config));
        if let Some(room) = room {
            self.rooms.entry(room).or_default().push(src);
        }
    }

    fn apply(&mut self, command: UdpControl) {
        match command {
            UdpControl::Register { client, token } => {
                self.tokens.insert(token, client);
                self.clients.insert(
                    client,
                    ClientUdp {
                        token,
                        room: None,
                        addr: None,
                    },
                );
            }
            UdpControl::SetRoom { client, room } => {
                let Some(state) = self.clients.get_mut(&client) else {
                    return;
                };
                state.room = room;
                if let Some(addr) = state.addr {
                    self.move_peer(addr, room);
                }
            }
            UdpControl::Unregister { client } => {
                if let Some(state) = self.clients.remove(&client) {
                    self.tokens.remove(&state.token);
                    if let Some(addr) = state.addr {
                        self.forget_addr(addr);
                    }
                }
            }
            UdpControl::RoomClosed { room } => {
                // Authenticated peers were already moved out by `SetRoom`;
                // this drops the legacy ones.
                if let Some(addrs) = self.rooms.remove(&room) {
                    for addr in addrs {
                        self.peers.remove(&addr);
                    }
                }
            }
        }
    }

    /// Moves a bound address to another room, keeping its rate limit state.
    fn move_peer(&mut self, addr: SocketAddr, room: Option<RoomId>) {
        let Some(peer) = self.peers.get_mut(&addr) else {
            return;
        };
        let old = std::mem::replace(&mut peer.room, room);
        if let Some(old) = old {
            self.remove_from_room(old, addr);
        }
        if let Some(room) = room {
            self.rooms.entry(room).or_default().push(addr);
        }
    }

    fn remove_from_room(&mut self, room: RoomId, addr: SocketAddr) {
        if let Some(addrs) = self.rooms.get_mut(&room) {
            addrs.retain(|&a| a != addr);
            if addrs.is_empty() {
                self.rooms.remove(&room);
            }
        }
    }

    fn forget_addr(&mut self, addr: SocketAddr) {
        let Some(peer) = self.peers.remove(&addr) else {
            return;
        };
        if let Some(room) = peer.room {
            self.remove_from_room(room, addr);
        }
    }

    fn expire_legacy_peers(&mut self) {
        let timeout = self.config.udp_peer_timeout();
        let expired: Vec<SocketAddr> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.client.is_none() && peer.last_seen.elapsed() > timeout)
            .map(|(&addr, _)| addr)
            .collect();
        for addr in expired {
            debug!(%addr, "legacy udp peer expired");
            self.forget_addr(addr);
        }
    }

    async fn ack(&self, dst: SocketAddr) {
        let ping = Packet::Ping.encode();
        for _ in 0..JOIN_ACKS {
            let _ = self.socket.send_to(&ping, dst).await;
        }
    }
}
