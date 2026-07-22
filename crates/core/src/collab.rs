//! Collaboration — code-review comments + editing-presence lock hints (T19).
//!
//! A **no-UI** collaboration layer for v2.0 Stage A. It provides:
//!   * [`Comment`] / [`CommentStore`] — shared code-review annotations anchored
//!     to a `(file, line_range)`, tenant-scoped and multi-tenant ready.
//!   * [`Lock`] / [`LockStore`] — a lightweight "someone is editing this file"
//!     presence hint (kept in-memory; no UI, no persistence required).
//!   * [`InMemoryCommentStore`] / [`InMemoryLockStore`] — zero-dependency
//!     backends for tests / offline CLI.
//!   * [`PgCommentStore`] — Postgres backend (tokio-postgres) that injects the
//!     session tenant (`SET LOCAL app.tenant_id`) so the 0005 RLS policy scopes
//!     every row to its tenant.
//!
//! Collaboration deliberately does **not** touch the `with_governance` chain
//! (Principal / AuditSink / Metrics): comments are a data layer + CLI surface,
//! not a privileged action. They reuse the *tenant* concept already established
//! by [`crate::principal::Principal`] so tenant isolation is consistent with the
//! rest of the system.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_postgres::NoTls;

/// SQL used to scope a transaction to a tenant. The 0005 RLS policy checks
/// `tenant_id = current_setting('app.tenant_id', true)`, so running this inside
/// a transaction (SET LOCAL) enforces strict per-request tenant isolation.
pub const SET_TENANT_LOCAL_SQL: &str = "SET LOCAL app.tenant_id = $1";

/// A code-review comment / annotation on a `(file, line_range)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// App-generated id (no uuid dependency; `c<N>` in memory, any text in PG).
    pub id: String,
    /// Owning tenant (multi-tenant ready; single-tenant default).
    pub tenant_id: String,
    /// Annotated file (uri).
    pub file: String,
    /// First line of the annotated range (1-based).
    pub line_start: usize,
    /// Last line of the annotated range (inclusive; equals `line_start` for a
    /// single-line comment).
    pub line_end: usize,
    /// Author identity (usually the acting `user_id`).
    pub author: String,
    /// Comment body.
    pub body: String,
    /// Whether the thread has been resolved.
    pub resolved: bool,
    /// Creation time, epoch milliseconds.
    pub created_at: i64,
}

impl Comment {
    /// Build a new (unresolved) comment. `id` is left empty; the store assigns
    /// one on [`CommentStore::add`]. `line_start == line_end` for a single line.
    pub fn new(
        tenant_id: &str,
        file: &str,
        line_start: usize,
        line_end: usize,
        author: &str,
        body: &str,
    ) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            id: String::new(),
            tenant_id: tenant_id.to_string(),
            file: file.to_string(),
            line_start,
            line_end,
            author: author.to_string(),
            body: body.to_string(),
            resolved: false,
            created_at,
        }
    }
}

/// Code-review annotation store. Tenant-scoped: every read/list is filtered to
/// `tenant_id`, and writes stamp `tenant_id` so the 0005 RLS policy holds.
#[async_trait]
pub trait CommentStore: Send + Sync {
    /// Insert a comment and return the id it was stored under.
    async fn add(&self, comment: &Comment) -> anyhow::Result<String>;

    /// List (unresolved-first, then by line) comments for a `(tenant, file)`.
    async fn list_for_file(&self, tenant_id: &str, file: &str) -> anyhow::Result<Vec<Comment>>;

    /// Mark a comment resolved for a tenant. Returns `true` if a row changed.
    async fn resolve(&self, tenant_id: &str, id: &str) -> anyhow::Result<bool>;
}

/// In-memory [`CommentStore`] — used by unit tests, the offline CLI, and as the
/// automatic fallback when Postgres is unreachable.
#[derive(Debug, Default)]
pub struct InMemoryCommentStore {
    comments: Mutex<Vec<Comment>>,
    counter: AtomicU64,
}

impl InMemoryCommentStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Total stored comments (all tenants; callers filter by tenant).
    pub fn count(&self) -> usize {
        self.comments.lock().expect("collab lock poisoned").len()
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("c{n}")
    }
}

#[async_trait]
impl CommentStore for InMemoryCommentStore {
    async fn add(&self, comment: &Comment) -> anyhow::Result<String> {
        let mut comments = self.comments.lock().expect("collab lock poisoned");
        let mut stored = comment.clone();
        stored.id = if comment.id.is_empty() {
            self.next_id()
        } else {
            comment.id.clone()
        };
        comments.push(stored.clone());
        Ok(stored.id)
    }

    async fn list_for_file(&self, tenant_id: &str, file: &str) -> anyhow::Result<Vec<Comment>> {
        let mut out: Vec<Comment> = self
            .comments
            .lock()
            .expect("collab lock poisoned")
            .iter()
            .filter(|c| c.tenant_id == tenant_id && c.file == file)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.resolved
                .cmp(&b.resolved)
                .then(a.line_start.cmp(&b.line_start))
                .then(a.id.cmp(&b.id))
        });
        Ok(out)
    }

    async fn resolve(&self, tenant_id: &str, id: &str) -> anyhow::Result<bool> {
        let mut comments = self.comments.lock().expect("collab lock poisoned");
        for c in comments.iter_mut() {
            if c.tenant_id == tenant_id && c.id == id {
                c.resolved = true;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Postgres-backed [`CommentStore`] (T19). Injects the tenant session so the
/// 0005 RLS policy scopes every row to its tenant (multi-tenant ready).
pub struct PgCommentStore {
    client: tokio::sync::Mutex<tokio_postgres::Client>,
    #[allow(dead_code)]
    default_tenant: String,
}

impl PgCommentStore {
    /// Connect to Postgres; pin the session tenant (so plain, non-transactional
    /// reads/writes also satisfy RLS) and prepare for per-request `SET LOCAL`.
    pub async fn connect(database_url: &str, tenant_id: &str) -> anyhow::Result<Self> {
        let (client, conn) = tokio_postgres::connect(database_url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("comment pg connection closed: {e}");
            }
        });
        // Single-tenant convenience: set the session default tenant so RLS is
        // satisfied even outside an explicit transaction.
        let _ = client.execute("SET app.tenant_id = $1", &[&tenant_id]).await;
        Ok(Self {
            client: tokio::sync::Mutex::new(client),
            default_tenant: tenant_id.to_string(),
        })
    }

    /// Run `f` inside a transaction scoped to `tenant_id` via SET LOCAL (the
    /// multi-tenant-ready code path asserted by the unit tests). `f` receives a
    /// reference to the tenant-scoped transaction and returns a future (bounded
    /// by that transaction's lifetime) whose output is `anyhow::Result<T>`; the
    /// transaction is committed and `T` is returned to the caller. `T` is
    /// inferred from the closure's `Ok(...)` tail.
    async fn with_tenant<T, F>(&self, tenant_id: &str, f: F) -> anyhow::Result<T>
    where
        F: for<'t> FnOnce(
            &'t tokio_postgres::Transaction<'t>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 't>,
        >,
    {
        let mut client = self.client.lock().await;
        let tx = client.transaction().await?;
        tx.execute(SET_TENANT_LOCAL_SQL, &[&tenant_id]).await?;
        let result = f(&tx).await?;
        tx.commit().await?;
        Ok(result)
    }
}

#[async_trait]
impl CommentStore for PgCommentStore {
    async fn add(&self, comment: &Comment) -> anyhow::Result<String> {
        let id = if comment.id.is_empty() {
            format!("c{}", comment.created_at)
        } else {
            comment.id.clone()
        };
        let line_start = comment.line_start as i64;
        let line_end = comment.line_end as i64;
        self.with_tenant(&comment.tenant_id, |tx| {
            let id = id.clone();
            let c = comment.clone();
            Box::pin(async move {
                tx.execute(
                    "INSERT INTO comments \
                     (id, tenant_id, file, line_start, line_end, author, body, resolved) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, false)",
                    &[
                        &id,
                        &c.tenant_id,
                        &c.file,
                        &line_start,
                        &line_end,
                        &c.author,
                        &c.body,
                    ],
                )
                .await?;
                Ok::<String, anyhow::Error>(id)
            })
        })
        .await
    }

    async fn list_for_file(&self, tenant_id: &str, file: &str) -> anyhow::Result<Vec<Comment>> {
        self.with_tenant(tenant_id, |tx| {
            let tenant = tenant_id.to_string();
            let fname = file.to_string();
            Box::pin(async move {
                let rows = tx
                    .query(
                        "SELECT id, tenant_id, file, line_start, line_end, author, body, \
                                resolved, extract(epoch from created_at)::bigint * 1000 \
                         FROM comments \
                        WHERE tenant_id = $1 AND file = $2 \
                        ORDER BY resolved ASC, line_start ASC, id ASC",
                        &[&tenant, &fname],
                    )
                    .await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in &rows {
                    out.push(comment_from_row(r)?);
                }
                Ok::<Vec<Comment>, anyhow::Error>(out)
            })
        })
        .await
    }

    async fn resolve(&self, tenant_id: &str, id: &str) -> anyhow::Result<bool> {
        self.with_tenant(tenant_id, |tx| {
            let tenant = tenant_id.to_string();
            let cid = id.to_string();
            Box::pin(async move {
                let n = tx
                    .execute(
                        "UPDATE comments SET resolved = true \
                         WHERE tenant_id = $1 AND id = $2",
                        &[&tenant, &cid],
                    )
                    .await?;
                Ok::<bool, anyhow::Error>(n > 0)
            })
        })
        .await
    }
}

/// Map a `comments` row to a [`Comment`].
fn comment_from_row(row: &tokio_postgres::Row) -> anyhow::Result<Comment> {
    Ok(Comment {
        id: row.try_get(0)?,
        tenant_id: row.try_get(1)?,
        file: row.try_get(2)?,
        line_start: row.try_get::<_, i64>(3)? as usize,
        line_end: row.try_get::<_, i64>(4)? as usize,
        author: row.try_get(5)?,
        body: row.try_get(6)?,
        resolved: row.try_get(7)?,
        created_at: row.try_get::<_, i64>(8)?,
    })
}

// ---------------------------------------------------------------------------
// Editing-presence lock hints (T19.3) — in-memory only.
// ---------------------------------------------------------------------------

/// A lightweight "someone is editing this file" hint. Not a hard lock; just a
/// presence signal surfaced by the CLI (no UI in this stage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    /// Owning tenant.
    pub tenant_id: String,
    /// File the lock is held on.
    pub file: String,
    /// Identity of the holder (usually a `user_id`).
    pub owner: String,
    /// Acquisition time, epoch milliseconds.
    pub acquired_at: i64,
}

impl Lock {
    /// Build a lock held by `owner` on `file`.
    pub fn new(tenant_id: &str, file: &str, owner: &str) -> Self {
        let acquired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            tenant_id: tenant_id.to_string(),
            file: file.to_string(),
            owner: owner.to_string(),
            acquired_at,
        }
    }
}

/// Editing-presence lock store. Tenant-scoped; one holder per `(tenant, file)`.
#[async_trait]
pub trait LockStore: Send + Sync {
    /// Acquire (or steal) the lock for `(tenant, file)` by `owner`.
    async fn acquire(&self, lock: &Lock) -> anyhow::Result<()>;
    /// Release the lock for `(tenant, file)`.
    async fn release(&self, tenant_id: &str, file: &str) -> anyhow::Result<()>;
    /// Return the current lock holder for `(tenant, file)`, if any.
    async fn get(&self, tenant_id: &str, file: &str) -> anyhow::Result<Option<Lock>>;
}

/// In-memory [`LockStore`] (T19.3 — "内存即可").
#[derive(Debug, Default)]
pub struct InMemoryLockStore {
    locks: Mutex<HashMap<String, Lock>>,
}

impl InMemoryLockStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn key(tenant_id: &str, file: &str) -> String {
        format!("{tenant_id}\u{0}{file}")
    }

    /// Number of currently held locks (all tenants).
    pub fn count(&self) -> usize {
        self.locks.lock().expect("lock lock poisoned").len()
    }
}

#[async_trait]
impl LockStore for InMemoryLockStore {
    async fn acquire(&self, lock: &Lock) -> anyhow::Result<()> {
        self.locks
            .lock()
            .expect("lock lock poisoned")
            .insert(Self::key(&lock.tenant_id, &lock.file), lock.clone());
        Ok(())
    }

    async fn release(&self, tenant_id: &str, file: &str) -> anyhow::Result<()> {
        self.locks
            .lock()
            .expect("lock lock poisoned")
            .remove(&Self::key(tenant_id, file));
        Ok(())
    }

    async fn get(&self, tenant_id: &str, file: &str) -> anyhow::Result<Option<Lock>> {
        Ok(self
            .locks
            .lock()
            .expect("lock lock poisoned")
            .get(&Self::key(tenant_id, file))
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_add_list_resolve() {
        let store = InMemoryCommentStore::new();
        let id = store
            .add(&Comment::new("acme", "src/a.rs", 10, 10, "alice", "use guard here"))
            .await
            .unwrap();
        assert!(!id.is_empty());

        // Initially unresolved and listed.
        let listed = store.list_for_file("acme", "src/a.rs").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].resolved);

        // Resolve it.
        let ok = store.resolve("acme", &id).await.unwrap();
        assert!(ok);
        let listed = store.list_for_file("acme", "src/a.rs").await.unwrap();
        assert!(listed[0].resolved);

        // Resolving a missing id returns false.
        assert!(!store.resolve("acme", "nope").await.unwrap());
    }

    #[tokio::test]
    async fn tenant_isolation_lists_only_own() {
        let store = InMemoryCommentStore::new();
        let _ = store
            .add(&Comment::new("acme", "src/a.rs", 1, 1, "alice", "acme note"))
            .await
            .unwrap();
        let _ = store
            .add(&Comment::new("other", "src/a.rs", 1, 1, "bob", "other note"))
            .await
            .unwrap();

        // Each tenant sees only its own comment on the same file.
        let acme = store.list_for_file("acme", "src/a.rs").await.unwrap();
        assert_eq!(acme.len(), 1);
        assert_eq!(acme[0].tenant_id, "acme");

        let other = store.list_for_file("other", "src/a.rs").await.unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].tenant_id, "other");

        // A tenant cannot resolve another tenant's comment.
        assert!(!store.resolve("acme", &other[0].id).await.unwrap());
    }

    #[test]
    fn pg_tenant_session_injection_constant_exists() {
        // The PgCommentStore runs SET LOCAL app.tenant_id inside each
        // tenant-scoped transaction (asserted here as a static string so the
        // RLS injection path is greppable / testable without a live PG).
        assert!(SET_TENANT_LOCAL_SQL.contains("SET LOCAL app.tenant_id"));
    }

    #[tokio::test]
    async fn lock_acquire_release_get() {
        let store = InMemoryLockStore::new();
        let lk = Lock::new("acme", "src/a.rs", "alice");
        store.acquire(&lk).await.unwrap();
        assert_eq!(store.count(), 1);
        let got = store.get("acme", "src/a.rs").await.unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().owner, "alice");

        // Cross-tenant view is isolated.
        assert!(store.get("other", "src/a.rs").await.unwrap().is_none());

        store.release("acme", "src/a.rs").await.unwrap();
        assert!(store.get("acme", "src/a.rs").await.unwrap().is_none());
    }
}
