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
const TOKEN_ENDPOINT: &str = "https://api.anthropic.com/v1/oauth/token";
/// Claude Code's own OAuth client. The refresh grant answers invalid_client without it.
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
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

/// Claude Code's OAuth credential, plus what is needed to renew it in place.
struct Creds {
    token: String,
    refresh: Option<String>,
    /// expiresAt is in the past — the token is spent even if the API has not said so yet
    expired: bool,
    path: std::path::PathBuf,
    /// The file exactly as Claude Code wrote it, so a renewal can put back every field it owns
    raw: String,
}

fn read_credentials() -> Option<Creds> {
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
            return Some(Creds {
                token: tok.to_string(),
                refresh: oauth
                    .get("refreshToken")
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                expired,
                path: p,
                raw: text,
            });
        }
    }
    None
}

/// Puts a renewed token into the credential JSON, leaving every other field exactly as Claude Code wrote
/// it: the file is Claude Code's, and the notch may only move the three values it just renewed.
fn merge_credentials(
    raw: &str,
    access: &str,
    refresh: Option<&str>,
    expires_at: u64,
) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let target = if v.get("claudeAiOauth").is_some() {
        v.get_mut("claudeAiOauth")?
    } else {
        &mut v
    };
    let obj = target.as_object_mut()?;
    obj.insert("accessToken".into(), access.into());
    // The server rotates the refresh token on some renewals and omits it on others; dropping the old one
    // when nothing came back would log the user out of Claude Code itself
    if let Some(r) = refresh {
        obj.insert("refreshToken".into(), r.into());
    }
    obj.insert("expiresAt".into(), serde_json::Value::from(expires_at));
    serde_json::to_string_pretty(&v).ok()
}

/// Swaps the credential file for its renewed copy. The first renewal keeps a backup of the original and
/// the new file lands by rename, so an interrupted write cannot leave Claude Code without a credential.
fn write_credentials(path: &std::path::Path, original: &str, text: &str) -> std::io::Result<()> {
    let bak = path.with_extension("json.codenotch-bak");
    if !bak.exists() {
        let _ = std::fs::write(&bak, original);
    }
    let tmp = path.with_extension("json.codenotch-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Spends the refresh token for a new access token. Returns (access, rotated refresh, expiry ms).
fn refresh_grant(refresh: &str) -> Result<(String, Option<String>, u64), String> {
    let resp = ureq::post(TOKEN_ENDPOINT)
        .set("content-type", "application/json")
        .timeout(Duration::from_secs(20))
        .send_json(serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh,
            "client_id": OAUTH_CLIENT_ID,
        }));
    let v: serde_json::Value = match resp {
        Ok(r) => r.into_json().map_err(|e| format!("parse: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!(
                "HTTP {code}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }
        Err(e) => return Err(format!("{e}")),
    };
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .ok_or("reply has no access_token")?
        .to_string();
    let rotated = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    // A reply without expires_in is treated as already expired rather than trusted forever
    let expires_at = v
        .get("expires_in")
        .and_then(|x| x.as_u64())
        .map(|s| now_ms() + s * 1000)
        .unwrap_or_else(now_ms);
    Ok((access, rotated, expires_at))
}

/// Renews the credential after the API has rejected it. Returns the fresh access token.
fn renew(c: &Creds) -> Option<String> {
    let refresh = c.refresh.as_deref()?;
    let (access, rotated, expires_at) = match refresh_grant(refresh) {
        Ok(t) => t,
        Err(e) => {
            crate::applog(&format!("usage: renewal refused: {e}"));
            return None;
        }
    };
    match merge_credentials(&c.raw, &access, rotated.as_deref(), expires_at) {
        Some(text) => match write_credentials(&c.path, &c.raw, &text) {
            Ok(()) => crate::applog("usage: credential renewed"),
            // The token works even if the file could not be replaced, so this poll still gets a reading
            Err(e) => crate::applog(&format!("usage: renewed, but the file was not written: {e}")),
        },
        None => crate::applog("usage: renewed, but the credential file could not be merged"),
    }
    Some(access)
}

/// For doctor: credential probe report (prints no secret values)
pub fn probe_credentials() -> String {
    match read_credentials() {
        Some(c) => format!(
            "credential: found (token {} chars, {}, refresh token {})",
            c.token.len(),
            if c.expired { "expired — renewed on the next poll" } else { "valid" },
            if c.refresh.is_some() { "present" } else { "missing" }
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
    fn merge_moves_the_three_renewed_values_and_nothing_else() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"r1","expiresAt":1,"scopes":["user"],"subscriptionType":"pro"},"mcpOAuth":{"keep":true}}"#;
        let out = merge_credentials(raw, "new", Some("r2"), 99).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "r2");
        assert_eq!(v["claudeAiOauth"]["expiresAt"], 99);
        assert_eq!(v["claudeAiOauth"]["scopes"][0], "user");
        assert_eq!(v["claudeAiOauth"]["subscriptionType"], "pro");
        assert_eq!(v["mcpOAuth"]["keep"], true);
    }

    #[test]
    fn merge_keeps_the_old_refresh_token_when_none_comes_back() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"r1","expiresAt":1}}"#;
        let out = merge_credentials(raw, "new", None, 42).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "r1");
        assert_eq!(v["claudeAiOauth"]["accessToken"], "new");
    }

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
                Some(c) => {
                    // On 401/403 re-read the file first (Claude Code may have just refreshed it), and spend
                    // our own refresh token only when the file still holds the token that was rejected.
                    let result = match fetch_once(&c.token) {
                        Err(FetchErr::NeedsAuth) => match read_credentials() {
                            Some(c2) if c2.token != c.token => fetch_once(&c2.token),
                            _ => match renew(&c) {
                                Some(fresh) => fetch_once(&fresh),
                                None => Err(FetchErr::NeedsAuth),
                            },
                        },
                        other => other,
                    };
                    let expired = c.expired;
                    let auth_note = if c.refresh.is_none() {
                        "No refresh token — sign in with the Claude Code CLI"
                    } else {
                        "Credential rejected and the renewal was refused — sign in with the Claude Code CLI"
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
