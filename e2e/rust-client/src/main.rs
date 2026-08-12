//! Cross-language E2E probe: the `rmtt` Rust client from this repository
//! against the Java rmtt server (e2e/java-server). It connects over TCP,
//! pushes a message upstream and verifies the server's echo comes back
//! downstream. Exit 0 on success.
use rmtt::{Client, ClientOptions};
use std::sync::mpsc;
use std::time::Duration;

fn main() {
    let port = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "19990".to_string());
    let addr = format!("tcp://127.0.0.1:{}", port);

    let mut opts = ClientOptions::default();
    opts.credential = "rust-e2e".to_string();
    opts.heartbeat_seconds = 30;

    let client = Client::connect(&addr, opts).expect("connect failed");
    println!("RUST_CLIENT_CONNECTED");

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    client.set_payload_handler(move |msg| {
        let _ = tx.send(msg.payload().to_vec());
    });

    client.push(b"ping-from-rust").expect("push failed");

    let echo = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("timeout waiting for server echo");
    if echo != b"echo:ping-from-rust" {
        eprintln!("unexpected echo: {:?}", String::from_utf8_lossy(&echo));
        std::process::exit(1);
    }
    println!("RUST_CLIENT_ECHO_OK {}", String::from_utf8_lossy(&echo));
    client.disconnect();
    println!("RUST_CLIENT_E2E_PASS");
}
