//! Trust/enforcement policy lookups: whether a domain is enforced, whether it
//! redacts PII on ingest, and per-identity domain access permissions.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::Store;

impl Store {
    /// Whether a domain has the enforced flag set. Unknown domains are treated
    /// as not enforced (default 0), matching the column's DEFAULT 0.
    pub fn domain_enforced(&self, name: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT enforced FROM domains WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
            != 0)
    }

    /// Whether a domain redacts PII from title/body/chunk text before storage.
    /// Unknown domains fail safe (redact), matching the column's DEFAULT 1 —
    /// unlike `domain_enforced`, which fails open (not enforced) by design.
    pub fn domain_redact_enabled(&self, name: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT redact_enabled FROM domains WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(1)
            != 0)
    }

    pub fn domain_allowed(&self, identity: Option<&str>, domain: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        let configured: i64 = c.query_row(
            "SELECT COUNT(*) FROM domain_permissions WHERE domain=?1",
            params![domain],
            |r| r.get(0),
        )?;
        if configured == 0 {
            return Ok(true);
        }
        let id = identity.unwrap_or("default");
        Ok(c.query_row(
            "SELECT allowed FROM domain_permissions WHERE identity=?1 AND domain=?2",
            params![id, domain],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
            != 0)
    }

    pub fn set_domain_permission(&self, identity: &str, domain: &str, allowed: bool) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO domain_permissions(identity,domain,allowed) VALUES(?1,?2,?3) ON CONFLICT(identity,domain) DO UPDATE SET allowed=excluded.allowed", params![identity, domain, allowed as i64])?;
        Ok(())
    }
}
