//! Per-request header generators ({{uuid}}/{{now}}/{{hash}}) and schema fingerprints.

use super::*;

/// Resolve per-request generator tokens in an overlay header value. `${ENV}` is
/// already expanded at load; this handles the dynamic ones, fresh per call:
/// `{{uuid}}` (a UUIDv4 — e.g. CDR's `x-fapi-interaction-id`), `{{now}}` /
/// `{{now_ms}}` (unix epoch s/ms), `{{iso8601}}` (UTC RFC-3339).
pub(super) fn resolve_generators(s: &str, args_hash: &str) -> String {
    if !s.contains("{{") {
        return s.to_string();
    }
    let mut out = s.to_string();
    if out.contains("{{hash}}") {
        out = out.replace("{{hash}}", args_hash); // stable per request content (idempotency)
    }
    if out.contains("{{uuid}}") {
        out = out.replace("{{uuid}}", &gen_uuid());
    }
    if out.contains("{{now_ms}}") {
        out = out.replace("{{now_ms}}", &epoch_millis().to_string());
    }
    if out.contains("{{now}}") {
        out = out.replace("{{now}}", &(epoch_millis() / 1000).to_string());
    }
    if out.contains("{{iso8601}}") {
        out = out.replace("{{iso8601}}", &iso8601_utc((epoch_millis() / 1000) as i64));
    }
    out
}

pub(super) fn epoch_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A UUIDv4 as canonical hex. Random bytes come from the OS CSPRNG via
/// `getrandom` (portable: getrandom(2)/SecRandomCopyBytes/RtlGenRandom), with
/// version/variant bits set per RFC 4122. If the OS RNG is somehow unavailable
/// (essentially never on a supported platform), it degrades to a time + monotonic
/// counter mix — still *unique* per call (so a request id never repeats), though
/// no longer unpredictable; don't rely on `{{uuid}}` as a security nonce.
pub(super) fn gen_uuid() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mix = (epoch_millis() as u64) ^ n;
        b[..8].copy_from_slice(&mix.to_le_bytes());
        b[8..].copy_from_slice(&mix.rotate_left(32).to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 1
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Format unix seconds as UTC RFC-3339 (`2026-08-06T08:09:00Z`). Date math via
/// Howard Hinnant's civil-from-days — dependency-free and stable.
pub(super) fn iso8601_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

pub(super) fn fnv1a(s: &str) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Fingerprint the RAW upstream tool: description + input schema. Including the
/// top-level description is what turns the binding registry into a rug-pull /
/// tool-poisoning detector — a server that swaps its description after approval
/// flips this hash, so `authored_sha` no longer matches (SoK arXiv:2512.08290).
pub(super) fn tool_fingerprint(description: &str, schema: &Value) -> String {
    format!("{:016x}", fnv1a(&format!("{description}\u{0}{schema}")))
}
