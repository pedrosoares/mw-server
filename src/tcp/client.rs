use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

pub struct TcpClient {
    running: Arc<AtomicBool>,
    tx: Sender<Vec<f32>>,
    rx: Receiver<Vec<f32>>,
    writer_handler: Option<JoinHandle<std::io::Result<()>>>,
    reader_handler: Option<JoinHandle<std::io::Result<()>>>,
}

impl TcpClient {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
        Self {
            running: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            writer_handler: None,
            reader_handler: None,
        }
    }

    pub fn connect(&mut self, remote: String) -> std::io::Result<()> {
        let mut stream = TcpStream::connect(remote.as_str())?;
        println!("Connected to server.");

        self.running.store(true, Ordering::Relaxed);

        let mut reader = stream.try_clone()?;
        let mut writer = stream;

        let writer_running = self.running.clone();
        self.writer_handler = Some(thread::spawn(move || -> std::io::Result<()> {
            let mut i = 0;
            while writer_running.load(Ordering::Relaxed) {
                let message = format!("Hello from the client! {}\n", i);
                writer.write_all(message.as_bytes())?;
                i = i + 1;
            }
            Ok(())
        }));

        let reader_running = self.running.clone();
        self.reader_handler = Some(thread::spawn(move || -> std::io::Result<()> {
            while reader_running.load(Ordering::Relaxed) {
                // Read server response
                let mut buffer = [0u8; 1024];
                let bytes_read = reader.read(&mut buffer)?;
                let reply = String::from_utf8_lossy(&buffer[..bytes_read]);
                println!("Server replied: {}", reply);
            }
            Ok(())
        }));

        Ok(())
    }

    pub fn join(&mut self) {
        if let Some(writer) = self.writer_handler.take() {
            writer.join().unwrap().unwrap();
        }
        if let Some(reader) = self.reader_handler.take() {
            reader.join().unwrap().unwrap();
        }
    }
}

pub fn client() -> std::io::Result<()> {
    let mut stream = TcpStream::connect("127.0.0.1:7878")?;
    println!("Connected to server.");

    // Send a message
    stream.write_all(b"Hello from the client!")?;
    println!("Sent message to server.");

    Ok(())
}
