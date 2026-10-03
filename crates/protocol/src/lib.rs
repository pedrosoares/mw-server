//! Wire protocol shared by the server and its clients.
//!
//! # TCP framing
//!
//! Every TCP message is a frame: a `u32` big-endian body length followed by a
//! [postcard]-encoded [`Packet`]. Postcard encodes an enum as a varint variant
//! index followed by the fields, so **the order of the [`Packet`] variants is
//! part of the wire format**: only ever append new variants at the end.
//!
//! # Handshake
//!
//! The first packet of every connection must be [`Packet::Hello`] with
//! [`PROTOCOL_VERSION`]; the server answers [`Packet::Welcome`] or closes the
//! connection.
//!
//! See `PROTOCOL.md` at the repository root for the full flow.

pub mod udp;

use std::fmt;
use std::io::{self, Read, Write};

/// Version announced in [`Packet::Hello`] / [`Packet::Welcome`].
pub const PROTOCOL_VERSION: u16 = 2;
/// Size of the big-endian length prefix of a TCP frame.
pub const HEADER_LEN: usize = 4;
/// Default upper bound for a frame body; larger frames close the connection.
pub const DEFAULT_MAX_FRAME_LEN: usize = 64 * 1024;

pub type Vec3 = (f32, f32, f32);

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub enum Packet {
    Ping,
    Disconnect,
    LoginRequest {
        name: String,
    },
    Login {
        id: i32,
        name: String,
    },
    ListMatches,
    RemoveFromListMatches,
    MatchDeleted,
    NewMatch {
        room_name: String,
    },
    DeleteMatch {
        room_id: i32,
    },
    MatchCreated {
        id: i32,
        owner_id: i32,
        room_name: String,
    },
    MatchJoined {
        id: i32,
        user_id: i32,
        user_name: String,
        room_name: String,
    },
    MatchLeaved {
        user_id: i32,
        user_name: String,
    },
    JoinMatch {
        room_id: i32,
    },
    LeaveMatch {
        room_id: i32,
    },
    MatchList {
        /// `(room_id, room_name, player_count)`
        matches: Vec<(i32, String, i32)>,
    },
    StartMatch {
        room_id: i32,
        map: String,
    },
    SpawnPlayers {
        room_id: i32,
        positions: Vec<Vec3>,
    },
    SpawnRemoteObject {
        id: i32,
        object_id: i32,
        position: Vec3,
        rotation: Vec3,
    },
    DespawnRemoteObject {
        id: i32,
        object_id: i32,
    },
    RemoteObjectCall {
        /// Target client when `broadcast` is false.
        id: i32,
        object_id: i32,
        method: String,
        params: Vec<RawPacket>,
        broadcast: bool,
    },
    RemoteObjectLocation {
        id: i32,
        object_id: i32,
        position: Vec3,
        rotation: Vec3,
    },
    Spawn {
        position: Vec3,
    },
    Message {
        id: i32,
        name: String,
        text: String,
    },

    /// Client -> server, mandatory first packet.
    Hello {
        protocol_version: u16,
    },
    /// Server -> client, answer to `Hello`. `udp_token` authenticates `UdpJoin`.
    Welcome {
        protocol_version: u16,
        client_id: i32,
        udp_token: u64,
    },
    /// Server -> client, a request was rejected.
    Error {
        code: ErrorCode,
        message: String,
    },
    /// Game-defined packet, routed by the server's `RoomLogic`.
    Game {
        kind: u16,
        payload: Vec<u8>,
    },
    /// UDP only: first datagram, binds the sender's address to its session.
    UdpJoin {
        token: u64,
    },
    /// Server -> room members: the owner left and `owner_id` (the earliest
    /// remaining member) owns the room now. Only sent by servers running
    /// with host migration.
    OwnerChanged {
        room_id: i32,
        owner_id: i32,
    },
}

/// Discriminant of a [`Packet`], equal to the varint postcard writes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Tag {
    Ping = 0,
    Disconnect,
    LoginRequest,
    Login,
    ListMatches,
    RemoveFromListMatches,
    MatchDeleted,
    NewMatch,
    DeleteMatch,
    MatchCreated,
    MatchJoined,
    MatchLeaved,
    JoinMatch,
    LeaveMatch,
    MatchList,
    StartMatch,
    SpawnPlayers,
    SpawnRemoteObject,
    DespawnRemoteObject,
    RemoteObjectCall,
    RemoteObjectLocation,
    Spawn,
    Message,
    Hello,
    Welcome,
    Error,
    Game,
    UdpJoin,
    OwnerChanged,
}

impl Tag {
    const ALL: [Tag; 29] = [
        Tag::Ping,
        Tag::Disconnect,
        Tag::LoginRequest,
        Tag::Login,
        Tag::ListMatches,
        Tag::RemoveFromListMatches,
        Tag::MatchDeleted,
        Tag::NewMatch,
        Tag::DeleteMatch,
        Tag::MatchCreated,
        Tag::MatchJoined,
        Tag::MatchLeaved,
        Tag::JoinMatch,
        Tag::LeaveMatch,
        Tag::MatchList,
        Tag::StartMatch,
        Tag::SpawnPlayers,
        Tag::SpawnRemoteObject,
        Tag::DespawnRemoteObject,
        Tag::RemoteObjectCall,
        Tag::RemoteObjectLocation,
        Tag::Spawn,
        Tag::Message,
        Tag::Hello,
        Tag::Welcome,
        Tag::Error,
        Tag::Game,
        Tag::UdpJoin,
        Tag::OwnerChanged,
    ];

    pub fn from_index(index: u32) -> Option<Tag> {
        Self::ALL.get(index as usize).copied()
    }

    /// Reads the tag of an encoded packet body without decoding the rest.
    pub fn peek(body: &[u8]) -> Option<Tag> {
        let mut index: u32 = 0;
        for (i, byte) in body.iter().take(5).enumerate() {
            index |= u32::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Self::from_index(index);
            }
        }
        None
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The packet is not allowed in the client's current state.
    InvalidState,
    RoomNotFound,
    RoomFull,
    /// The client is not the owner of the room it tried to manage.
    NotOwner,
    UnsupportedVersion,
    /// The packet could not be decoded.
    Malformed,
    /// The match already started; it can no longer be joined.
    MatchStarted,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub enum RawPacket {
    String(String),
    Int(i32),
    Bool(bool),
    Float(f32),
    Vector3(Vec3),
    Array(Vec<RawPacket>),
    Null,
}

impl Packet {
    pub fn tag(&self) -> Tag {
        match self {
            Packet::Ping => Tag::Ping,
            Packet::Disconnect => Tag::Disconnect,
            Packet::LoginRequest { .. } => Tag::LoginRequest,
            Packet::Login { .. } => Tag::Login,
            Packet::ListMatches => Tag::ListMatches,
            Packet::RemoveFromListMatches => Tag::RemoveFromListMatches,
            Packet::MatchDeleted => Tag::MatchDeleted,
            Packet::NewMatch { .. } => Tag::NewMatch,
            Packet::DeleteMatch { .. } => Tag::DeleteMatch,
            Packet::MatchCreated { .. } => Tag::MatchCreated,
            Packet::MatchJoined { .. } => Tag::MatchJoined,
            Packet::MatchLeaved { .. } => Tag::MatchLeaved,
            Packet::JoinMatch { .. } => Tag::JoinMatch,
            Packet::LeaveMatch { .. } => Tag::LeaveMatch,
            Packet::MatchList { .. } => Tag::MatchList,
            Packet::StartMatch { .. } => Tag::StartMatch,
            Packet::SpawnPlayers { .. } => Tag::SpawnPlayers,
            Packet::SpawnRemoteObject { .. } => Tag::SpawnRemoteObject,
            Packet::DespawnRemoteObject { .. } => Tag::DespawnRemoteObject,
            Packet::RemoteObjectCall { .. } => Tag::RemoteObjectCall,
            Packet::RemoteObjectLocation { .. } => Tag::RemoteObjectLocation,
            Packet::Spawn { .. } => Tag::Spawn,
            Packet::Message { .. } => Tag::Message,
            Packet::Hello { .. } => Tag::Hello,
            Packet::Welcome { .. } => Tag::Welcome,
            Packet::Error { .. } => Tag::Error,
            Packet::Game { .. } => Tag::Game,
            Packet::UdpJoin { .. } => Tag::UdpJoin,
            Packet::OwnerChanged { .. } => Tag::OwnerChanged,
        }
    }

    /// Decodes a packet body (without the length prefix).
    pub fn decode(body: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(body)
    }

    /// Encodes the packet body (without the length prefix), e.g. for UDP.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("Packet is always serializable")
    }

    /// Encodes a complete TCP frame (length prefix + body) in one allocation.
    pub fn encode_frame(&self) -> Vec<u8> {
        let frame = postcard::to_extend(self, vec![0u8; HEADER_LEN])
            .expect("Packet is always serializable");
        let len = (frame.len() - HEADER_LEN) as u32;
        let mut frame = frame;
        frame[..HEADER_LEN].copy_from_slice(&len.to_be_bytes());
        frame
    }
}

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge { len: usize, max: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(err) => write!(f, "io error: {err}"),
            FrameError::TooLarge { len, max } => {
                write!(f, "frame of {len} bytes exceeds limit of {max}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(err: io::Error) -> Self {
        FrameError::Io(err)
    }
}

/// Blocking frame reader for Rust clients and tests. Reads one frame body into
/// `body`, reusing its allocation.
pub fn read_frame<R: Read>(
    reader: &mut R,
    max_len: usize,
    body: &mut Vec<u8>,
) -> Result<(), FrameError> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > max_len {
        return Err(FrameError::TooLarge { len, max: max_len });
    }
    body.resize(len, 0);
    reader.read_exact(body)?;
    Ok(())
}

/// Blocking frame writer for Rust clients and tests.
pub fn write_packet<W: Write>(writer: &mut W, packet: &Packet) -> io::Result<()> {
    writer.write_all(&packet.encode_frame())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One instance of every variant, in declaration order.
    fn samples() -> Vec<Packet> {
        vec![
            Packet::Ping,
            Packet::Disconnect,
            Packet::LoginRequest { name: "ab".into() },
            Packet::Login {
                id: -1,
                name: "ab".into(),
            },
            Packet::ListMatches,
            Packet::RemoveFromListMatches,
            Packet::MatchDeleted,
            Packet::NewMatch {
                room_name: "r".into(),
            },
            Packet::DeleteMatch { room_id: 3 },
            Packet::MatchCreated {
                id: 1,
                owner_id: 2,
                room_name: "r".into(),
            },
            Packet::MatchJoined {
                id: 1,
                user_id: 2,
                user_name: "u".into(),
                room_name: "r".into(),
            },
            Packet::MatchLeaved {
                user_id: 2,
                user_name: "u".into(),
            },
            Packet::JoinMatch { room_id: 300 },
            Packet::LeaveMatch { room_id: 1 },
            Packet::MatchList {
                matches: vec![(1, "r".into(), 2)],
            },
            Packet::StartMatch {
                room_id: 1,
                map: "m".into(),
            },
            Packet::SpawnPlayers {
                room_id: 1,
                positions: vec![(1.0, 2.0, 3.0)],
            },
            Packet::SpawnRemoteObject {
                id: 1,
                object_id: 2,
                position: (1.0, 0.0, 0.0),
                rotation: (0.0, 0.0, 0.5),
            },
            Packet::DespawnRemoteObject {
                id: 1,
                object_id: 2,
            },
            Packet::RemoteObjectCall {
                id: 1,
                object_id: 2,
                method: "f".into(),
                params: vec![
                    RawPacket::Int(5),
                    RawPacket::String("s".into()),
                    RawPacket::Bool(true),
                    RawPacket::Float(1.5),
                    RawPacket::Vector3((1.0, 2.0, 3.0)),
                    RawPacket::Array(vec![RawPacket::Null]),
                ],
                broadcast: true,
            },
            Packet::RemoteObjectLocation {
                id: 1,
                object_id: 2,
                position: (1.0, 2.0, 3.0),
                rotation: (0.0, 0.0, 0.0),
            },
            Packet::Spawn {
                position: (1.0, 2.0, 3.0),
            },
            Packet::Message {
                id: 1,
                name: "n".into(),
                text: "hi".into(),
            },
            Packet::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            Packet::Welcome {
                protocol_version: PROTOCOL_VERSION,
                client_id: 7,
                udp_token: u64::MAX,
            },
            Packet::Error {
                code: ErrorCode::RoomFull,
                message: "full".into(),
            },
            Packet::Game {
                kind: 300,
                payload: vec![1, 2, 3],
            },
            Packet::UdpJoin { token: 42 },
            Packet::OwnerChanged {
                room_id: 1,
                owner_id: 2,
            },
        ]
    }

    /// Frames as produced since commit fd69794. Clients in other languages
    /// (GDScript, the godot-network adapter) depend on these bytes.
    const GOLDEN: [&[u8]; 23] = [
        &[0, 0, 0, 1, 0],
        &[0, 0, 0, 1, 1],
        &[0, 0, 0, 4, 2, 2, 97, 98],
        &[0, 0, 0, 5, 3, 1, 2, 97, 98],
        &[0, 0, 0, 1, 4],
        &[0, 0, 0, 1, 5],
        &[0, 0, 0, 1, 6],
        &[0, 0, 0, 3, 7, 1, 114],
        &[0, 0, 0, 2, 8, 6],
        &[0, 0, 0, 5, 9, 2, 4, 1, 114],
        &[0, 0, 0, 7, 10, 2, 4, 1, 117, 1, 114],
        &[0, 0, 0, 4, 11, 4, 1, 117],
        &[0, 0, 0, 3, 12, 216, 4],
        &[0, 0, 0, 2, 13, 2],
        &[0, 0, 0, 6, 14, 1, 2, 1, 114, 4],
        &[0, 0, 0, 4, 15, 2, 1, 109],
        &[
            0, 0, 0, 15, 16, 2, 1, 0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64,
        ],
        &[
            0, 0, 0, 27, 17, 2, 4, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 63,
        ],
        &[0, 0, 0, 3, 18, 2, 4],
        &[
            0, 0, 0, 35, 19, 2, 4, 1, 102, 6, 1, 10, 0, 1, 115, 2, 1, 3, 0, 0, 192, 63, 4, 0, 0,
            128, 63, 0, 0, 0, 64, 0, 0, 64, 64, 5, 1, 6, 1,
        ],
        &[
            0, 0, 0, 27, 20, 2, 4, 0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0,
        ],
        &[0, 0, 0, 13, 21, 0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64],
        &[0, 0, 0, 7, 22, 2, 1, 110, 2, 104, 105],
    ];

    #[test]
    fn wire_format_is_unchanged() {
        for (packet, golden) in samples().iter().zip(GOLDEN) {
            assert_eq!(packet.encode_frame(), golden, "{packet:?}");
            assert_eq!(&Packet::decode(&golden[HEADER_LEN..]).unwrap(), packet);
        }
    }

    #[test]
    fn tags_match_encoded_discriminant() {
        let samples = samples();
        assert_eq!(
            samples.len(),
            Tag::ALL.len(),
            "add new variants to samples()"
        );
        for (index, packet) in samples.iter().enumerate() {
            let body = packet.encode();
            assert_eq!(packet.tag() as usize, index, "{packet:?}");
            assert_eq!(Tag::peek(&body), Some(packet.tag()), "{packet:?}");
            assert_eq!(&Packet::decode(&body).unwrap(), packet);
        }
    }

    #[test]
    fn peek_rejects_garbage() {
        assert_eq!(Tag::peek(&[]), None);
        assert_eq!(Tag::peek(&[200]), None);
        assert_eq!(Tag::peek(&[0x80, 0x80, 0x80, 0x80, 0x80]), None);
    }

    #[test]
    fn read_frame_enforces_limit() {
        let frame = Packet::LoginRequest {
            name: "x".repeat(100),
        }
        .encode_frame();
        let mut body = Vec::new();
        let err = read_frame(&mut frame.as_slice(), 10, &mut body).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { len: 102, max: 10 }));
        read_frame(&mut frame.as_slice(), 1000, &mut body).unwrap();
        assert_eq!(body, &frame[HEADER_LEN..]);
    }
}
