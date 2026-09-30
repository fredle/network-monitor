//! Daemon <-> UI transport: a per-user named pipe carrying one JSON value per
//! line. The pipe's default security descriptor only lets the creating user
//! connect with read/write, and the pipe name includes the user name so two
//! signed-in users never talk to each other's daemon.

use crate::engine::Shared;
use crate::model::{Message, Request};
use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions, Stream};
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let clean: String = user.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    format!("NetworkMonitor.ipc.{clean}")
}

/// Requests the daemon main thread must handle (they touch the tray or spawn windows).
pub type UiHook = Arc<dyn Fn(Request) + Send + Sync>;

pub fn serve(shared: Arc<Shared>, updater: Sender<crate::update::Cmd>, ui_hook: UiHook) -> std::io::Result<()> {
    let name = pipe_name().to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new().name(name).create_sync()?;
    std::thread::Builder::new().name("ipc".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let shared = Arc::clone(&shared);
            let updater = updater.clone();
            let hook = Arc::clone(&ui_hook);
            std::thread::spawn(move || handle(conn, shared, updater, hook));
        }
    })?;
    Ok(())
}

fn handle(conn: Stream, shared: Arc<Shared>, updater: Sender<crate::update::Cmd>, hook: UiHook) {
    let (recv, mut send) = conn.split();
    let (tx, rx): (Sender<Message>, Receiver<Message>) = channel();

    // Writer: everything bound for this client goes through one channel so the
    // stream is only ever written from one thread.
    let writer = std::thread::spawn(move || {
        for msg in rx {
            let Ok(mut line) = serde_json::to_string(&msg) else { continue };
            line.push('\n');
            if send.write_all(line.as_bytes()).and_then(|_| send.flush()).is_err() {
                break; // client went away
            }
        }
    });

    for line in BufReader::new(recv).lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Request>(&line) else { continue };
        match req {
            Request::Subscribe => {
                let _ = tx.send(shared.hello());
                shared.subscribe(tx.clone());
            }
            Request::SetSettings(s) => {
                let applied = shared.apply_settings(*s);
                let _ = tx.send(Message::SettingsApplied(Box::new(applied)));
            }
            Request::Reroam => shared.request_reroam(),
            Request::CheckUpdate => {
                let _ = updater.send(crate::update::Cmd::CheckNow);
            }
            Request::ApplyUpdate => {
                let _ = updater.send(crate::update::Cmd::ApplyNow);
            }
            Request::OpenLogFolder => {
                let _ = std::process::Command::new("explorer.exe").arg(crate::paths::log_dir()).spawn();
            }
            other @ (Request::ShowWindow(_) | Request::Quit) => hook(other),
        }
    }
    drop(tx); // lets the writer thread finish once the subscriber entry is pruned
    let _ = writer.join();
}

// ---- client side (UI process) ------------------------------------------------

pub struct Connection {
    pub requests: Sender<Request>,
    pub messages: Receiver<Message>,
}

/// Connect to the daemon and start reader/writer threads. `on_message` is called
/// after each inbound message is queued (used to wake the UI for a repaint).
pub fn connect(on_message: impl Fn() + Send + 'static) -> std::io::Result<Connection> {
    let name = pipe_name().to_ns_name::<GenericNamespaced>()?;
    let stream = Stream::connect(name)?;
    let (recv, mut send) = stream.split();
    let (req_tx, req_rx): (Sender<Request>, Receiver<Request>) = channel();
    let (msg_tx, msg_rx): (Sender<Message>, Receiver<Message>) = channel();

    std::thread::spawn(move || {
        for req in req_rx {
            let Ok(mut line) = serde_json::to_string(&req) else { continue };
            line.push('\n');
            if send.write_all(line.as_bytes()).and_then(|_| send.flush()).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        for line in BufReader::new(recv).lines() {
            let Ok(line) = line else { break };
            if let Ok(msg) = serde_json::from_str::<Message>(&line) {
                if msg_tx.send(msg).is_err() {
                    break;
                }
                on_message();
            }
        }
    });
    Ok(Connection { requests: req_tx, messages: msg_rx })
}

/// One-shot request (used by a second launch to ask the running daemon to show a window).
pub fn send_once(req: Request) -> std::io::Result<()> {
    let name = pipe_name().to_ns_name::<GenericNamespaced>()?;
    let mut s = Stream::connect(name)?;
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    s.flush()
}
