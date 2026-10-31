//! Cross-language E2E probe: the `rmtt` Rust server from this repository
//! (e2e/rust-server) against the Go / Rust rmtt clients. Every upstream PUSH
//! is logged as `PUSH_FROM_CLIENT device=<id> payload=<text>` and echoed back
//! to the same device as `echo:<text>`, which the client verifies.
//!
//! Prints `E2E_SERVER_READY port=<port>` once bound, then stays alive until
//! the harness kills it.
use std::sync::Arc;
use std::time::Duration;

use rmtt::server::{AuthResult, Server, ServerOptions};

fn main() {
    let port = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "19990".to_string());
    let addr = format!("0.0.0.0:{port}");

    let server = Arc::new(Server::bind(&addr, ServerOptions::default()).expect("bind failed"));
    // Allow any credential and use it as the device id (like the Go server).
    server.set_authenticator(|credential| AuthResult::allow(credential));

    let s = server.clone();
    server.set_message_handler(move |device_id, payload| {
        let text = String::from_utf8_lossy(payload);
        println!("PUSH_FROM_CLIENT device={device_id} payload={text}");
        let echoed = format!("echo:{text}");
        if let Err(e) = s.push(device_id, echoed.as_bytes()) {
            eprintln!("echo push failed for device={device_id}: {e}");
        }
    });

    server.set_on_connect(|device_id| println!("CONNECTED device={device_id}"));
    server.set_on_disconnect(|device_id, reason| {
        println!("DISCONNECTED device={device_id} reason={reason}")
    });

    println!("E2E_SERVER_READY port={port}");
    // Keep the process alive until the harness kills it.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
