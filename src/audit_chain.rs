//! Tamper evidence for the audit stream.
//!
//! The audit log says who called what. On its own it is a text file: anyone who
//! can write to it can rewrite a line, or delete one, and nothing shows.
//!
//! A plain hash chain does NOT fix that. An attacker who can rewrite the file
//! can recompute every hash after the line they changed, and the chain still
//! verifies — it detects corruption, not tampering. Evidence requires something
//! the attacker does not have, so each line carries an **HMAC** under a key the
//! writer holds: `log_hmac_key`.
//!
//! Each line gets `seq` and `mac`, where the MAC covers the previous line's MAC
//! as well as this line's content. That chaining is what makes the two attacks
//! visible:
//!
//! - **Altering** a line changes its MAC, and because the next line's MAC
//!   covers it, every later line fails too. The break points at the first
//!   altered line.
//! - **Deleting** a line leaves a gap in `seq` and breaks the chain at the
//!   following line, so removing the record of one call is not quiet either.
//!
//! What it does not defend against: an attacker who has the KEY (they can forge
//! freely), or one who truncates the tail and stops — the remaining prefix is
//! internally consistent. Truncation is detectable only against an external
//! record of where the log had reached, which is what shipping lines to a SIEM
//! gives you. The key belongs somewhere the proxy can read and an intruder on
//! the log host cannot: `${vault:…}` or a mounted secret, not the config file.

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Running chain state: the sequence number of the next line, and the MAC of
/// the previous one.
pub struct Chain {
    key: ring::hmac::Key,
    next_seq: u64,
    prev_mac: String,
}

/// The MAC of the very first line has no predecessor to cover; this stands in,
/// so line 1 is chained by the same rule as every other line.
const GENESIS: &str = "genesis";

impl Chain {
    pub fn new(secret: &str) -> Self {
        Chain {
            key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes()),
            next_seq: 1,
            prev_mac: GENESIS.to_string(),
        }
    }

    /// Resume from a log this process is appending to.
    ///
    /// Without this a restart would start a second chain at seq 1 in the middle
    /// of the file, and verification would read that as tampering — the feature
    /// would cry wolf on every deploy.
    pub fn resume_from(&mut self, path: &str) -> Result<()> {
        let Ok(text) = std::fs::read_to_string(path) else { return Ok(()) };
        let Some(last) = text.lines().rfind(|l| !l.trim().is_empty()) else {
            return Ok(());
        };
        let v: Value = serde_json::from_str(last)
            .with_context(|| format!("the last line of {path} is not JSON; refusing to chain onto it"))?;
        let (Some(seq), Some(mac)) = (v.get("seq").and_then(Value::as_u64), v.get("mac").and_then(Value::as_str))
        else {
            bail!(
                "{path} already has audit lines without `seq`/`mac`. Chaining onto an unsigned \
                 log would imply the earlier lines were protected too — rotate the file first."
            );
        };
        self.next_seq = seq + 1;
        self.prev_mac = mac.to_string();
        Ok(())
    }

    /// Stamp `line` with its sequence number and MAC, in place.
    pub fn stamp(&mut self, line: &mut Value) {
        let seq = self.next_seq;
        if let Some(obj) = line.as_object_mut() {
            obj.insert("seq".into(), Value::from(seq));
        }
        let mac = mac_for(&self.key, &self.prev_mac, seq, line);
        if let Some(obj) = line.as_object_mut() {
            obj.insert("mac".into(), Value::from(mac.clone()));
        }
        self.next_seq = seq + 1;
        self.prev_mac = mac;
    }
}

/// The MAC of one line: HMAC-SHA256 over the previous MAC, the sequence number,
/// and the line's content with `mac` removed.
///
/// `mac` is excluded because it is the output; `seq` is covered explicitly as
/// well as being in the body, so a line cannot be moved to another position
/// without breaking.
fn mac_for(key: &ring::hmac::Key, prev: &str, seq: u64, line: &Value) -> String {
    let mut body = line.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("mac");
    }
    // serde_json with `preserve_order` keeps insertion order, so a line
    // re-serialised from the file hashes identically to the one written.
    let payload = format!("{prev}\u{1f}{seq}\u{1f}{body}");
    let tag = ring::hmac::sign(key, payload.as_bytes());
    tag.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// What a verification run found.
pub struct Verdict {
    pub lines: usize,
    /// 1-based line numbers that failed, with why.
    pub failures: Vec<(usize, String)>,
}

impl Verdict {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Re-derive every MAC in `path` and report the first line that does not match.
pub fn verify(path: &str, secret: &str) -> Result<Verdict> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let mut prev = GENESIS.to_string();
    let mut expect_seq = 1u64;
    let mut failures = Vec::new();
    let mut lines = 0usize;

    for (i, raw) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let n = i + 1;
        lines = n;
        let v: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                failures.push((n, format!("not JSON: {e}")));
                break; // the chain cannot continue past a line we cannot read
            }
        };
        let seq = v.get("seq").and_then(Value::as_u64);
        let mac = v.get("mac").and_then(Value::as_str).map(str::to_string);
        let (Some(seq), Some(mac)) = (seq, mac) else {
            failures.push((n, "no `seq`/`mac` — the line is unsigned".into()));
            break;
        };
        if seq != expect_seq {
            // A gap means lines were removed; a repeat means they were spliced.
            failures.push((n, format!("sequence jumped: expected {expect_seq}, found {seq}")));
            break;
        }
        let want = mac_for(&key, &prev, seq, &v);
        if want != mac {
            failures.push((n, "MAC does not match — this line was altered, or the one before it was".into()));
            break;
        }
        prev = mac;
        expect_seq = seq + 1;
    }
    Ok(Verdict { lines, failures })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &str = "audit-key";

    fn tmp(name: &str) -> String {
        let p = std::env::temp_dir().join(format!("minmcp-chain-{}-{name}.ndjson", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p.to_string_lossy().into_owned()
    }

    /// Write `n` chained lines, returning the path.
    fn write_log(name: &str, n: usize) -> String {
        let path = tmp(name);
        let mut chain = Chain::new(KEY);
        let mut out = String::new();
        for i in 0..n {
            let mut line = json!({"ts_ms": 1000 + i, "event": "call", "caller": "alice", "tool_id": format!("t{i}")});
            chain.stamp(&mut line);
            out.push_str(&format!("{line}\n"));
        }
        std::fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn an_untouched_log_verifies() {
        let path = write_log("clean", 5);
        let v = verify(&path, KEY).unwrap();
        assert!(v.ok(), "a log nobody touched must verify: {:?}", v.failures);
        assert_eq!(v.lines, 5);
    }

    #[test]
    fn altering_a_line_is_caught_at_that_line() {
        // The attack: rewrite one record to hide what was called.
        let path = write_log("altered", 5);
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered: Vec<String> = text
            .lines()
            .map(|l| l.replace(r#""tool_id":"t2""#, r#""tool_id":"something_innocent""#))
            .collect();
        std::fs::write(&path, tampered.join("\n")).unwrap();

        let v = verify(&path, KEY).unwrap();
        assert!(!v.ok(), "an altered line must not verify");
        assert_eq!(v.failures[0].0, 3, "the break should point at the altered line (1-based)");
    }

    #[test]
    fn deleting_a_line_is_caught_by_the_sequence() {
        // The other attack, and the one a per-line signature alone would miss:
        // remove the record entirely rather than change it.
        let path = write_log("deleted", 5);
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = text.lines().enumerate().filter(|(i, _)| *i != 2).map(|(_, l)| l).collect();
        std::fs::write(&path, kept.join("\n")).unwrap();

        let v = verify(&path, KEY).unwrap();
        assert!(!v.ok(), "a deleted line must be detectable");
        assert!(v.failures[0].1.contains("sequence"), "should name the gap: {:?}", v.failures);
    }

    #[test]
    fn reordering_lines_is_caught() {
        let path = write_log("reordered", 5);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.swap(1, 3);
        std::fs::write(&path, lines.join("\n")).unwrap();
        assert!(!verify(&path, KEY).unwrap().ok(), "reordered lines must not verify");
    }

    #[test]
    fn the_wrong_key_does_not_verify() {
        // The whole point: without the key you cannot produce a passing chain,
        // which is why a bare hash chain would not be evidence.
        let path = write_log("wrongkey", 3);
        let v = verify(&path, "not-the-key").unwrap();
        assert!(!v.ok(), "a different key must not verify");
        assert_eq!(v.failures[0].0, 1, "it fails from the very first line");
    }

    #[test]
    fn an_attacker_without_the_key_cannot_repair_the_chain() {
        // A plain hash chain is recomputable by whoever rewrote the file. This
        // asserts the difference: re-chaining with a guessed key is rejected.
        let path = write_log("repair", 4);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut forged = Chain::new("guessed-key");
        let mut out = String::new();
        for l in text.lines() {
            let mut v: Value = serde_json::from_str(l).unwrap();
            if let Some(o) = v.as_object_mut() {
                o.insert("tool_id".into(), json!("forged"));
                o.remove("seq");
                o.remove("mac");
            }
            forged.stamp(&mut v);
            out.push_str(&format!("{v}\n"));
        }
        std::fs::write(&path, out).unwrap();
        assert!(!verify(&path, KEY).unwrap().ok(), "a re-chained forgery must still fail");
    }

    #[test]
    fn resuming_continues_one_chain_across_a_restart() {
        // Without this a restart begins a second chain at seq 1 mid-file, and
        // verification reads that as tampering — crying wolf on every deploy.
        let path = write_log("resume", 3);
        let mut chain = Chain::new(KEY);
        chain.resume_from(&path).unwrap();
        let mut line = json!({"ts_ms": 9999, "event": "call", "caller": "bob"});
        chain.stamp(&mut line);
        assert_eq!(line["seq"], json!(4), "sequence continues rather than restarting");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("{line}\n"));
        std::fs::write(&path, text).unwrap();
        assert!(verify(&path, KEY).unwrap().ok(), "the resumed chain must verify end to end");
    }

    #[test]
    fn refusing_to_chain_onto_an_unsigned_log() {
        // Appending signed lines to an unsigned file would imply the earlier
        // lines were protected too. They were not.
        let path = tmp("unsigned");
        std::fs::write(&path, "{\"event\":\"call\",\"caller\":\"alice\"}\n").unwrap();
        let mut chain = Chain::new(KEY);
        let err = chain.resume_from(&path).unwrap_err().to_string();
        assert!(err.contains("rotate"), "should tell the operator what to do: {err}");
    }

    #[test]
    fn an_empty_or_missing_log_starts_a_fresh_chain() {
        let mut chain = Chain::new(KEY);
        chain.resume_from("/nonexistent/minmcp/audit.ndjson").unwrap();
        let mut line = json!({"event": "call"});
        chain.stamp(&mut line);
        assert_eq!(line["seq"], json!(1));
    }
}
