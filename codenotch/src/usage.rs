//! Claude usage adapter (official), implemented from the upstream Codenotch's documented behaviour.
//! Endpoint: GET https://api.anthropic.com/api/oauth/usage
//! Headers: Authorization: Bearer <token>; anthropic-beta: oauth-2025-04-20; 15 s timeout
//! Rules (upstream's discipline):
//!   - the credential comes from Claude Code's own store (Windows: ~/.claude/.credentials.json), read only
//!   - 401/403 → re-read the credential once and retry (Claude Code may have just refreshed the token) → still failing means needsAuth
//!   - 429 → back off 60 s × 2^n, Retry-After raises it, always capped at 5 min; the deadline is persisted
//!   - never invent a percentage on failure: keep the last reading marked stale, and the UI shows how old it is
//! Reply (snake_case): { limits:[{kind,percent,resets_at}], five_hour:{utilization,resets_at}, seven_day:{...} }
//! limits is the forward-compatible main shape; five_hour/seven_day are merged in as a fallback (a window that just rolled over disappears from limits).

use crate::AppState;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const POLL_ACTIVE_SECS: u64 = 60;
const POLL_IDLE_SECS: u64 = 120;
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 300;

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Immediate refresh from the tray or a command
pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Sleep in slices so request_refresh can interrupt it
fn sleep_interruptible(total_secs: u64) {
    for _ in 0..total_secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LimitWindow {
    pub id: String,
    pub label: String,
    /// 0.0–1.0 (fraction used)
    pub used: f64,
    /// Reset time, ms epoch (None = unknown)
    pub resets_at: Option<u64>,
    /// Pure count window (no published denominator, a number with no published denominator) — the cell shows ~N and the ring draws only its track
    #[serde(default)]
    pub count: Option<i64>,
    /// The number is ours, not the vendor's (upstream fidelity=.derived) — the card adds a ~ prefix
    #[serde(default)]
    pub derived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageSnapshot {
    /// ok | stale | needsAuth | backoff | error
    pub status: String,
    pub windows: Vec<LimitWindow>,
    pub fetched_at: u64,
    pub note: String,
    #[serde(default)]
    pub backoff_until: u64,
}

fn store_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("usage.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into(); // an old reading after a restart is labelled as such
            }
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

/// Reads Claude Code's OAuth credential. Returns (token, expired hint).
fn read_credentials() -> Option<(String, bool)> {
    let home = dirs::home_dir()?;
    for name in [".credentials.json", "credentials.json"] {
        let p = home.join(".claude").join(name);
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let oauth = v.get("claudeAiOauth").unwrap_or(&v);
        if let Some(tok) = oauth.get("accessToken").and_then(|x| x.as_str()) {
            let expired = oauth
                .get("expiresAt")
                .and_then(|x| x.as_f64())
                .map(|ms| (ms as u64) <= now_ms())
                .unwrap_or(false);
            return Some((tok.to_string(), expired));
        }
    }
    None
}

/// For doctor: credential probe report (prints no secret values)
pub fn probe_credentials() -> String {
    match read_credentials() {
        Some((tok, expired)) => format!(
            "credential: found (token {} chars, {})",
            tok.len(),
            if expired { "expired — Claude Code refreshes it on its next use" } else { "valid" }
        ),
        None => "credential: ~/.claude/.credentials.json not found (needsAuth; the desktop app may use another store — signing in once with the Claude Code CLI creates it)".into(),
    }
}

/// Base64 for the JWT payloads the providers hand out: accepts both alphabets and tolerates
/// missing padding, which is what those tokens actually look like.
pub(crate) fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for c in s.bytes() {
        let sextet: u8 = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        let v = sextet as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn parse_reset(v: &serde_json::Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

fn label_for(kind: &str) -> String {
    match kind {
        "session" => "Current session".into(),
        "seven_day" | "weekly_all" => "Weekly (all models)".into(),
        "seven_day_opus" | "weekly_opus" => "Weekly (Opus)".into(),
        "weekly_scoped" => "Weekly (model-scoped)".into(),
        other => {
            // Forward compatibility: an unknown kind gets a readable label
            let mut s = other.replace('_', " ");
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

fn parse_response(v: &serde_json::Value) -> Vec<LimitWindow> {
    let mut out: Vec<LimitWindow> = Vec::new();
    if let Some(arr) = v.get("limits").and_then(|x| x.as_array()) {
        for l in arr {
            let Some(kind) = l.get("kind").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(pct) = l.get("percent").and_then(|x| x.as_f64()) else {
                continue;
            };
            let resets = l.get("resets_at").and_then(parse_reset);
            if resets.is_none() {
                continue; // upstream rule: a window without a reset time is not shown
            }
            out.push(LimitWindow {
                id: kind.to_string(),
                label: label_for(kind),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: resets, ..Default::default()
            });
        }
    }
    // Fallback merge: a window that just rolled over disappears from limits while the named field remains.
    // In practice the kinds in limits are weekly_all/weekly_scoped, not seven_day — deduplicating by id
    // alone would add the seven_day fallback a second time (the card showed "Weekly all" and
    // "Weekly (all models)" as twins). Three dedupe rules: id alias / same resets_at and percentage / same label.
    let aliases: [(&str, &str, &[&str]); 2] = [
        ("five_hour", "session", &["session", "five_hour"]),
        ("seven_day", "seven_day", &["seven_day", "weekly_all", "weekly"]),
    ];
    for (field, id, alias) in aliases {
        let Some(w) = v.get(field) else { continue };
        let Some(u) = w.get("utilization").and_then(|x| x.as_f64()) else { continue };
        let used = (u / 100.0).clamp(0.0, 1.0);
        let resets_at = w.get("resets_at").and_then(parse_reset);
        let label = label_for(id);
        let dup = out.iter().any(|x| {
            alias.contains(&x.id.as_str())
                || x.label == label
                || (resets_at.is_some()
                    && x.resets_at.map(|r| r / 1000) == resets_at.map(|r| r / 1000)
                    && (x.used - used).abs() < 0.005)
        });
        if dup {
            continue;
        }
        out.push(LimitWindow { id: id.into(), label, used, resets_at, ..Default::default() });
    }
    // session always comes first (upstream display order)
    out.sort_by_key(|w| if w.id == "session" { 0 } else { 1 });
    out
}

enum FetchErr {
    NeedsAuth,
    RateLimited(u64), // suggested wait in seconds (the Retry-After before the floor is applied)
    Other(String),
}

fn fetch_once(token: &str) -> Result<Vec<LimitWindow>, FetchErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => {
            let v: serde_json::Value = r
                .into_json()
                .map_err(|e| FetchErr::Other(format!("parse: {e}")))?;
            Ok(parse_response(&v))
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err(FetchErr::NeedsAuth)
        }
        Err(ureq::Error::Status(429, r)) => {
            let ra = r
                .header("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Err(FetchErr::RateLimited(ra))
        }
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn backoff_secs(consecutive: u32, retry_after_floor: u64) -> u64 {
    let exp = BACKOFF_BASE_SECS.saturating_mul(1u64 << consecutive.min(4));
    // ponytail: the cap wins over Retry-After. The endpoint answers 429 with Retry-After: 3600,
    // which used to freeze the reading for an hour at a time; one request per cap window costs
    // nothing and keeps the ring from going hours stale.
    exp.max(retry_after_floor).clamp(BACKOFF_BASE_SECS, BACKOFF_CAP_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_never_exceeds_the_cap() {
        assert_eq!(backoff_secs(0, 0), BACKOFF_BASE_SECS);
        assert_eq!(backoff_secs(0, 3600), BACKOFF_CAP_SECS); // Retry-After raises, cap still holds
        assert_eq!(backoff_secs(9, 0), BACKOFF_CAP_SECS);
        assert!(backoff_secs(2, 120) >= BACKOFF_BASE_SECS);
    }
}

/// One log line per distinct failure: the reason a reading is old has to be in run.log, but a poller in a
/// long backoff must not drown the activity lines it sits next to.
fn log_once(last: &mut String, note: String) {
    if *last != note {
        crate::applog(&format!("usage: {note}"));
        *last = note;
    }
}

fn set_and_broadcast(app: &AppHandle, mutate: impl FnOnce(&mut UsageSnapshot)) {
    let st = app.state::<AppState>();
    let snap = {
        let mut u = st.usage.lock().unwrap();
        mutate(&mut u);
        u.clone()
    };
    persist(&snap);
    let _ = app.emit("usage", &snap);
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // Broadcast the persisted old reading at startup (stale beats blank)
        {
            let st = app.state::<AppState>();
            let snap = st.usage.lock().unwrap().clone();
            let _ = app.emit("usage", &snap);
        }
        let mut consecutive_429: u32 = 0;
        // Last logged failure, so a poller stuck for hours writes one line instead of one per tick
        let mut last_note = String::new();
        loop {
            // No requests inside the backoff window
            let bu = {
                let st = app.state::<AppState>();
                let u = st.usage.lock().unwrap();
                u.backoff_until
            };
            let now = now_ms();
            if bu > now {
                sleep_interruptible(((bu - now) / 1000).clamp(1, 30));
                continue;
            }
            match read_credentials() {
                None => {
                    log_once(&mut last_note, "no credential file".into());
                    set_and_broadcast(&app, |u| {
                        u.status = "needsAuth".into();
                        u.note = "No Claude Code credential found".into();
                    })
                }
                Some((token, expired)) => {
                    // On 401/403 re-read the credential and retry once (Claude Code may have just refreshed it)
                    let result = match fetch_once(&token) {
                        Err(FetchErr::NeedsAuth) => match read_credentials() {
                            Some((t2, _)) if t2 != token => fetch_once(&t2),
                            _ => Err(FetchErr::NeedsAuth),
                        },
                        other => other,
                    };
                    let auth_note = if expired {
                        "Credential expired — run any claude command (or chat with Claude) to refresh it"
                    } else {
                        "Credential rejected (switched accounts?)"
                    };
                    match result {
                        Ok(windows) => {
                            if consecutive_429 > 0 || !last_note.is_empty() {
                                crate::applog(&format!("usage: ok again after {last_note}"));
                                last_note.clear();
                            }
                            consecutive_429 = 0;
                            set_and_broadcast(&app, |u| {
                                u.status = "ok".into();
                                u.windows = windows;
                                u.fetched_at = now_ms();
                                u.note.clear();
                                u.backoff_until = 0;
                            });
                        }
                        Err(FetchErr::NeedsAuth) => {
                            log_once(&mut last_note, format!("401/403 (credential expired={expired})"));
                            set_and_broadcast(&app, |u| {
                                u.status = "needsAuth".into();
                                u.note = auth_note.into();
                            })
                        }
                        Err(FetchErr::RateLimited(ra)) => {
                            consecutive_429 += 1;
                            let wait = backoff_secs(consecutive_429 - 1, ra);
                            log_once(
                                &mut last_note,
                                format!("429 (Retry-After {ra}s, #{consecutive_429}) → waiting {wait}s"),
                            );
                            set_and_broadcast(&app, |u| {
                                if !u.windows.is_empty() {
                                    u.status = "stale".into();
                                }
                                u.note = format!("Rate limited, retrying in {wait}s");
                                u.backoff_until = now_ms() + wait * 1000;
                            });
                        }
                        Err(FetchErr::Other(msg)) => {
                            log_once(&mut last_note, msg.clone());
                            set_and_broadcast(&app, |u| {
                            if u.windows.is_empty() {
                                u.status = "error".into();
                            } else {
                                u.status = "stale".into();
                            }
                            u.note = msg;
                            })
                        }
                    }
                }
            }
            // 60 s while a session is active, 120 s otherwise
            let active = {
                let st = app.state::<AppState>();
                let store = st.store.lock().unwrap();
                let s = store.snapshot("en", "en", false);
                !s.sessions.is_empty()
            };
            sleep_interruptible(if active {
                POLL_ACTIVE_SECS
            } else {
                POLL_IDLE_SECS
            });
        }
    });
}
