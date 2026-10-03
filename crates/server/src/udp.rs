//! UDP relay (voice, positions and any other unreliable traffic).
//!
//! A single task receives datagrams and forwards them to the other members of
//! the sender's room, which keeps each sender's packets in order. Room
//! membership comes from the hub through [`UdpControl`] messages, so no state
//! is shared between tasks.
//!
//! A new address must first send `UdpJoin { token }` with the token from
//! `Welcome`. After that every datagram from that address is relayed, prefixed
//! with the sender's client id; the payload format is up to the game.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use mw_protocol::Packet;
use mw_protocol::udp::stamp;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, trace};

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
}

/// Pings sent back on a successful join; the client may treat one as an ack.
const JOIN_ACKS: usize = 3;

struct ClientUdp {
    token: u64,
    room: Option<RoomId>,
    addr: Option<SocketAddr>,
}

struct Peer {
    client: ClientId,
    room: Option<RoomId>,
    limit: RateLimit,
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
    };
    let mut buf = vec![0u8; 64 * 1024];

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
        }
    }
}

impl Relay {
    async fn on_datagram(&mut self, data: &[u8], src: SocketAddr) {
        let Some(peer) = self.peers.get_mut(&src) else {
            match Packet::decode(data) {
                Ok(Packet::UdpJoin { token }) => match self.tokens.get(&token).copied() {
                    Some(client) => {
                        self.bind(client, src);
                        self.ack(src).await;
                    }
                    None => debug!(%src, "udp join with unknown token"),
                },
                _ => trace!(%src, len = data.len(), "datagram from unknown address dropped"),
            }
            return;
        };
        if !peer.limit.try_take(data.len()) {
            trace!(%src, "udp rate limit exceeded, datagram dropped");
            return;
        }
        let client = peer.client;
        let room = peer.room;

        // A retransmitted join (its acks got lost) is answered, not relayed,
        // so the token never leaks to other players.
        if let Ok(Packet::UdpJoin { token }) = Packet::decode(data)
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
        // The server vouches for the sender, so receivers can trust the id.
        stamp(client.wire(), data, &mut self.stamped);
        for &target in targets {
            if target != src
                && let Err(err) = self.socket.send_to(&self.stamped, target).await
            {
                debug!(%err, %target, "udp send failed");
            }
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
        let limit = RateLimit::new(
            self.config.udp_max_packets_per_sec,
            self.config.udp_max_bytes_per_sec,
        );
        self.peers.insert(
            src,
            Peer {
                client,
                room,
                limit,
            },
        );
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
        if let Some(peer) = self.peers.remove(&addr)
            && let Some(room) = peer.room
        {
            self.remove_from_room(room, addr);
        }
    }

    async fn ack(&self, dst: SocketAddr) {
        let ping = Packet::Ping.encode();
        for _ in 0..JOIN_ACKS {
            let _ = self.socket.send_to(&ping, dst).await;
        }
    }
}
