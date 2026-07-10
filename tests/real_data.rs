//! Smoke test against the real transcripts on this machine.
//! Ignored by default (depends on ~/.claude / ~/.codex); run with:
//!   cargo test --test real_data -- --ignored --nocapture

use std::sync::mpsc;
use std::time::Duration;

use space::data::{Backend, humanize_since, now_epoch};
use space::parse::{Load, spawn_loader};

#[test]
#[ignore]
fn loads_and_summarizes_real_sessions() {
    let (tx, rx) = mpsc::channel::<Load>();
    spawn_loader(tx);

    let mut sessions = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Load::Session(s)) => sessions.push(*s),
            Ok(Load::Done) => break,
            Err(_) => break,
        }
    }

    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_activity));
    let now = now_epoch();
    println!("\nloaded {} sessions:", sessions.len());
    for s in &sessions {
        println!(
            "  [{}] {:<10} {:<40} {:<9} #{:<4} {}",
            s.backend.label(),
            s.space,
            s.title.chars().take(40).collect::<String>(),
            humanize_since(s.last_activity, now),
            s.msg_count,
            s.cwd,
        );
    }

    // Every row must have the fields the UI and handoff rely on.
    for s in &sessions {
        assert!(!s.id.is_empty(), "session id must be present");
        assert!(!s.title.is_empty(), "title must be present");
        assert!(matches!(s.backend, Backend::Claude | Backend::Codex));
    }
}
