//! The TCP listener, the connection cap, and one thread for each client.
//!
//! See [FR61], [FR63], and [NFR14]. One thread for each connection is decision
//! D6 in `design.md`: [NFR14] asks for 100 connections, and a disk read blocks
//! the thread anyway.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use protocol::error::ErrorCode;
use protocol::message::ServerMsg;

use crate::config::Config;
use crate::net::cancel::CancelRegistry;
use crate::net::session;

/// The listening socket and the state that every session shares.
pub struct Server {
    listener: TcpListener,
    /// The cap that [NFR14] sets.
    max_connections: usize,
    /// The id of the next connection. The first connection gets 1.
    next_conn_id: AtomicU64,
    /// The count of live connections.
    live: Arc<AtomicUsize>,
    cancels: Arc<CancelRegistry>,
}

impl Server {
    /// Binds the port of the config. Port 0 takes any free port, which
    /// [`Server::local_addr`] then reports.
    pub fn bind(config: &Config) -> io::Result<Server> {
        Ok(Server {
            listener: TcpListener::bind(("0.0.0.0", config.port))?,
            max_connections: config.max_connections,
            next_conn_id: AtomicU64::new(1),
            live: Arc::new(AtomicUsize::new(0)),
            cancels: Arc::new(CancelRegistry::new()),
        })
    }

    /// The address of the bound socket, with the port that the operating
    /// system chose.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts connections until the listener stops. See [FR61] and [FR63].
    pub fn run(self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => self.start(stream),
                // One failed accept leaves the socket open, so the loop goes on.
                Err(e) => log::warn!("accept failed: {e}"),
            }
        }
    }

    /// Takes a slot and starts the session thread. Past the cap the connection
    /// gets one error and closes.
    fn start(&self, stream: TcpStream) {
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
        let peer = match stream.peer_addr() {
            Ok(addr) => addr.to_string(),
            Err(e) => format!("an unknown address ({e})"),
        };
        let slot = Slot::take(&self.live);
        if slot.live > self.max_connections {
            refuse(&stream, self.max_connections, &peer);
            return;
        }
        log::info!("connection {conn_id} from {peer}");
        let cancels = Arc::clone(&self.cancels);
        // `slot` moves into the thread, so a panicking session frees it too.
        std::thread::spawn(move || {
            session::serve(&stream, conn_id, &cancels);
            drop(slot);
        });
    }
}

/// One slot of the connection cap. The count falls when the slot drops.
struct Slot {
    /// The count of live connections that this slot makes.
    live: usize,
    count: Arc<AtomicUsize>,
}

impl Slot {
    fn take(count: &Arc<AtomicUsize>) -> Slot {
        Slot {
            live: count.fetch_add(1, Ordering::SeqCst) + 1,
            count: Arc::clone(count),
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Writes one error and closes. The code says "retry later", not "the disk is
/// full", so a client can tell the two apart. See [NFR14].
fn refuse(stream: &TcpStream, max: usize, peer: &str) {
    log::warn!("refused {peer}: the server holds {max} connections");
    let msg = ServerMsg::Error {
        code: ErrorCode::TooManyConnections,
        message: format!("the server holds {max} connections, try again later"),
        position: None,
    };
    let _ = session::send(stream, &msg);
}
