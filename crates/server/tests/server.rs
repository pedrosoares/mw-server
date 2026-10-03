//! End-to-end tests: a real server on localhost, driven by blocking clients
//! that speak the wire protocol like the Godot client does.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mw_server::protocol::udp::{self as udp_proto, LatestWins, Relayed};
use mw_server::protocol::{
    DEFAULT_MAX_FRAME_LEN, ErrorCode, FrameError, PROTOCOL_VERSION, Packet, read_frame,
    write_packet,
};
use mw_server::{ClientId, Config, RoomCtx, RoomLogic, Route, Server};
use tokio::sync::oneshot;

const TIMEOUT: Duration = Duration::from_secs(2);
const SILENCE: Duration = Duration::from_millis(200);

struct TestServer {
    tcp: SocketAddr,
    udp: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

fn start(configure: impl FnOnce(&mut Config)) -> TestServer {
    start_with(configure, Server::new)
}

fn start_with(
    configure: impl FnOnce(&mut Config),
    build: impl FnOnce(Config) -> Server + Send + 'static,
) -> TestServer {
    let mut config = Config {
        tcp_addr: "127.0.0.1:0".parse().unwrap(),
        udp_addr: "127.0.0.1:0".parse().unwrap(),
        ..Config::default()
    };
    configure(&mut config);

    let (addrs_tx, addrs_rx) = std::sync::mpsc::channel();
    let (stop, stop_rx) = oneshot::channel::<()>();
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let server = build(config).bind().await.unwrap();
            addrs_tx
                .send((server.tcp_addr().unwrap(), server.udp_addr().unwrap()))
                .unwrap();
            server.run(stop_rx).await.unwrap();
        });
    });
    let (tcp, udp) = addrs_rx.recv().unwrap();
    TestServer {
        tcp,
        udp,
        stop: Some(stop),
        thread: Some(thread),
    }
}

struct Client {
    stream: TcpStream,
    body: Vec<u8>,
    id: i32,
    udp_token: u64,
}

impl Client {
    fn connect(server: &TestServer) -> Self {
        let stream = TcpStream::connect(server.tcp).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        Self {
            stream,
            body: Vec::new(),
            id: -1,
            udp_token: 0,
        }
    }

    /// Connect + Hello + login.
    fn player(server: &TestServer, name: &str) -> Self {
        let mut client = Self::connect(server);
        client.send(&Packet::Hello {
            protocol_version: PROTOCOL_VERSION,
        });
        match client.recv() {
            Packet::Welcome {
                protocol_version,
                client_id,
                udp_token,
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                client.id = client_id;
                client.udp_token = udp_token;
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
        client.login(name);
        client
    }

    fn login(&mut self, name: &str) {
        self.send(&Packet::LoginRequest { name: name.into() });
        match self.recv() {
            Packet::Login { id, name: got } => {
                assert_eq!(got, name);
                self.id = id;
            }
            other => panic!("expected Login, got {other:?}"),
        }
    }

    fn send(&mut self, packet: &Packet) {
        write_packet(&mut self.stream, packet).unwrap();
    }

    fn recv(&mut self) -> Packet {
        read_frame(&mut self.stream, DEFAULT_MAX_FRAME_LEN, &mut self.body)
            .unwrap_or_else(|err| panic!("client {} got no packet: {err}", self.id));
        Packet::decode(&self.body).unwrap()
    }

    fn expect(&mut self, expected: Packet) {
        assert_eq!(self.recv(), expected, "client {}", self.id);
    }

    fn assert_silent(&mut self) {
        self.stream.set_read_timeout(Some(SILENCE)).unwrap();
        match read_frame(&mut self.stream, DEFAULT_MAX_FRAME_LEN, &mut self.body) {
            Err(FrameError::Io(err))
                if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Ok(()) => panic!(
                "client {} unexpectedly got {:?}",
                self.id,
                Packet::decode(&self.body)
            ),
            Err(err) => panic!("client {}: {err}", self.id),
        }
        self.stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    }

    fn assert_closed(&mut self) {
        let mut byte = [0u8; 1];
        match self.stream.read(&mut byte) {
            Ok(0) => {}
            Err(err) if err.kind() == ErrorKind::ConnectionReset => {}
            other => panic!("expected the server to close the connection, got {other:?}"),
        }
    }

    fn create_room(&mut self, name: &str) -> i32 {
        self.send(&Packet::NewMatch {
            room_name: name.into(),
        });
        match self.recv() {
            Packet::MatchCreated { id, owner_id, .. } => {
                assert_eq!(owner_id, self.id);
                id
            }
            other => panic!("expected MatchCreated, got {other:?}"),
        }
    }

    /// Joins and consumes the `MatchJoined` list sent to the joiner.
    fn join_room(&mut self, room: i32, members_before: usize) {
        self.send(&Packet::JoinMatch { room_id: room });
        for _ in 0..=members_before {
            assert!(matches!(self.recv(), Packet::MatchJoined { id, .. } if id == room));
        }
    }
}

fn joined(room: i32, user: &Client, user_name: &str, room_name: &str) -> Packet {
    Packet::MatchJoined {
        id: room,
        user_id: user.id,
        user_name: user_name.into(),
        room_name: room_name.into(),
    }
}

#[test]
fn lobby_and_relay_flow() {
    let server = start(|_| {});
    let mut watcher = Client::player(&server, "watcher");
    watcher.send(&Packet::ListMatches);
    watcher.expect(Packet::MatchList { matches: vec![] });

    let mut host = Client::player(&server, "host");
    let room = host.create_room("room");
    watcher.expect(Packet::MatchList {
        matches: vec![(room, "room".into(), 1)],
    });

    let mut guest = Client::player(&server, "guest");
    guest.send(&Packet::JoinMatch { room_id: room });
    guest.expect(joined(room, &host, "host", "room"));
    guest.expect(joined(room, &guest, "guest", "room"));
    host.expect(joined(room, &guest, "guest", "room"));
    watcher.expect(Packet::MatchList {
        matches: vec![(room, "room".into(), 2)],
    });

    // Replication packets are relayed verbatim to the others only.
    let location = Packet::RemoteObjectLocation {
        id: host.id,
        object_id: 1,
        position: (1.0, 2.0, 3.0),
        rotation: (0.0, 0.5, 0.0),
    };
    host.send(&location);
    guest.expect(location);
    host.assert_silent();

    // Targeted call reaches only its target.
    let call = Packet::RemoteObjectCall {
        id: host.id,
        object_id: 1,
        method: "hit".into(),
        params: vec![],
        broadcast: false,
    };
    guest.send(&call);
    host.expect(call);
    guest.assert_silent();

    // Chat sender can't be spoofed.
    guest.send(&Packet::Message {
        id: 999,
        name: "admin".into(),
        text: "hi".into(),
    });
    host.expect(Packet::Message {
        id: guest.id,
        name: "guest".into(),
        text: "hi".into(),
    });

    let start = Packet::StartMatch {
        room_id: room,
        map: "arena".into(),
    };
    host.send(&start);
    guest.expect(start);
    // Started matches leave the list.
    watcher.expect(Packet::MatchList { matches: vec![] });

    host.send(&Packet::SpawnPlayers {
        room_id: room,
        positions: vec![(1.0, 0.0, 0.0), (2.0, 0.0, 0.0)],
    });
    host.expect(Packet::Spawn {
        position: (1.0, 0.0, 0.0),
    });
    guest.expect(Packet::Spawn {
        position: (2.0, 0.0, 0.0),
    });

    guest.send(&Packet::LeaveMatch { room_id: room });
    host.expect(Packet::MatchLeaved {
        user_id: guest.id,
        user_name: "guest".into(),
    });
    watcher.expect(Packet::MatchList { matches: vec![] });

    drop(host);
    watcher.expect(Packet::MatchList { matches: vec![] });
    guest.assert_silent();
}

#[test]
fn host_disconnect_deletes_room_and_frees_guests() {
    let server = start(|_| {});
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room, 1);
    host.recv(); // MatchJoined(guest)

    drop(host);
    guest.expect(Packet::MatchDeleted);

    // The guest is back in the lobby and can host.
    guest.create_room("mine");
}

#[test]
fn guest_disconnect_notifies_room() {
    let server = start(|_| {});
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room, 1);
    host.recv(); // MatchJoined(guest)
    let guest_id = guest.id;

    drop(guest);
    host.expect(Packet::MatchLeaved {
        user_id: guest_id,
        user_name: "guest".into(),
    });
}

#[test]
fn room_management_is_checked() {
    let server = start(|config| config.max_room_players = 2);
    let mut host_a = Client::player(&server, "a");
    let room_a = host_a.create_room("a");
    let mut host_b = Client::player(&server, "b");
    let room_b = host_b.create_room("b");

    // Can't manage a room you're not in.
    host_a.send(&Packet::DeleteMatch { room_id: room_b });
    assert!(matches!(
        host_a.recv(),
        Packet::Error {
            code: ErrorCode::InvalidState,
            ..
        }
    ));

    // Guests can't manage their room.
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room_a, 1);
    host_a.recv(); // MatchJoined(guest)
    guest.send(&Packet::StartMatch {
        room_id: room_a,
        map: "m".into(),
    });
    assert!(matches!(
        guest.recv(),
        Packet::Error {
            code: ErrorCode::NotOwner,
            ..
        }
    ));
    host_a.assert_silent();

    // Full and missing rooms are refused.
    let mut late = Client::player(&server, "late");
    late.send(&Packet::JoinMatch { room_id: room_a });
    assert!(matches!(
        late.recv(),
        Packet::Error {
            code: ErrorCode::RoomFull,
            ..
        }
    ));
    late.send(&Packet::JoinMatch { room_id: 4242 });
    assert!(matches!(
        late.recv(),
        Packet::Error {
            code: ErrorCode::RoomNotFound,
            ..
        }
    ));
    // Room B is untouched.
    host_b.send(&Packet::DeleteMatch { room_id: room_b });
    host_b.assert_silent();
    host_b.create_room("again");
}

#[test]
fn bad_input_does_not_hurt_the_server() {
    let server = start(|config| config.max_frame_len = 1024);
    let mut bystander = Client::player(&server, "bystander");

    // Undecodable body: reported, connection kept.
    let mut client = Client::player(&server, "client");
    client.stream.write_all(&[0, 0, 0, 2, 250, 1]).unwrap();
    assert!(matches!(
        client.recv(),
        Packet::Error {
            code: ErrorCode::Malformed,
            ..
        }
    ));
    client.login("still-alive");

    // Oversized frame: connection closed, nothing allocated.
    let mut attacker = Client::connect(&server);
    attacker.stream.write_all(&u32::MAX.to_be_bytes()).unwrap();
    attacker.assert_closed();

    // Names are sanitized.
    client.send(&Packet::LoginRequest {
        name: "\u{0}\n".into(),
    });
    assert!(matches!(client.recv(), Packet::Login { name, .. } if name.starts_with("Player")));

    bystander.login("ok");
}

#[test]
fn hello_is_mandatory() {
    let server = start(|_| {});
    let mut client = Client::connect(&server);
    client.send(&Packet::LoginRequest { name: "x".into() });
    assert!(matches!(
        client.recv(),
        Packet::Error {
            code: ErrorCode::UnsupportedVersion,
            ..
        }
    ));
    client.assert_closed();
}

#[test]
fn started_matches_are_hidden_and_closed() {
    let server = start(|_| {});
    let mut watcher = Client::player(&server, "watcher");
    watcher.send(&Packet::ListMatches);
    watcher.expect(Packet::MatchList { matches: vec![] });
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    watcher.expect(Packet::MatchList {
        matches: vec![(room, "r".into(), 1)],
    });

    host.send(&Packet::StartMatch {
        room_id: room,
        map: "m".into(),
    });
    watcher.expect(Packet::MatchList { matches: vec![] });

    let mut late = Client::player(&server, "late");
    late.send(&Packet::JoinMatch { room_id: room });
    assert!(matches!(
        late.recv(),
        Packet::Error {
            code: ErrorCode::MatchStarted,
            ..
        }
    ));
}

#[test]
fn late_join_replays_the_match() {
    let server = start(|config| config.late_join = true);
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let object = |id: i32, object_id: i32, x: f32| Packet::SpawnRemoteObject {
        id,
        object_id,
        position: (x, 0.0, 0.0),
        rotation: (0.0, 1.0, 0.0),
    };
    host.send(&Packet::StartMatch {
        room_id: room,
        map: "yard".into(),
    });
    host.send(&object(host.id, 1, 1.0));
    host.send(&object(host.id, 2, 2.0));
    host.send(&Packet::RemoteObjectLocation {
        id: host.id,
        object_id: 1,
        position: (5.0, 0.0, 0.0),
        rotation: (0.0, 1.0, 0.0),
    });
    host.send(&Packet::DespawnRemoteObject {
        id: host.id,
        object_id: 2,
    });
    // Barrier: once this is answered, the server has seen the packets above.
    host.send(&Packet::ListMatches);
    assert!(matches!(host.recv(), Packet::Error { .. }));

    // Started rooms stay listed and joinable.
    let mut late = Client::player(&server, "late");
    late.send(&Packet::ListMatches);
    late.expect(Packet::MatchList {
        matches: vec![(room, "r".into(), 1)],
    });
    late.join_room(room, 1);
    late.expect(Packet::StartMatch {
        room_id: room,
        map: "yard".into(),
    });
    // Only the live object, at its latest position.
    late.expect(object(host.id, 1, 5.0));
    late.assert_silent();
    host.expect(joined(room, &late, "late", "r"));

    // A leaver's objects are forgotten.
    late.send(&object(late.id, 1, 9.0));
    host.recv();
    drop(late);
    assert!(matches!(host.recv(), Packet::MatchLeaved { .. }));
    let mut third = Client::player(&server, "third");
    third.join_room(room, 1);
    third.recv(); // StartMatch
    third.expect(object(host.id, 1, 5.0));
    third.assert_silent();
}

#[test]
fn host_migration_hands_the_room_over() {
    let server = start(|config| config.host_migration = true);
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut second = Client::player(&server, "second");
    second.join_room(room, 1);
    host.recv();
    let mut third = Client::player(&server, "third");
    third.join_room(room, 2);
    host.recv();
    second.recv();

    // The owner leaves: the earliest remaining member takes over.
    host.send(&Packet::LeaveMatch { room_id: room });
    let new_owner = second.id;
    for client in [&mut second, &mut third] {
        assert!(matches!(client.recv(), Packet::MatchLeaved { user_id, .. } if user_id == host.id));
        client.expect(Packet::OwnerChanged {
            room_id: room,
            owner_id: new_owner,
        });
    }
    // The new owner has the owner's rights, the others still don't.
    third.send(&Packet::StartMatch {
        room_id: room,
        map: "m".into(),
    });
    assert!(matches!(
        third.recv(),
        Packet::Error {
            code: ErrorCode::NotOwner,
            ..
        }
    ));
    second.send(&Packet::StartMatch {
        room_id: room,
        map: "m".into(),
    });
    third.expect(Packet::StartMatch {
        room_id: room,
        map: "m".into(),
    });

    // Disconnecting migrates too; the last one out closes the room.
    drop(second);
    assert!(matches!(third.recv(), Packet::MatchLeaved { .. }));
    third.expect(Packet::OwnerChanged {
        room_id: room,
        owner_id: third.id,
    });
    third.send(&Packet::LeaveMatch { room_id: room });
    third.send(&Packet::ListMatches);
    third.expect(Packet::MatchList { matches: vec![] });
}

#[test]
fn players_cannot_act_for_others() {
    let server = start(|_| {});
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room, 1);
    host.recv(); // MatchJoined(guest)

    let spoofed = [
        Packet::RemoteObjectLocation {
            id: host.id,
            object_id: 1,
            position: (0.0, 0.0, 0.0),
            rotation: (0.0, 0.0, 0.0),
        },
        Packet::DespawnRemoteObject {
            id: host.id,
            object_id: 1,
        },
        Packet::RemoteObjectCall {
            id: host.id,
            object_id: 1,
            method: "die".into(),
            params: vec![],
            broadcast: true,
        },
        Packet::Spawn {
            position: (0.0, 0.0, 0.0),
        },
    ];
    for packet in spoofed {
        guest.send(&packet);
        assert!(matches!(
            guest.recv(),
            Packet::Error {
                code: ErrorCode::InvalidState,
                ..
            }
        ));
    }
    host.assert_silent();
}

#[test]
fn unsupported_version_is_refused() {
    let server = start(|_| {});
    let mut client = Client::connect(&server);
    client.send(&Packet::Hello {
        protocol_version: 99,
    });
    assert!(matches!(
        client.recv(),
        Packet::Error {
            code: ErrorCode::UnsupportedVersion,
            ..
        }
    ));
    client.assert_closed();
}

fn udp_client() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(TIMEOUT)).unwrap();
    socket
}

fn udp_recv(socket: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 2048];
    let len = socket.recv(&mut buf).expect("expected a datagram");
    buf[..len].to_vec()
}

fn udp_assert_silent(socket: &UdpSocket) {
    socket.set_read_timeout(Some(SILENCE)).unwrap();
    let mut buf = [0u8; 2048];
    if let Ok(len) = socket.recv(&mut buf) {
        panic!("unexpected datagram {:?}", &buf[..len]);
    }
    socket.set_read_timeout(Some(TIMEOUT)).unwrap();
}

fn udp_join(server: &TestServer, packet: &Packet) -> UdpSocket {
    let socket = udp_client();
    socket.send_to(&packet.encode(), server.udp).unwrap();
    socket
}

fn expect_acks(socket: &UdpSocket) {
    for _ in 0..3 {
        assert_eq!(udp_recv(socket), Packet::Ping.encode());
    }
}

#[test]
fn udp_voice_is_authenticated_and_scoped_to_the_room() {
    let server = start(|_| {});
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room, 1);
    host.recv(); // MatchJoined(guest)

    let host_udp = udp_join(
        &server,
        &Packet::UdpJoin {
            token: host.udp_token,
        },
    );
    expect_acks(&host_udp);
    let guest_udp = udp_join(
        &server,
        &Packet::UdpJoin {
            token: guest.udp_token,
        },
    );
    expect_acks(&guest_udp);

    // v2 receivers get the datagram prefixed with the sender's id.
    let voice = udp_proto::encode_voice(b"voice-frame");
    host_udp.send_to(&voice, server.udp).unwrap();
    let received = udp_recv(&guest_udp);
    let relayed = Relayed::parse(&received).unwrap();
    assert_eq!(relayed.sender, host.id);
    assert_eq!(relayed.channel, udp_proto::CHANNEL_VOICE);
    assert_eq!(relayed.payload, b"voice-frame");
    udp_assert_silent(&host_udp);

    // Positions over UDP: newest sequence wins on the receiver.
    let mut latest = LatestWins::default();
    for sequence in [5, 3, 6] {
        let location = Packet::RemoteObjectLocation {
            id: host.id,
            object_id: 1,
            position: (sequence as f32, 0.0, 0.0),
            rotation: (0.0, 0.0, 0.0),
        };
        host_udp
            .send_to(&udp_proto::encode_state(sequence, &location), server.udp)
            .unwrap();
    }
    let mut applied = Vec::new();
    for _ in 0..3 {
        let received = udp_recv(&guest_udp);
        let relayed = Relayed::parse(&received).unwrap();
        assert_eq!(relayed.channel, udp_proto::CHANNEL_STATE);
        let (sequence, packet) = udp_proto::decode_state(relayed.payload).unwrap();
        let Packet::RemoteObjectLocation { object_id, .. } = packet else {
            panic!("expected a location, got {packet:?}");
        };
        if latest.accept(relayed.sender, object_id, sequence) {
            applied.push(sequence);
        }
    }
    assert_eq!(applied, [5, 6]);

    // A wrong token and the removed v1 join are ignored.
    let intruder = udp_join(&server, &Packet::UdpJoin { token: 12345 });
    let legacy = udp_join(&server, &Packet::JoinMatch { room_id: room });
    udp_assert_silent(&intruder);
    udp_assert_silent(&legacy);
    intruder.send_to(b"spam", server.udp).unwrap();
    udp_assert_silent(&guest_udp);

    // After leaving, the guest no longer hears the room.
    guest.send(&Packet::LeaveMatch { room_id: room });
    host.recv(); // MatchLeaved
    host_udp.send_to(b"after-leave", server.udp).unwrap();
    udp_assert_silent(&guest_udp);
}

#[test]
fn udp_excess_is_dropped() {
    let server = start(|config| config.udp_max_packets_per_sec = 5);
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room, 1);
    let host_udp = udp_join(
        &server,
        &Packet::UdpJoin {
            token: host.udp_token,
        },
    );
    expect_acks(&host_udp);
    let guest_udp = udp_join(
        &server,
        &Packet::UdpJoin {
            token: guest.udp_token,
        },
    );
    expect_acks(&guest_udp);

    for _ in 0..50 {
        host_udp.send_to(b"\0x", server.udp).unwrap();
    }
    guest_udp.set_read_timeout(Some(SILENCE)).unwrap();
    let mut buf = [0u8; 64];
    let mut received = 0;
    while guest_udp.recv(&mut buf).is_ok() {
        received += 1;
    }
    assert!((5..=7).contains(&received), "received {received}");
}

#[test]
fn tcp_flood_is_throttled_without_loss() {
    let server = start(|config| config.max_msgs_per_sec = 20);
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room, 1);
    host.recv(); // MatchJoined(guest)

    // Login/create/join already used a few tokens; the rest of the first
    // second's budget goes through at once, then 20 per second.
    let started = Instant::now();
    for object_id in 0..60 {
        host.send(&Packet::RemoteObjectLocation {
            id: host.id,
            object_id,
            position: (0.0, 0.0, 0.0),
            rotation: (0.0, 0.0, 0.0),
        });
    }
    for object_id in 0..60 {
        assert!(matches!(
            guest.recv(),
            Packet::RemoteObjectLocation { object_id: got, .. } if got == object_id
        ));
    }
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(1800), "took {elapsed:?}");
}

/// Server-authoritative example: kind 1 is answered by the server, kind 5 goes
/// to the host only, and every tick of a started match pings everyone once.
#[derive(Default)]
struct TestLogic {
    ticked: bool,
}

impl RoomLogic for TestLogic {
    fn on_game_packet(
        &mut self,
        ctx: &mut RoomCtx<'_>,
        _from: ClientId,
        kind: u16,
        payload: &[u8],
    ) -> Route {
        match kind {
            1 => {
                let reversed: Vec<u8> = payload.iter().rev().copied().collect();
                ctx.broadcast_game(2, &reversed, None);
                Route::Drop
            }
            5 => Route::Owner,
            _ => Route::Others,
        }
    }

    fn on_tick(&mut self, ctx: &mut RoomCtx<'_>, _dt: Duration) {
        if !self.ticked {
            self.ticked = true;
            ctx.broadcast_game(9, &[], None);
        }
    }
}

#[test]
fn room_logic_routes_game_packets() {
    let server = start_with(
        |config| config.tick_rate = 50,
        |config| Server::new(config).with_room_logic(|_| Box::new(TestLogic::default())),
    );
    let mut host = Client::player(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::player(&server, "guest");
    guest.join_room(room, 1);
    host.recv();
    guest.send(&Packet::Game {
        kind: 1,
        payload: vec![1, 2, 3],
    });
    let answer = Packet::Game {
        kind: 2,
        payload: vec![3, 2, 1],
    };
    host.expect(answer.clone());
    guest.expect(answer);

    let to_host = Packet::Game {
        kind: 5,
        payload: vec![7],
    };
    guest.send(&to_host);
    host.expect(to_host);
    guest.assert_silent();

    host.send(&Packet::StartMatch {
        room_id: room,
        map: "m".into(),
    });
    guest.recv(); // StartMatch
    let tick = Packet::Game {
        kind: 9,
        payload: vec![],
    };
    host.expect(tick.clone());
    guest.expect(tick);
}
