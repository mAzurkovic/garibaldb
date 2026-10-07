//! The `garibaldb-server` binary. See `docs/design.md`.

mod catalog;
mod config;
mod exec;
mod logging;
mod net;
mod sql;
mod store;

use config::Config;
use net::listener::Server;

/// Reads the command line, starts the logger, then serves until the listener
/// fails. A bad flag stops with code 2, and a port that will not bind with 1.
fn main() {
    let config = match Config::from_args() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("garibaldb-server: {e}");
            std::process::exit(2);
        }
    };

    if let Err(e) = logging::init() {
        eprintln!("garibaldb-server: {e}");
        std::process::exit(2);
    }

    let server = match Server::bind(&config) {
        Ok(server) => server,
        Err(e) => {
            log::error!("cannot bind port {}: {e}", config.port);
            std::process::exit(1);
        }
    };

    // The bound port, which is not `config.port` when the flag asks for 0.
    let port = match server.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            log::error!("cannot read the local address: {e}");
            std::process::exit(1);
        }
    };

    // [FR83] for this milestone. Recovery arrives in milestone 9.
    log::info!(
        "garibaldb-server listening on port {port}, data in {}",
        config.data_dir.display()
    );
    server.run();
    log::info!("garibaldb-server stopped");
}
