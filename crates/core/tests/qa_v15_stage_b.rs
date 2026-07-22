//! QA — v1.5 Stage B (T17 CKG / T18 Engineering) supplementary regression suite.
//!
//! These tests harden the v1.5B acceptance surface that is *not* covered by the
//! engineer's own unit tests:
//!   1. **Naming-hygiene guard** — `ckg::Symbol` (struct node) and
//!      `chat::Attachment::Symbol` (enum variant) must remain unambiguously
//!      distinct. A future glob `use crate::ckg::*; use crate::chat::*;` or a
//!      re-export collision would make this file fail to compile, catching the
//!      regression at the boundary.
//!   2. **Trait-abstraction guard** — both `InMemoryCkg` (default) and
//!      `PgCkgStore` (optional PG backend) satisfy the `CkgIndex` trait, so the
//!      CKG consumer (`context_manager` / `retrieval`) can swap backends.
//!   3. **Beyond-text-similarity** — `retrieval::retrieve_with_ckg` must surface
//!      a CKG symbol neighbor that has *zero* lexical overlap with the query,
//!      proving the graph goes past pure embedding similarity.
//!
//! All tests are pure / deterministic and run under `cargo test` with no
//! external dependencies (no DB, no `git`/`cargo` spawn).

use ide_core::chat::Attachment;
use ide_core::ckg::{CkgIndex, InMemoryCkg, PgCkgStore, Symbol, SymbolKind};
use ide_core::context_manager::{ContextChunk, ContextSource, Priority};
use ide_core::retrieval::retrieve_with_ckg;

/// Compile-time guard: any `&dyn CkgIndex` is accepted by the consumer path.
fn assert_ckg_index(_: &dyn CkgIndex) {}

#[test]
fn both_backends_satisfy_ckg_index_trait() {
    // Default in-memory backend.
    let mem = InMemoryCkg::new();
    assert_ckg_index(&mem);

    // Optional Postgres backend (builds an in-memory-only store when no DB is
    // configured — must still satisfy the trait so queries keep working).
    let pg = PgCkgStore::new();
    assert_ckg_index(&pg);
}

#[test]
fn ckg_symbol_and_chat_attachment_symbol_are_distinct() {
    // `ckg::Symbol` is a struct node; `chat::Attachment::Symbol` is an enum
    // variant in a *different* module. They must never collide as bare names.
    // We import them under distinct local aliases to prove both types exist and
    // are independently addressable (the v1.5B naming constraint).
    use ide_core::chat::Attachment::Symbol as ChatSymbolVariant;
    use ide_core::ckg::Symbol as CkgSymbol;

    let ckg_sym = CkgSymbol {
        kind: SymbolKind::Fn,
        name: "do_work".to_string(),
        file: "a.rs".to_string(),
        line: 1,
        doc: String::new(),
    };
    let attach = Attachment::Symbol("do_work".to_string());

    assert_eq!(ckg_sym.name, "do_work");
    assert_eq!(ckg_sym.kind, SymbolKind::Fn);
    match attach {
        ChatSymbolVariant(s) => assert_eq!(s, "do_work"),
        other => panic!("expected Attachment::Symbol, got {other:?}"),
    }
}

#[test]
fn ckg_enriches_beyond_text_similarity() {
    // The corpus only contains text about "process"; the code graph binds
    // `process` to a *lexically unrelated* callee (`zzz_unrelated_symbol`).
    // Pure embedding retrieval would never surface it; the CKG must.
    let corpus = vec![ContextChunk::new(
        ContextSource::OpenFile,
        "a.rs",
        "process the user request and dispatch",
        Priority::High,
    )];

    let mut ckg = InMemoryCkg::new();
    ckg.ingest_file(
        "a.rs",
        "fn process() {\n    zzz_unrelated_symbol();\n}\n\
         fn zzz_unrelated_symbol() {}\n",
    );

    let top = retrieve_with_ckg("process", &corpus, 1, &ckg);

    // The CKG neighbor, with no lexical overlap to the query text, must be
    // injected as supplementary context.
    assert!(
        top.iter()
            .any(|t| t.chunk.content.contains("zzz_unrelated_symbol")),
        "CKG must surface symbol neighbors even when lexically dissimilar to the query"
    );
    // The original similarity hit is preserved alongside the graph neighbors.
    assert!(top.iter().any(|t| t.chunk.uri == "a.rs"));
}
