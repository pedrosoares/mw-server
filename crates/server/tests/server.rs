//! End-to-end tests: a real server on localhost, driven by blocking clients
//! that speak the wire protocol like the Godot client does.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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

    /// A v1 client: connect + login.
    fn legacy(server: &TestServer, name: &str) -> Self {
        let mut client = Self::connect(server);
        client.login(name);
        client
    }

    /// A v2 client: connect + hello + login.
    fn modern(server: &TestServer, name: &str) -> Self {
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
    let mut watcher = Client::legacy(&server, "watcher");
    watcher.send(&Packet::ListMatches);
    watcher.expect(Packet::MatchList { matches: vec![] });

    let mut host = Client::legacy(&server, "host");
    let room = host.create_room("room");
    watcher.expect(Packet::MatchList {
        matches: vec![(room, "room".into(), 1)],
    });

    let mut guest = Client::legacy(&server, "guest");
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
    watcher.expect(Packet::MatchList {
        matches: vec![(room, "room".into(), 2)],
    });

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
    watcher.expect(Packet::MatchList {
        matches: vec![(room, "room".into(), 1)],
    });

    drop(host);
    watcher.expect(Packet::MatchList { matches: vec![] });
    guest.assert_silent();
}

#[test]
fn host_disconnect_deletes_room_and_frees_guests() {
    let server = start(|_| {});
    let mut host = Client::legacy(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::legacy(&server, "guest");
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
    let mut host = Client::legacy(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::legacy(&server, "guest");
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
    let mut host_a = Client::modern(&server, "a");
    let room_a = host_a.create_room("a");
    let mut host_b = Client::modern(&server, "b");
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
    let mut guest = Client::modern(&server, "guest");
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

    // Full and missing rooms: v2 gets an Error, v1 gets MatchDeleted.
    let mut late = Client::modern(&server, "late");
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
    let mut legacy = Client::legacy(&server, "legacy");
    legacy.send(&Packet::JoinMatch { room_id: 4242 });
    legacy.expect(Packet::MatchDeleted);

    // Room B is untouched.
    host_b.send(&Packet::DeleteMatch { room_id: room_b });
    host_b.assert_silent();
    host_b.create_room("again");
}

#[test]
fn bad_input_does_not_hurt_the_server() {
    let server = start(|config| config.max_frame_len = 1024);
    let mut bystander = Client::legacy(&server, "bystander");

    // Undecodable body: ignored, connection kept.
    let mut client = Client::legacy(&server, "client");
    client.stream.write_all(&[0, 0, 0, 2, 250, 1]).unwrap();
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
    let mut host = Client::modern(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::modern(&server, "guest");
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

    host_udp.send_to(b"voice-frame", server.udp).unwrap();
    assert_eq!(udp_recv(&guest_udp), b"voice-frame");
    udp_assert_silent(&host_udp);

    // Wrong token and the legacy join are rejected by default.
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
fn legacy_udp_join_can_be_enabled() {
    let server = start(|config| config.legacy_udp_join = true);
    let a = udp_join(&server, &Packet::JoinMatch { room_id: 1 });
    expect_acks(&a);
    let b = udp_join(&server, &Packet::JoinMatch { room_id: 1 });
    expect_acks(&b);
    let other_room = udp_join(&server, &Packet::JoinMatch { room_id: 2 });
    expect_acks(&other_room);

    a.send_to(b"hello", server.udp).unwrap();
    assert_eq!(udp_recv(&b), b"hello");
    udp_assert_silent(&other_room);
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
    let mut host = Client::modern(&server, "host");
    let room = host.create_room("r");
    let mut guest = Client::modern(&server, "guest");
    guest.join_room(room, 1);
    host.recv();
    let mut old = Client::legacy(&server, "old");
    old.join_room(room, 2);
    host.recv();
    guest.recv();

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
    // v1 clients never see v2 packets.
    old.assert_silent();

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
    old.recv(); // StartMatch
    let tick = Packet::Game {
        kind: 9,
        payload: vec![],
    };
    host.expect(tick.clone());
    guest.expect(tick);
    old.assert_silent();
}
