//! QA critical-path regression tests for v0.5 Stage A (T05 / T06 / T07).
//!
//! These complement the engineer's in-module unit tests by covering edges that
//! were asserted only implicitly (or not at all):
//!   * T07 — `EditKind` <-> six-bit permission bit mapping, and the Rust enum
//!     string forms vs the `0002_v05.sql` `kind` / `state` CHECK constraints
//!     (verifies the Rust authority and the DDL are in lock-step — item 3/4).
//!   * T07 — the Craft state-machine guards (`mark_pending` is one-way;
//!     `reject` must not roll back an `Applied` proposal; a `Rejected`
//!     proposal is excluded from `CraftSession::pending`).
//!   * T06 — chat `@file`/`@symbol` attachment parsing and multi-turn history
//!     eviction (the rolling window bound by `max_turns`).
//!
//! No external services: pure logic + the `CliHost` in-memory stub only.

use ide_core::chat::{parse_attachments, Attachment, ChatSession};
use ide_core::craft::{CraftEngine, CraftProposal, CraftSession, CraftState, EditKind};
use ide_core::host::{CliHost, HostBridge};
use ide_core::permissions::{Permission, PermissionSet};
use std::sync::Arc;

// --- T07: EditKind <-> six-bit permission + SQL `kind` CHECK alignment (item 3) ---
#[test]
fn editkind_maps_to_correct_permission_bit() {
    assert_eq!(EditKind::FileEdit.required_permission(), Permission::Modify);
    assert_eq!(EditKind::RunCommand.required_permission(), Permission::Execute);
    assert_eq!(EditKind::Commit.required_permission(), Permission::Commit);
}

#[test]
fn editkind_str_matches_sql_kind_check() {
    // 0002_v05.sql: kind CHECK IN ('FileEdit','RunCommand','Commit')
    assert_eq!(EditKind::FileEdit.as_str(), "FileEdit");
    assert_eq!(EditKind::RunCommand.as_str(), "RunCommand");
    assert_eq!(EditKind::Commit.as_str(), "Commit");
}

#[test]
fn craftstate_debug_matches_sql_state_check() {
    // 0002_v05.sql: state CHECK IN ('Suggestion','PendingConfirm','Applied','Rejected')
    let variants = [
        (CraftState::Suggestion, "Suggestion"),
        (CraftState::PendingConfirm, "PendingConfirm"),
        (CraftState::Applied, "Applied"),
        (CraftState::Rejected, "Rejected"),
    ];
    for (state, sql) in variants {
        assert_eq!(format!("{:?}", state), sql);
    }
}

// --- T07: state-machine transitions / guards ---
#[test]
fn mark_pending_is_one_way_from_suggestion() {
    let mut p = CraftProposal::new("p1", "u", "a", "b", "r", EditKind::FileEdit);
    assert_eq!(p.state, CraftState::Suggestion);
    p.mark_pending();
    assert_eq!(p.state, CraftState::PendingConfirm);
    p.mark_pending(); // no-op once no longer in Suggestion
    assert_eq!(p.state, CraftState::PendingConfirm);
}

#[tokio::test]
async fn reject_does_not_override_applied() {
    let host = Arc::new(CliHost::new());
    host.seed("mem://a.rs", "let x = 1;");
    let bridge = Arc::new(HostBridge::new(host));
    let eng = CraftEngine::new(bridge, PermissionSet::all());
    let mut p = eng.propose("mem://a.rs", "let x = 1;", "let x = 2;", "bump", EditKind::FileEdit);
    let st = eng.confirm(&mut p).await.unwrap();
    assert_eq!(st, CraftState::Applied);
    // Guard: a late reject must NOT revert an already-applied proposal.
    CraftEngine::reject(&mut p);
    assert_eq!(p.state, CraftState::Applied);
}

#[tokio::test]
async fn rejected_proposal_excluded_from_pending() {
    let host = Arc::new(CliHost::new());
    host.seed("mem://a.rs", "let x = 1;");
    let bridge = Arc::new(HostBridge::new(host));
    let eng = CraftEngine::new(bridge, PermissionSet::all());
    let mut p = eng.propose("mem://a.rs", "let x = 1;", "let x = 2;", "bump", EditKind::FileEdit);
    CraftEngine::reject(&mut p);
    assert_eq!(p.state, CraftState::Rejected);
    let mut session = CraftSession::new();
    session.add(p);
    // pending() must exclude the rejected proposal (Suggestion | PendingConfirm only).
    assert_eq!(session.pending().len(), 0);
}

// --- T06: chat attachment parsing + multi-turn eviction ---
#[test]
fn chat_parses_file_symbol_raw_attachments() {
    let a = parse_attachments(&[
        "@file:src/main.rs".to_string(),
        "@symbol:retry".to_string(),
        "loose".to_string(),
    ]);
    assert_eq!(
        a,
        vec![
            Attachment::File("src/main.rs".to_string()),
            Attachment::Symbol("retry".to_string()),
            Attachment::Raw("loose".to_string()),
        ]
    );
}

#[test]
fn chat_session_evicts_oldest_beyond_max_turns() {
    let mut s = ChatSession::new(2048, 2);
    s.push_user("one");
    s.push_user("two");
    s.push_user("three");
    assert_eq!(s.history().len(), 2);
    assert!(s.history().iter().any(|t| t.content == "two"));
    assert!(s.history().iter().any(|t| t.content == "three"));
    assert!(!s.history().iter().any(|t| t.content == "one"));
}
