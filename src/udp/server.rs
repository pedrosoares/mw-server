use crossbeam::channel::{Receiver, Sender, unbounded};
use network_types::connection::Packet;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type Clients = Arc<RwLock<HashMap<i32, Vec<SocketAddr>>>>;

#[derive(Clone, Debug)]
struct Message {
    match_id: i32,
    src: SocketAddr,
    data: Vec<u8>,
}

pub fn server(endpoint: String, running: Arc<AtomicBool>) -> std::io::Result<Vec<JoinHandle<()>>> {
    let socket = UdpSocket::bind(endpoint.as_str())?;
    socket.set_nonblocking(true)?;

    println!("UDP server on {}", endpoint);

    let clients: Clients = Arc::new(RwLock::new(HashMap::new()));

    // Channel for dispatching received packets
    let (tx, rx): (Sender<Message>, Receiver<Message>) = unbounded();

    let mut join_handlers: Vec<JoinHandle<()>> = Vec::new();

    //
    // THREAD 1: RECEIVE THREAD (NON-BLOCKING UDP POLLING)
    //
    {
        let socket = socket.try_clone()?;
        let tx = tx.clone();
        let clients = Arc::clone(&clients);

        let server_running = running.clone();
        join_handlers.push(thread::spawn(move || {
            let mut buf = [0u8; 2048];

            let mut cache = HashMap::new();

            while server_running.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buf) {
                    Ok((len, src)) => {
                        if cache.contains_key(&src) {
                            // by pass match
                            // Send message to workers
                            let match_id = cache[&src];
                            tx.send(Message {
                                src,
                                match_id,
                                data: buf[..len].to_vec(),
                            })
                            .ok();
                            continue;
                        }

                        if let Ok(packet) = Packet::try_from(&buf[..len]) {
                            match packet {
                                Packet::JoinMatch { room_id } => {
                                    if !cache.contains_key(&src) {
                                        cache.insert(src, room_id);
                                        let mut list = clients.write().unwrap();

                                        println!("=============================================");
                                        println!("New client: {} on room: {}", src, room_id);
                                        println!("=============================================");
                                        list.entry(room_id)
                                            .or_insert_with(|| Vec::new())
                                            .push(src.clone());
                                    }

                                    let ping = Packet::Ping.serialize();
                                    socket.send_to(ping.as_slice(), src).ok();
                                    socket.send_to(ping.as_slice(), src).ok();
                                    socket.send_to(ping.as_slice(), src).ok();
                                }
                                _ => {
                                    eprintln!("Reach invalid Packet with a unlogged user {}", src);
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // No data, avoid busy loop
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }));
    }

    //
    // THREAD POOL: WORKERS FOR PROCESSING + RELAYING
    //
    let thread_count = 4; // adjust based on CPU cores

    for id in 0..thread_count {
        let socket = socket.try_clone()?;
        let clients = Arc::clone(&clients);
        let rx = rx.clone();

        let current_thread_running = running.clone();
        join_handlers.push(thread::spawn(move || {
            println!("Worker thread {} started.", id);

            while current_thread_running.load(Ordering::Relaxed) {
                if let Ok(msg) = rx.recv() {
                    let targets: Vec<SocketAddr> = {
                        let list = clients.read().unwrap();
                        list[&msg.match_id]
                            .iter()
                            .cloned()
                            .filter(|addr| *addr != msg.src)
                            .collect()
                    };

                    // Relay to all other clients
                    for addr in targets {
                        match socket.send_to(&msg.data, addr) {
                            Ok(_) => continue,
                            // TODO Handle x errors to remove client from room
                            Err(err) => println!("Error sending {:?} for {:?}", err, addr),
                        }
                    }
                }
            }
        }));
    }

    Ok(join_handlers)
}
