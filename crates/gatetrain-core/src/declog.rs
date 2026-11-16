//! The decision log (design 6.8): append-only, hash-chained.
//! `hash = sha256(prev_hash + "\n" + canonical(body))`, body = the entry without `hash`; the first `prev_hash` is 64 zeros.
//!
//! The crate `libro` was evaluated and not used (GPL-3.0-only); see docs/design/adr-001-decision-log.md.
use crate::canon::canonical;
use crate::ledger::Ledger;
use crate::Result;
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// What a caller supplies; `seq`, `prev_hash` and `hash` are the log's job.
#[derive(Debug, Clone, Default)]
pub struct NewEntry {
    pub at: String,
    pub actor: String,
    pub kind: String,
    pub batch: Option<String>,
    pub subject: Option<String>,
    pub rule: Option<String>,
    pub policy_hash: Option<String>,
    pub evidence: Option<Value>,
    pub outcome: String,
    pub reason: String,
    pub signature: Option<String>,
}

/// The body that is hashed: every field except `hash`, nulls kept.
pub fn body(seq: i64, e: &NewEntry, prev_hash: &str) -> Value {
    json!({
        "seq": seq, "at": e.at, "actor": e.actor, "kind": e.kind, "batch": e.batch, "subject": e.subject,
        "rule": e.rule, "policy_hash": e.policy_hash, "evidence": e.evidence, "outcome": e.outcome,
        "reason": e.reason, "signature": e.signature, "prev_hash": prev_hash,
    })
}

pub fn chain_hash(prev_hash: &str, body: &Value) -> Result<String> {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(b"\n");
    h.update(canonical(body)?.as_bytes());
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Append one entry; returns (seq, hash). One immediate transaction, so two writers cannot fork the chain.
pub fn append(l: &mut Ledger, e: &NewEntry) -> Result<(i64, String)> {
    l.require_current()?;
    let tx = l.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (seq, prev): (i64, String) = match tx.query_row("SELECT seq, hash FROM decisions ORDER BY seq DESC LIMIT 1", [], |r| {
        Ok((r.get::<_, i64>(0)? + 1, r.get::<_, String>(1)?))
    }) {
        Ok(x) => x,
        Err(rusqlite::Error::QueryReturnedNoRows) => (1, GENESIS.to_string()),
        Err(e) => return Err(e.into()),
    };
    let hash = chain_hash(&prev, &body(seq, e, &prev))?;
    let ev = e.evidence.as_ref().map(canonical).transpose()?;
    tx.execute(
        "INSERT INTO decisions(seq,at,actor,kind,batch,subject,rule,policy_hash,evidence,outcome,reason,signature,prev_hash,hash) \
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![seq, e.at, e.actor, e.kind, e.batch, e.subject, e.rule, e.policy_hash, ev, e.outcome, e.reason, e.signature, prev, hash],
    )?;
    tx.commit()?;
    Ok((seq, hash))
}

#[derive(Debug, PartialEq)]
pub struct Verified {
    pub entries: i64,
    pub head_hash: String,
}

#[derive(Debug, PartialEq)]
pub struct Broken {
    pub seq: i64,
    pub reason: String,
}

/// Walk from seq 1: no gap, prev_hash linkage, recomputed hash. The first bad row is named.
/// Signatures and policy_hash checks arrive with the policy slice. A truncated tail is only caught by an anchor.
pub fn verify(l: &Ledger) -> Result<std::result::Result<Verified, Broken>> {
    let mut st = l.conn.prepare(
        "SELECT seq,at,actor,kind,batch,subject,rule,policy_hash,evidence,outcome,reason,signature,prev_hash,hash FROM decisions ORDER BY seq",
    )?;
    let mut rows = st.query([])?;
    let (mut expect, mut prev, mut n) = (1i64, GENESIS.to_string(), 0i64);
    while let Some(r) = rows.next()? {
        let seq: i64 = r.get(0)?;
        let broken = |reason: String| Ok(Err(Broken { seq, reason }));
        if seq != expect {
            return broken(format!("sequence gap: expected {expect}, found {seq}"));
        }
        let ev_text: Option<String> = r.get(8)?;
        let evidence = match ev_text {
            None => None,
            Some(t) => match serde_json::from_str::<Value>(&t) {
                Ok(v) => Some(v),
                Err(e) => return broken(format!("evidence is not JSON: {e}")),
            },
        };
        let e = NewEntry {
            at: r.get(1)?,
            actor: r.get(2)?,
            kind: r.get(3)?,
            batch: r.get(4)?,
            subject: r.get(5)?,
            rule: r.get(6)?,
            policy_hash: r.get(7)?,
            evidence,
            outcome: r.get(9)?,
            reason: r.get(10)?,
            signature: r.get(11)?,
        };
        let (stored_prev, stored_hash): (String, String) = (r.get(12)?, r.get(13)?);
        if stored_prev != prev {
            return broken(format!("prev_hash does not match the previous entry's hash (row says {stored_prev})"));
        }
        let want = match chain_hash(&prev, &body(seq, &e, &stored_prev)) {
            Ok(h) => h,
            Err(err) => return broken(format!("entry has no canonical form: {err}")),
        };
        if want != stored_hash {
            return broken(format!("hash mismatch: row says {stored_hash}, content hashes to {want}"));
        }
        prev = stored_hash;
        expect += 1;
        n += 1;
    }
    Ok(Ok(Verified { entries: n, head_hash: prev }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> Ledger {
        let mut l = Ledger::open_in_memory().unwrap();
        l.migrate().unwrap();
        l
    }

    fn entry(n: i64) -> NewEntry {
        NewEntry {
            at: format!("2026-11-15T10:00:{n:02}+07:00"),
            actor: "engine".into(),
            kind: "transition".into(),
            batch: Some("bx".into()),
            subject: Some(format!("sha{n}")),
            evidence: Some(json!({"steps": n, "reds": 0})),
            outcome: "allowed".into(),
            reason: "test".into(),
            ..Default::default()
        }
    }

    fn filled(n: i64) -> Ledger {
        let mut l = ledger();
        for i in 1..=n {
            append(&mut l, &entry(i)).unwrap();
        }
        l
    }

    #[test]
    fn empty_log_verifies() {
        assert_eq!(verify(&ledger()).unwrap(), Ok(Verified { entries: 0, head_hash: GENESIS.into() }));
    }

    #[test]
    fn chain_of_five_verifies() {
        let v = verify(&filled(5)).unwrap().unwrap();
        assert_eq!(v.entries, 5);
    }

    #[test]
    fn update_is_refused_by_trigger() {
        let l = filled(2);
        let e = l.conn.execute("UPDATE decisions SET reason='x' WHERE seq=1", []).unwrap_err();
        assert!(e.to_string().contains("append-only"), "{e}");
    }

    #[test]
    fn delete_is_refused_by_trigger() {
        let l = filled(2);
        let e = l.conn.execute("DELETE FROM decisions WHERE seq=2", []).unwrap_err();
        assert!(e.to_string().contains("append-only"), "{e}");
    }

    #[test]
    fn tamper_with_a_row_names_that_row() {
        let l = filled(5);
        // An attacker with file access drops the trigger first; verify must still catch the edit.
        l.conn.execute_batch("DROP TRIGGER decisions_no_update; UPDATE decisions SET reason='forged' WHERE seq=3;").unwrap();
        let b = verify(&l).unwrap().unwrap_err();
        assert_eq!(b.seq, 3);
        assert!(b.reason.contains("hash mismatch"), "{}", b.reason);
    }

    #[test]
    fn rehashing_a_row_breaks_the_next_link() {
        let l = filled(4);
        l.conn.execute_batch("DROP TRIGGER decisions_no_update; UPDATE decisions SET hash='ab' WHERE seq=2;").unwrap();
        let b = verify(&l).unwrap().unwrap_err();
        assert_eq!(b.seq, 2);
    }

    #[test]
    fn deleted_middle_row_is_a_gap() {
        let l = filled(5);
        l.conn.execute_batch("DROP TRIGGER decisions_no_delete; DELETE FROM decisions WHERE seq=3;").unwrap();
        let b = verify(&l).unwrap().unwrap_err();
        assert_eq!(b.seq, 4);
        assert!(b.reason.contains("gap"), "{}", b.reason);
    }

    #[test]
    fn golden_file_matches_an_independent_implementation() {
        // tests/golden/decisions.jsonl was produced once by an independent hashlib/json implementation, not by this crate (removed in the commit that dropped Python; recoverable from git at 414983e).
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/golden/decisions.jsonl")).unwrap();
        let mut prev = GENESIS.to_string();
        let mut n = 0;
        for line in text.lines() {
            let g: Value = serde_json::from_str(line).unwrap();
            let canon = canonical(&g["body"]).unwrap();
            assert_eq!(canon, g["canonical"].as_str().unwrap(), "canonical bytes, entry {n}");
            assert_eq!(g["body"]["prev_hash"].as_str().unwrap(), prev);
            let h = chain_hash(&prev, &g["body"]).unwrap();
            assert_eq!(h, g["hash"].as_str().unwrap(), "hash, entry {n}");
            prev = h;
            n += 1;
        }
        assert!(n >= 3);
    }
}
