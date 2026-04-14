//! Minimal smoke test: connect to the compositor and dump every toplevel
//! event to stdout. Useful for verifying a wlroots-based compositor is
//! actually advertising the protocol.

use zwindows::{spawn, ToplevelEvent};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Some(client) = spawn() else {
        eprintln!("zwindows: compositor does not advertise zwlr_foreign_toplevel_manager_v1");
        std::process::exit(1);
    };

    println!("listening for toplevel events (Ctrl+C to stop)");
    while let Ok(ev) = client.events.recv_blocking() {
        match ev {
            ToplevelEvent::Added(t) => {
                println!("+ id={} app={:?} title={:?}", t.id, t.app_id, t.title);
            }
            ToplevelEvent::Updated(t) => {
                println!(
                    "~ id={} app={:?} title={:?} activated={} minimized={}",
                    t.id, t.app_id, t.title, t.activated, t.minimized
                );
            }
            ToplevelEvent::Removed(id) => {
                println!("- id={id}");
            }
        }
    }
}
