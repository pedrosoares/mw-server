use std::{
    io::Read,
    net::TcpStream,
    sync::{Arc, RwLock},
};

use crate::Client;

pub fn move_clients(
    src: &Arc<RwLock<Vec<Client>>>,
    predicate: impl Fn(&Client) -> bool,
) -> Arc<RwLock<Vec<Client>>> {
    let mut src_vec = src.write().unwrap();
    let mut moved = Vec::new();

    let mut i = src_vec.len();
    while i > 0 {
        i -= 1;
        if predicate(&src_vec[i]) {
            let client = src_vec.swap_remove(i); // MOVE out
            moved.push(client); // now in destination
        }
    }

    Arc::new(RwLock::new(moved))
}

fn read_exact_bytes(stream: &mut TcpStream, size: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn read_message(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    // Read length header
    let header = read_exact_bytes(stream, 4)?;
    let len = u32::from_be_bytes(header.try_into().unwrap()) as usize;

    // Read full message body
    read_exact_bytes(stream, len)
}
