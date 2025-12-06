use std::error::Error;

#[derive(serde::Serialize, serde::Deserialize, Debug)]
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
        matches: Vec<(i32, String, i32)>,
    },
    StartMatch {
        room_id: i32,
        map: String,
    },
    SpawnPlayers {
        room_id: i32,
        positions: Vec<(f32, f32, f32)>,
    },
    SpawnRemoteObject {
        id: i32,
        object_id: i32,
        position: (f32, f32, f32),
        rotation: (f32, f32, f32),
    },
    DespawnRemoteObject {
        id: i32,
        object_id: i32,
    },
    RemoteObjectCall {
        id: i32,
        object_id: i32,
        method: String,
        params: Vec<RawPacket>,
        broadcast: bool,
    },
    RemoteObjectLocation {
        id: i32,
        object_id: i32,
        position: (f32, f32, f32),
        rotation: (f32, f32, f32),
    },
    Spawn {
        position: (f32, f32, f32),
    },
    Message {
        id: i32,
        name: String,
        text: String,
    },
}

impl Packet {
    pub fn from(buffer: &[u8]) -> Self {
        postcard::from_bytes(buffer).unwrap()
    }

    pub fn try_from(buffer: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(buffer)
    }

    pub fn serialize(&self) -> Vec<u8> {
        postcard::to_stdvec(self).unwrap()
    }

    pub fn serialize_with_header(&self) -> Vec<u8> {
        let message = postcard::to_stdvec(self).unwrap();

        let len = message.len() as u32;
        let mut buf = Vec::with_capacity(4 + message.len());

        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&message[..]);

        return buf;
    }
}

// TODO do it latter
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub enum RawPacket {
    String(String),
    Int(i32),
    Bool(bool),
    Float(f32),
    Vector3((f32, f32, f32)),
    Array(Vec<RawPacket>),
    Null,
}
