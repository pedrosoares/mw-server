use std::fmt;

/// Connection id, unique for the lifetime of the server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(pub u32);

/// Room (match) id, unique for the lifetime of the server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoomId(pub u32);

macro_rules! wire_id {
    ($ty:ident) => {
        impl $ty {
            /// The id as sent on the wire (`i32`).
            pub fn wire(self) -> i32 {
                self.0 as i32
            }

            /// Parses a wire id; negative values (`-1` means "none") are rejected.
            pub fn from_wire(id: i32) -> Option<Self> {
                u32::try_from(id).ok().map(Self)
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

wire_id!(ClientId);
wire_id!(RoomId);
