//! Admin-only readers for the two step-log folders.
//!
//! WHY THESE EXIST AT ALL, given `main.rs` 404s both folders over HTTP:
//! `<uploads>/log` and `<uploads>/login` sit INSIDE the tree the `/uploads`
//! static route serves, and the 404 scopes registered ahead of it are what stop
//! anyone with a URL from browsing every sign-in and every face call. That
//! block must stay exactly as it is. These handlers are the deliberate,
//! *gated* way in: nothing is served as a file, every response is JSON built
//! here, and each call must carry a valid bearer token AND `X-Admin-Key`.
//!
//! Do not "simplify" this by relaxing the static route. The files carry
//! usernames, client IPs, GPS coordinates, employee ids and full request and
//! response bodies; an unauthenticated URL to any of them is the whole risk.
//!
//! Two folders, two endpoints, because they answer different questions and are
//! filtered differently:
//!   * `logs/login`      — `LOGIN_LOG_DIR`, one file per sign-in.
//!   * `logs/attendance` — `WOW_LOG_DIR`, one file per enroll/verify/mapping-save.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use actix_web::{post, web, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::routes::wow_attendance::require_admin_caller;

/// Attendance step logs. Same default and env var `StepLogger` itself uses, so
/// the reader can never point at a different folder than the writer.
fn attendance_log_dir() -> String {
    std::env::var("WOW_LOG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "./uploads/log".to_string())
}

/// Sign-in step logs — mirrors `routes::auth::login_log_dir`.
fn login_log_dir() -> String {
    std::env::var("LOGIN_LOG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "./uploads/login".to_string())
}

/// Hard ceiling on a single file returned to the browser. The logger truncates
/// long params itself, so a file over this is a malformed one — return the head
/// and say so rather than streaming megabytes into a `<pre>`.
const MAX_CONTENT_BYTES: usize = 512 * 1024;

/// Page ceiling. Listing reads the first line of every file ON THE PAGE, so an
/// unbounded `limit` would be an unbounded number of file opens per request.
const MAX_LIMIT: i64 = 200;

#[derive(Deserialize)]
pub struct LogQuery {
    /// When present the response is that one file's content instead of a
    /// listing. Validated against the real directory listing — see `read_one`.
    pub file: Option<String>,
    /// Substring match on the id the file is named after (person id, or the
    /// submitted username for a failed login).
    pub person_id: Option<String>,
    /// Inclusive `YYYY-MM-DD` bounds on the timestamp in the filename.
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub page: Option<i64>,
    pub limit: Option<i64>,
}

/// One row of the listing, before the route line is read.
struct Entry {
    file: String,
    person_id: String,
    at: chrono::NaiveDateTime,
    size_bytes: u64,
}

/// Split `{id}_{%Y%m%d}_{%I}_{%M}_{%S}_{%p}.log` back into its id and timestamp.
///
/// Parsed from the RIGHT: the id is whatever precedes the last five segments,
/// because it is not always a bare number. A failed login is filed under the
/// submitted username (`routes::auth::login` sets it before DU answers), and an
/// email address or a name can contain `_` itself.
fn parse_log_name(name: &str) -> Option<(String, chrono::NaiveDateTime)> {
    let stem = name.strip_suffix(".log")?;
    // [ampm, ss, mm, hh, yyyymmdd, id] — rsplitn yields right-to-left.
    let parts: Vec<&str> = stem.rsplitn(6, '_').collect();
    if parts.len() < 6 {
        return None;
    }
    let (ampm, ss, mm, hh, date, id) = (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]);
    if id.is_empty() {
        return None;
    }
    let at = chrono::NaiveDateTime::parse_from_str(
        &format!("{date} {hh}:{mm}:{ss} {ampm}"),
        // %I is 12-hour and %p the AM/PM the writer used; parsing with %H here
        // would silently mis-order every afternoon log.
        "%Y%m%d %I:%M:%S %p",
    )
    .ok()?;
    Some((id.to_string(), at))
}

/// The `route:` value the logger writes as line 1, e.g.
/// `ext-api/wow-attendance/verify`. Read per row so the listing can say what
/// each call WAS without opening the file in the UI — only the first line is
/// read, never the body.
fn route_line(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    first
        .strip_prefix("route:")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn bad_request(message: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "success": false, "message": message }))
}

/// Every `.log` in `dir`, newest first, after the query's filters.
///
/// Errors from the directory read are NOT surfaced as 500s: a folder that does
/// not exist yet is the normal state on a fresh box (nothing has been logged),
/// and an empty list is the honest answer there.
fn collect(dir: &str, q: &LogQuery) -> Result<Vec<Entry>, HttpResponse> {
    let from = match q.from_date.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| bad_request("`from_date` must be YYYY-MM-DD"))?,
        ),
        None => None,
    };
    let to = match q.to_date.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| bad_request("`to_date` must be YYYY-MM-DD"))?,
        ),
        None => None,
    };
    let needle = q
        .person_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);

    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };

    let mut out: Vec<Entry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".log") {
            continue;
        }
        let Some((person_id, at)) = parse_log_name(&name) else {
            // A name this reader cannot date is skipped rather than shown with
            // a fabricated timestamp — it would sort to an arbitrary place.
            continue;
        };
        if let Some(n) = &needle {
            if !person_id.to_lowercase().contains(n.as_str()) {
                continue;
            }
        }
        if from.is_some_and(|f| at.date() < f) || to.is_some_and(|t| at.date() > t) {
            continue;
        }
        out.push(Entry {
            file: name,
            person_id,
            at,
            size_bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
        });
    }

    // Newest first, and the filename as a stable tiebreak so two calls landing
    // in the same second do not swap places between requests and duplicate a
    // row across page boundaries.
    out.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.file.cmp(&a.file)));
    Ok(out)
}

fn list(dir: &str, q: &LogQuery) -> HttpResponse {
    let all = match collect(dir, q) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let page = q.page.unwrap_or(1).max(1);
    let limit = q.limit.unwrap_or(20).clamp(1, MAX_LIMIT);
    let total = all.len() as i64;
    let skip = ((page - 1) * limit).max(0) as usize;

    let rows: Vec<_> = all
        .iter()
        .skip(skip)
        .take(limit as usize)
        .map(|e| {
            json!({
                "file": e.file,
                "person_id": e.person_id,
                // ISO-ish and already local wall-clock: the filename carries no
                // zone, so this is rendered as-is rather than pretended to be UTC.
                "logged_at": e.at.format("%Y-%m-%d %H:%M:%S").to_string(),
                "size_bytes": e.size_bytes,
                "route": route_line(&Path::new(dir).join(&e.file)),
            })
        })
        .collect();

    HttpResponse::Ok().json(json!({
        "success": true,
        "total": total,
        "page": page,
        "limit": limit,
        "files": rows,
    }))
}

fn read_one(dir: &str, requested: &str) -> HttpResponse {
    // PATH TRAVERSAL GUARD, and the only one that matters: the requested name
    // is never joined onto `dir` until it has been found in that directory's
    // OWN listing. `../`, an absolute path, a symlink name and a NUL byte all
    // fail to match a real entry, so none of them can reach a file. Do not
    // replace this with string checks on the input — those are the ones that
    // get bypassed.
    let found = fs::read_dir(dir)
        .ok()
        .and_then(|read| {
            read.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .find(|n| n == requested && n.ends_with(".log"))
        });

    let Some(name) = found else {
        return HttpResponse::NotFound()
            .json(json!({ "success": false, "message": "No such log file" }));
    };

    let path = Path::new(dir).join(&name);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("log read failed for {}: {err}", path.display());
            return HttpResponse::InternalServerError()
                .json(json!({ "success": false, "message": "Could not read that log file" }));
        }
    };

    let size_bytes = bytes.len();
    let truncated = size_bytes > MAX_CONTENT_BYTES;
    let slice = if truncated {
        // Back off to a char boundary — the logs are UTF-8 and a mid-codepoint
        // split would corrupt the last line. A continuation byte is 0b10xxxxxx.
        let mut end = MAX_CONTENT_BYTES;
        while end > 0 && bytes[end] & 0xC0 == 0x80 {
            end -= 1;
        }
        &bytes[..end]
    } else {
        &bytes[..]
    };

    let (person_id, logged_at) = parse_log_name(&name)
        .map(|(id, at)| (id, at.format("%Y-%m-%d %H:%M:%S").to_string()))
        .unwrap_or_default();

    HttpResponse::Ok().json(json!({
        "success": true,
        "file": name,
        "person_id": person_id,
        "logged_at": logged_at,
        "size_bytes": size_bytes,
        "truncated": truncated,
        "content": String::from_utf8_lossy(slice),
    }))
}

fn handle(req: &actix_web::HttpRequest, dir: String, q: &LogQuery) -> HttpResponse {
    if let Err(resp) = require_admin_caller(req) {
        return resp;
    }
    match q.file.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => read_one(&dir, name),
        None => list(&dir, q),
    }
}

// ---------------------------------------------------------------------
// Sign-in logs — POST /ext-api/wow-attendance/logs/login
// ---------------------------------------------------------------------
//
// Query only, no form body. Every other endpoint here accepts both because it
// has clients predating the move to headers; these two have none, so they take
// the one form and there is nothing to keep in sync.
#[post("/wow-attendance/logs/login")]
pub async fn wow_login_logs(
    req: actix_web::HttpRequest,
    query: web::Query<LogQuery>,
) -> HttpResponse {
    handle(&req, login_log_dir(), &query)
}

// ---------------------------------------------------------------------
// Attendance step logs — POST /ext-api/wow-attendance/logs/attendance
// ---------------------------------------------------------------------
#[post("/wow-attendance/logs/attendance")]
pub async fn wow_attendance_logs(
    req: actix_web::HttpRequest,
    query: web::Query<LogQuery>,
) -> HttpResponse {
    handle(&req, attendance_log_dir(), &query)
}
