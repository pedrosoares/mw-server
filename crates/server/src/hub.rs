//! The lobby: owns every session and room on a single task, so no locks are
//! needed and every state change is ordered. Connections talk to it through
//! [`HubEvent`]s and receive encoded frames on their own outbound queue.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mw_protocol::{ErrorCode, HEADER_LEN, PROTOCOL_VERSION, Packet, Tag, Vec3};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tracing::{debug, error, info, trace};

use crate::clients::{Clients, Session, frame};
use crate::config::Config;
use crate::ids::{ClientId, RoomId};
use crate::logic::{RoomCtx, RoomLogic, Route};
use crate::udp::UdpControl;

pub type LogicFactory = Arc<dyn Fn(RoomId) -> Box<dyn RoomLogic> + Send + Sync>;

pub(crate) enum HubEvent {
    Connected {
        id: ClientId,
        addr: SocketAddr,
        outbound: mpsc::Sender<Bytes>,
        kill: oneshot::Sender<()>,
    },
    /// A complete frame, length prefix included, so it can be relayed as-is.
    Frame {
        id: ClientId,
        frame: Bytes,
    },
    Disconnected {
        id: ClientId,
    },
}

struct Room {
    id: RoomId,
    name: String,
    owner: ClientId,
    /// Join order; the owner is always first.
    members: Vec<ClientId>,
    started: bool,
    logic: Box<dyn RoomLogic>,
}

pub(crate) struct Hub {
    config: Arc<Config>,
    logic: LogicFactory,
    udp: mpsc::UnboundedSender<UdpControl>,
    clients: Clients,
    rooms: BTreeMap<RoomId, Room>,
    next_room: u32,
}

impl Hub {
    pub fn new(
        config: Arc<Config>,
        logic: LogicFactory,
        udp: mpsc::UnboundedSender<UdpControl>,
    ) -> Self {
        Self {
            config,
            logic,
            udp,
            clients: Clients::default(),
            rooms: BTreeMap::new(),
            next_room: 0,
        }
    }

    pub async fn run(mut self, mut events: mpsc::Receiver<HubEvent>, shutdown: impl Future) {
        let mut ticker = self.config.tick_interval().map(|period| {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            ticker
        });
        let mut last_tick = Instant::now();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                event = events.recv() => match event {
                    Some(event) => self.handle(event),
                    None => break,
                },
                now = next_tick(&mut ticker) => {
                    self.tick(now - last_tick);
                    last_tick = now;
                }
            }
            self.drop_kicked();
        }

        info!(clients = self.clients.map.len(), "lobby shutting down");
        let bye = frame(&Packet::Disconnect);
        let ids: Vec<ClientId> = self.clients.map.keys().copied().collect();
        for id in ids {
            self.clients.send_frame(id, Tag::Disconnect, &bye);
        }
        // Dropping the sessions closes every outbound queue; writers flush
        // what is queued and close their sockets.
    }

    fn handle(&mut self, event: HubEvent) {
        match event {
            HubEvent::Connected {
                id,
                addr,
                outbound,
                kill,
            } => {
                let token = match getrandom::u64() {
                    Ok(token) => token,
                    Err(err) => {
                        error!(%err, "no randomness for udp token, refusing client");
                        return;
                    }
                };
                info!(client = %id, %addr, "connected");
                self.clients
                    .map
                    .insert(id, Session::new(id, addr, token, outbound, kill));
                self.udp(UdpControl::Register { client: id, token });
            }
            HubEvent::Frame { id, frame } => self.on_frame(id, frame),
            HubEvent::Disconnected { id } => self.remove_client(id, "connection closed"),
        }
    }

    fn on_frame(&mut self, id: ClientId, raw: Bytes) {
        let Some(session) = self.clients.map.get(&id) else {
            return;
        };
        let room = session.room;
        let packet = match Packet::decode(&raw[HEADER_LEN..]) {
            Ok(packet) => packet,
            Err(err) => {
                self.reject(
                    id,
                    ErrorCode::Malformed,
                    &format!("undecodable packet: {err}"),
                );
                return;
            }
        };
        trace!(client = %id, ?packet, "packet");
        let tag = packet.tag();

        match (packet, room) {
            (Packet::Ping, _) => {}
            (Packet::Disconnect, _) => self.remove_client(id, "client sent Disconnect"),
            (Packet::Hello { protocol_version }, _) => self.hello(id, protocol_version),
            (Packet::RemoveFromListMatches, _) => {
                if let Some(session) = self.clients.map.get_mut(&id) {
                    session.listing = false;
                }
            }

            // ---- lobby ----
            (Packet::LoginRequest { name }, None) => self.login(id, &name),
            (Packet::ListMatches, None) => {
                if let Some(session) = self.clients.map.get_mut(&id) {
                    session.listing = true;
                }
                let list = self.match_list();
                self.clients.send(id, &list);
            }
            (Packet::NewMatch { room_name }, None) => self.create_room(id, &room_name),
            (Packet::JoinMatch { room_id }, None) => self.join_room(id, room_id),

            // ---- room management ----
            (Packet::LeaveMatch { room_id }, Some(room)) => {
                if self.check_room(id, room, room_id, false) {
                    if self.rooms.get(&room).is_some_and(|r| r.owner == id) {
                        self.delete_room(room);
                    } else {
                        self.leave_room(id);
                    }
                }
            }
            (Packet::DeleteMatch { room_id }, Some(room)) => {
                if self.check_room(id, room, room_id, true) {
                    self.delete_room(room);
                }
            }
            (Packet::StartMatch { room_id, map }, Some(room)) => {
                if self.check_room(id, room, room_id, true) {
                    self.start(id, room, &map, &raw);
                }
            }
            (Packet::SpawnPlayers { room_id, positions }, Some(room)) => {
                if self.check_room(id, room, room_id, true) {
                    self.spawn_players(id, room, &positions);
                }
            }

            // ---- in-match relay ----
            (
                Packet::SpawnRemoteObject { .. }
                | Packet::DespawnRemoteObject { .. }
                | Packet::RemoteObjectLocation { .. }
                | Packet::Spawn { .. },
                Some(room),
            ) => self.relay(id, room, Route::Others, tag, &raw),
            (
                Packet::RemoteObjectCall {
                    id: target,
                    broadcast,
                    ..
                },
                Some(room),
            ) => {
                let route = if broadcast {
                    Route::Others
                } else {
                    match ClientId::from_wire(target) {
                        Some(target) => Route::To(target),
                        None => Route::Drop,
                    }
                };
                self.relay(id, room, route, tag, &raw);
            }
            (Packet::Message { text, .. }, Some(room)) => {
                // Rewrite the sender so clients can't impersonate each other.
                let message = Packet::Message {
                    id: id.wire(),
                    name: self.clients.name(id).to_owned(),
                    text,
                };
                self.relay(id, room, Route::Others, tag, &frame(&message));
            }
            (Packet::Game { kind, payload }, Some(room)) => {
                let mut route = Route::Others;
                self.with_logic(room, |logic, ctx| {
                    route = logic.on_game_packet(ctx, id, kind, &payload);
                });
                self.relay(id, room, route, tag, &raw);
            }

            (packet, _) => {
                let message = format!("{:?} is not allowed in this state", packet.tag());
                self.reject(id, ErrorCode::InvalidState, &message);
            }
        }
    }

    fn hello(&mut self, id: ClientId, version: u16) {
        let Some(session) = self.clients.map.get_mut(&id) else {
            return;
        };
        session.modern = true;
        if version != PROTOCOL_VERSION {
            let message = format!("server speaks protocol {PROTOCOL_VERSION}, client {version}");
            self.reject(id, ErrorCode::UnsupportedVersion, &message);
            self.remove_client(id, "unsupported protocol version");
            return;
        }
        let welcome = Packet::Welcome {
            protocol_version: PROTOCOL_VERSION,
            client_id: id.wire(),
            udp_token: session.udp_token,
        };
        self.clients.send(id, &welcome);
    }

    fn login(&mut self, id: ClientId, name: &str) {
        let name = self.sanitize(name, || format!("Player{id}"));
        if let Some(session) = self.clients.map.get_mut(&id) {
            session.name.clone_from(&name);
        }
        info!(client = %id, %name, "login");
        self.clients.send(
            id,
            &Packet::Login {
                id: id.wire(),
                name,
            },
        );
    }

    fn create_room(&mut self, owner: ClientId, name: &str) {
        self.next_room += 1;
        let room_id = RoomId(self.next_room);
        let name = self.sanitize(name, || format!("Room {room_id}"));
        info!(room = %room_id, %owner, %name, "room created");

        self.rooms.insert(
            room_id,
            Room {
                id: room_id,
                name: name.clone(),
                owner,
                members: vec![owner],
                started: false,
                logic: (self.logic)(room_id),
            },
        );
        self.set_room(owner, Some(room_id));
        self.clients.send(
            owner,
            &Packet::MatchCreated {
                id: room_id.wire(),
                owner_id: owner.wire(),
                room_name: name,
            },
        );
        self.with_logic(room_id, |logic, ctx| logic.on_join(ctx, owner));
        self.broadcast_match_list();
    }

    fn join_room(&mut self, id: ClientId, wire_room: i32) {
        let room_id = RoomId::from_wire(wire_room).filter(|room| self.rooms.contains_key(room));
        let Some(room_id) = room_id else {
            self.join_failed(id, ErrorCode::RoomNotFound, "room does not exist");
            return;
        };
        let max = self.config.max_room_players;
        if max > 0 && self.rooms[&room_id].members.len() >= max {
            self.join_failed(id, ErrorCode::RoomFull, "room is full");
            return;
        }

        self.set_room(id, Some(room_id));
        let room = self.rooms.get_mut(&room_id).expect("checked above");
        room.members.push(id);
        info!(room = %room_id, client = %id, "joined room");

        let joined = frame(&Packet::MatchJoined {
            id: room_id.wire(),
            user_id: id.wire(),
            user_name: self.clients.name(id).to_owned(),
            room_name: room.name.clone(),
        });
        for &member in &room.members {
            // Tell the new client about everyone (itself included), and
            // everyone else about the new client.
            let about_member = Packet::MatchJoined {
                id: room_id.wire(),
                user_id: member.wire(),
                user_name: self.clients.name(member).to_owned(),
                room_name: room.name.clone(),
            };
            self.clients.send(id, &about_member);
            if member != id {
                self.clients.send_frame(member, Tag::MatchJoined, &joined);
            }
        }
        self.with_logic(room_id, |logic, ctx| logic.on_join(ctx, id));
        self.broadcast_match_list();
    }

    /// v1 clients don't understand `Error`; `MatchDeleted` sends them back to
    /// the lobby instead of leaving them waiting for a join that never comes.
    fn join_failed(&mut self, id: ClientId, code: ErrorCode, message: &str) {
        if self.clients.map.get(&id).is_some_and(|s| s.modern) {
            self.reject(id, code, message);
        } else {
            debug!(client = %id, ?code, message, "join failed");
            self.clients.send(id, &Packet::MatchDeleted);
        }
    }

    fn leave_room(&mut self, id: ClientId) {
        let Some(room_id) = self.clients.map.get(&id).and_then(|s| s.room) else {
            return;
        };
        self.set_room(id, None);
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        room.members.retain(|&member| member != id);
        info!(room = %room_id, client = %id, "left room");

        let left = frame(&Packet::MatchLeaved {
            user_id: id.wire(),
            user_name: self.clients.name(id).to_owned(),
        });
        for &member in &room.members {
            self.clients.send_frame(member, Tag::MatchLeaved, &left);
        }
        self.with_logic(room_id, |logic, ctx| logic.on_leave(ctx, id));
        self.broadcast_match_list();
    }

    fn delete_room(&mut self, room_id: RoomId) {
        let Some(room) = self.rooms.remove(&room_id) else {
            return;
        };
        info!(room = %room_id, "room deleted");
        let deleted = frame(&Packet::MatchDeleted);
        for &member in &room.members {
            self.set_room(member, None);
            if member != room.owner {
                self.clients.send_frame(member, Tag::MatchDeleted, &deleted);
            }
        }
        self.udp(UdpControl::RoomClosed { room: room_id });
        self.broadcast_match_list();
    }

    fn start(&mut self, owner: ClientId, room_id: RoomId, map: &str, frame: &Bytes) {
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.started = true;
        }
        info!(room = %room_id, map, "match started");
        self.relay(owner, room_id, Route::Others, Tag::StartMatch, frame);
        self.with_logic(room_id, |logic, ctx| logic.on_start(ctx, map));
        self.broadcast_match_list();
    }

    fn spawn_players(&mut self, owner: ClientId, room_id: RoomId, positions: &[Vec3]) {
        if positions.is_empty() {
            self.reject(
                owner,
                ErrorCode::Malformed,
                "SpawnPlayers without positions",
            );
            return;
        }
        let room = &self.rooms[&room_id];
        for (index, &member) in room.members.iter().enumerate() {
            let position = positions[index % positions.len()];
            self.clients.send(member, &Packet::Spawn { position });
        }
    }

    /// Forwards an already encoded frame inside a room.
    fn relay(&mut self, from: ClientId, room_id: RoomId, route: Route, tag: Tag, frame: &Bytes) {
        let Some(room) = self.rooms.get(&room_id) else {
            return;
        };
        match route {
            Route::Others | Route::All => {
                for &member in &room.members {
                    if route == Route::All || member != from {
                        self.clients.send_frame(member, tag, frame);
                    }
                }
            }
            Route::Owner => self.clients.send_frame(room.owner, tag, frame),
            Route::To(target) if room.members.contains(&target) => {
                self.clients.send_frame(target, tag, frame);
            }
            Route::To(target) => debug!(client = %from, %target, "target not in room"),
            Route::Drop => {}
        }
    }

    /// Checks that `id` is in `room` (the room the packet names) and, if
    /// `owner_only`, that it owns it.
    fn check_room(&mut self, id: ClientId, room: RoomId, wire_room: i32, owner_only: bool) -> bool {
        if room.wire() != wire_room {
            self.reject(id, ErrorCode::InvalidState, "not in that room");
            false
        } else if owner_only && self.rooms.get(&room).is_none_or(|r| r.owner != id) {
            self.reject(id, ErrorCode::NotOwner, "only the room owner can do that");
            false
        } else {
            true
        }
    }

    fn set_room(&mut self, id: ClientId, room: Option<RoomId>) {
        if let Some(session) = self.clients.map.get_mut(&id) {
            session.room = room;
            if room.is_some() {
                session.listing = false;
            }
            self.udp(UdpControl::SetRoom { client: id, room });
        }
    }

    fn match_list(&self) -> Packet {
        Packet::MatchList {
            matches: self
                .rooms
                .values()
                .map(|room| (room.id.wire(), room.name.clone(), room.members.len() as i32))
                .collect(),
        }
    }

    fn broadcast_match_list(&mut self) {
        let subscribers: Vec<ClientId> = self
            .clients
            .map
            .iter()
            .filter(|(_, session)| session.listing)
            .map(|(&id, _)| id)
            .collect();
        if subscribers.is_empty() {
            return;
        }
        let list = frame(&self.match_list());
        for id in subscribers {
            self.clients.send_frame(id, Tag::MatchList, &list);
        }
    }

    fn remove_client(&mut self, id: ClientId, reason: &str) {
        let Some(session) = self.clients.map.get(&id) else {
            return;
        };
        if let Some(room) = session.room {
            if self.rooms.get(&room).is_some_and(|r| r.owner == id) {
                self.delete_room(room);
            } else {
                self.leave_room(id);
            }
        }
        if let Some(session) = self.clients.map.remove(&id) {
            info!(client = %id, addr = %session.addr, reason, "disconnected");
        }
        self.udp(UdpControl::Unregister { client: id });
    }

    fn drop_kicked(&mut self) {
        while let Some(id) = self.clients.kicked.pop() {
            self.remove_client(id, "too slow or closed");
        }
    }

    fn tick(&mut self, dt: Duration) {
        let started: Vec<RoomId> = self
            .rooms
            .values()
            .filter(|room| room.started)
            .map(|room| room.id)
            .collect();
        for room in started {
            self.with_logic(room, |logic, ctx| logic.on_tick(ctx, dt));
        }
    }

    fn with_logic(
        &mut self,
        room_id: RoomId,
        f: impl FnOnce(&mut dyn RoomLogic, &mut RoomCtx<'_>),
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let mut ctx = RoomCtx {
            room: room.id,
            owner: room.owner,
            members: &room.members,
            started: room.started,
            clients: &mut self.clients,
        };
        f(room.logic.as_mut(), &mut ctx);
    }

    /// Logs a refused request and tells v2 clients why.
    fn reject(&mut self, id: ClientId, code: ErrorCode, message: &str) {
        debug!(client = %id, ?code, message, "rejected");
        if self.clients.map.get(&id).is_some_and(|s| s.modern) {
            let error = Packet::Error {
                code,
                message: message.to_owned(),
            };
            self.clients.send(id, &error);
        }
    }

    fn sanitize(&self, name: &str, fallback: impl FnOnce() -> String) -> String {
        let name: String = name
            .chars()
            .filter(|c| !c.is_control())
            .take(self.config.max_name_len)
            .collect();
        let name = name.trim();
        if name.is_empty() {
            fallback()
        } else {
            name.to_owned()
        }
    }

    fn udp(&self, control: UdpControl) {
        // Only fails when the UDP relay is gone during shutdown.
        let _ = self.udp.send(control);
    }
}

async fn next_tick(ticker: &mut Option<Interval>) -> Instant {
    match ticker {
        Some(ticker) => ticker.tick().await,
        None => std::future::pending().await,
    }
}
