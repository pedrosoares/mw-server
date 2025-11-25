use std::io::{Read, Write};
use std::net::TcpListener;
// use std::sync::atomic::{AtomicBool, Ordering};
// use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
// use std::thread::{self, JoinHandle};

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::thread;
// use std::sync::Mutex;

// pub struct TcpClient {
//     rx: Receiver<Vec<u8>>,
//     running: Arc<AtomicBool>,
//     stream: TcpStream,
//     reader_handler: JoinHandle<std::io::Result<()>>,
// }

// impl TcpClient {
//     pub fn new(mut stream: TcpStream) -> std::io::Result<Self> {
//         let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
//         let running = Arc::new(AtomicBool::new(false));
//         Ok(Self {
//             rx,
//             running: running.clone(),
//             stream: stream.try_clone()?,
//             reader_handler: thread::spawn(move || -> std::io::Result<()> {
//                 while running.load(Ordering::Relaxed) {
//                     let mut buffer = [0u8; 1024];
//                     let bytes_read = stream.read(&mut buffer)?;
//                     tx.send(buffer.to_vec()).unwrap();
//                     let reply = String::from_utf8_lossy(&buffer[..bytes_read]);
//                     println!("Server replied: {}", reply);
//                 }
//                 Ok(())
//             }),
//         })
//     }

//     pub fn join(&mut self) {
//         // self.reader_handler.join().unwrap().unwrap();
//     }
// }

// pub struct TcpServer {
//     running: Arc<AtomicBool>,
//     clients: Vec<TcpClient>,
//     handler: Option<JoinHandle<()>>,
// }

// impl TcpServer {
//     pub fn new() -> Self {
//         // let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
//         Self {
//             running: Arc::new(AtomicBool::new(false)),
//             clients: Vec::new(),
//             handler: None,
//         }
//     }

//     pub fn start(&mut self, remote: String) -> std::io::Result<()> {
//         self.running.store(true, Ordering::Relaxed);

//         self.handler = Some(thread::spawn(move || {
//             let listener = TcpListener::bind(remote.as_str()).unwrap();
//             println!("Connected to server.");
//             for stream in listener.incoming() {
//                 let stream = stream.unwrap();
//                 self.clients.push(TcpClient::new(stream).unwrap());
//             }
//         }));

//         Ok(())
//     }
// }

// pub fn server() -> std::io::Result<()> {
//     let listener = TcpListener::bind("127.0.0.1:7878")?;
//     println!("Server listening on port 7878");

//     for stream in listener.incoming() {
//         let mut stream = stream?;
//         println!("Client connected: {:?}", stream.peer_addr()?);

//         let mut buffer = [0u8; 1024];
//         let bytes_read = stream.read(&mut buffer)?;

//         if bytes_read == 0 {
//             println!("Client disconnected");
//             continue;
//         }

//         let received = String::from_utf8_lossy(&buffer[..bytes_read]);
//         println!("Received: {}", received);

//         // Echo back to the client
//         stream.write_all(b"Message received by server")?;
//     }

//     Ok(())
// }

type Clients = Arc<RwLock<Vec<TcpStream>>>;

fn handle_client(stream: TcpStream, clients: Clients) {
    let peer_addr = stream.peer_addr().unwrap();
    // let reader = BufReader::new(stream.try_clone().unwrap());

    let mut stream = stream.try_clone().unwrap();

    loop {
        let mut buffer = [0u8; 2048];
        if let Ok(bytes_read) = stream.read(&mut buffer) {
            if bytes_read > 0 {
                // let msg = String::from_utf8_lossy(&buffer[..bytes_read]);
                // println!("{} says: {}", peer_addr, msg);

                // Take a snapshot of the clients under a read lock
                let clients_snapshot = {
                    let clients_guard = clients.read().unwrap();
                    clients_guard
                        .iter()
                        .filter(|c| c.peer_addr().unwrap() != peer_addr)
                        .map(|c| c.try_clone().unwrap())
                        .collect::<Vec<_>>()
                };

                // Send the message outside of the lock
                for mut client in clients_snapshot {
                    client.write(&buffer[..bytes_read]).unwrap();
                    // let _ = write!(client, "{}", msg);
                }
            }
        } else {
            break;
        }
    }

    // Remove client after disconnect
    let mut clients_guard = clients.write().unwrap();
    clients_guard.retain(|c| c.peer_addr().unwrap() != peer_addr);
    println!("Client {} disconnected", peer_addr);
}

struct Room {
    master_id: i32,
    room_name: String,
    started: bool,
    clients: Clients,
}

pub fn server() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:7878")?;
    println!("Server listening on 127.0.0.1:7878");

    let clients: Clients = Arc::new(RwLock::new(Vec::new()));
    let rooms: Arc<RwLock<Vec<Room>>> = Arc::new(RwLock::new(Vec::new()));

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                println!("Client {} connected", stream.peer_addr().unwrap());

                let mut clients_guard = clients.write().unwrap();

                let user_id: i32 = clients_guard.len() as i32;
                stream.write(&user_id.to_be_bytes()).unwrap();

                // Tell everyone that there is a new User
                clients_guard.iter().enumerate().for_each(|(id, mut c)| {
                    let buf = [vec![51 as u8], user_id.to_be_bytes().to_vec()].concat();
                    c.write(&buf).unwrap();
                    // Tell the User about the other Users
                    let buf = [vec![51 as u8], (id as i32).to_be_bytes().to_vec()].concat();
                    stream.write(&buf).unwrap();
                });
                // Add client to shared list
                clients_guard.push(stream.try_clone().unwrap());

                let clients_clone = Arc::clone(&clients);
                thread::spawn(move || handle_client(stream, clients_clone));
            }
            Err(e) => eprintln!("Connection failed: {}", e),
        }
    }

    Ok(())
}
