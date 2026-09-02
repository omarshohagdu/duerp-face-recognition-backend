// Per-call step logger.
//
// Writes one log file per API call to `<WOW_LOG_DIR>/{id}_{time}.log`, capturing an
// ordered, timestamped line for every step the handler goes through. The file
// is flushed on Drop, so it is written no matter which branch the handler
// returns from (including every early `return`) — as long as the logger stays
// alive for the whole handler body.
//
// Usage:
//     let log = StepLogger::new("ext-api/wow-attendance/enroll");
//     log.step("request received");
//     log.set_id(&id);                        // renames the eventual file
//     log.params("query", &query_to_json(q)); // what the caller sent
//     log.file(&path, browsable.as_deref());  // what we wrote, and its URL
//     log.response("ai", 200, &ai_body);      // what the AI platform said
//     log.response("local", 200, &our_body);  // what we said back
//
// KEEP THIS FILE IDENTICAL to `duerp-api/src/utils/step_logger.rs` apart from
// this header. Each service writes into its own folder now, but ONE parser reads
// both — `duerp-api/src/routes/log_routes.rs` merges the two folders, splitting
// on the `----` separator and reading the `route:` / `id:` / `started_at:`
// header keys. A format change on one side silently breaks the viewer for every
// file, not just that service's.
//
// The destination directory is `./uploads/log` by default and can be overridden
// with the `WOW_LOG_DIR` environment variable.

use std::cell::RefCell;

use serde_json::Value;

/// Cap on how much of one `Params` / `Response` payload reaches a log line.
/// AI recognition replies can carry large arrays, and an unbounded response
/// would let one call write a multi-megabyte file into the shared uploads
/// volume. The line says when it clipped, so a truncated payload is never
/// mistaken for a short one.
const MAX_LOGGED_CHARS: usize = 4000;

/// Keys whose VALUE is masked wherever it appears, at any depth.
///
/// The step logs are admin-gated and HTTP-blocked, but they are still files on
/// a shared volume — a bearer token or password copied into one is a credential
/// sitting in a second place, outliving the request that carried it. Matching is
/// substring + case-insensitive so `X-App-Password`, `access_token` and
/// `apiKey` are all caught without listing every spelling.
const SECRET_KEY_MARKERS: [&str; 13] = [
    "password", "passwd", "secret", "token", "authorization", "api_key",
    "apikey", "credential", "signature", "private", "cookie",
    // Bare "session" is deliberately NOT a marker: across this ERP it means the
    // ACADEMIC session ("2024-25"), and masking it would gut the logs for the
    // most common query parameter there is. The two spellings that really do
    // carry a credential are listed instead, and "token" already covers
    // `session_token` / `access_token` / `refresh_token`.
    "session_id", "sessionid",
];

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEY_MARKERS.iter().any(|m| key.contains(m))
}

/// Deep copy with secret values replaced by a marker. Structure is preserved —
/// the point is to show that a field was sent, and its shape, without showing
/// what it was.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_secret_key(k) {
                        (k.clone(), Value::String("[redacted]".into()))
                    } else {
                        (k.clone(), redact(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// Minimal percent-decoding, so a logged query reads as what the caller meant
/// rather than as `%20`-soup. Invalid escapes are left verbatim instead of
/// being dropped — a malformed query is itself worth seeing in the log.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `a=1&b=two%20words` -> `{"a":"1","b":"two words"}`, secrets masked.
/// An empty query yields an empty object rather than null, so the log line
/// distinguishes "no params" from "params we failed to read".
pub fn query_to_json(query: &str) -> Value {
    let mut map = serde_json::Map::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = percent_decode(k);
        let val = if is_secret_key(&key) {
            "[redacted]".to_string()
        } else {
            percent_decode(v)
        };
        map.insert(key, Value::String(val));
    }
    Value::Object(map)
}

fn clip(mut s: String) -> String {
    if s.chars().count() > MAX_LOGGED_CHARS {
        let total = s.len();
        s = s.chars().take(MAX_LOGGED_CHARS).collect::<String>();
        s.push_str(&format!(" … [truncated, {total} bytes total]"));
    }
    s
}

/// Mask secret values in a raw query string, keeping its `k=v&k=v` shape.
///
/// The `endpoint:` line shows the URL as it was called, which would otherwise
/// make it the one place a token survives after `params()` masked it everywhere
/// else. Keys are decoded before matching so `access%5Ftoken` is caught too.
pub fn redact_query(query: &str) -> String {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => {
                if is_secret_key(&percent_decode(k)) {
                    format!("{k}=[redacted]")
                } else {
                    format!("{k}={v}")
                }
            }
            None => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Render a value for a log line: redacted, compact, clipped.
fn render(value: &Value) -> String {
    clip(serde_json::to_string(&redact(value)).unwrap_or_else(|_| "<unserialisable>".into()))
}


/// One call's log, written as four labelled sections rather than one flat
/// stream: what was called, what came in, what happened, what came back.
///
/// The file is grouped, not chronological — reading a failure means jumping
/// straight to the responses, and a timestamped jumble buries them among the
/// steps. Each section is still individually ordered, and every step keeps its
/// own timestamp, so nothing about the sequence is lost.
pub struct StepLogger {
    // The person / entity id the call is about. Unknown until the request is
    // inspected, so it starts as a placeholder and is updated via `set_id`.
    id: RefCell<String>,
    // Timestamp captured when the call started; part of the file name so
    // repeated calls for the same id never overwrite each other.
    time: String,
    // Human-readable start time used inside the file header.
    started_at: String,
    // The route label, recorded in the header for context.
    route: String,
    // Method + full path including query. Set once the request is in hand.
    endpoint: RefCell<String>,
    // Destination folder, resolved once when the call starts. Reading the env
    // var at Drop instead would let a mid-call config change split one service's
    // logs across two folders, and makes the type impossible to test without
    // mutating process-global state.
    dir: String,
    // Public origin (`https://host`) used to turn a served path into a URL that
    // can be opened straight from the log. Empty when unknown, which keeps the
    // relative path rather than inventing a host.
    base_url: RefCell<String>,
    // The sections, buffered separately and joined on Drop.
    params: RefCell<Vec<String>>,
    steps: RefCell<Vec<String>>,
    images: RefCell<Vec<String>>,
    ai_responses: RefCell<Vec<String>>,
    backend_response: RefCell<Option<String>>,
}

impl StepLogger {
    pub fn new(route: &str) -> Self {
        let now = chrono::Local::now();
        StepLogger {
            id: RefCell::new("unknown".to_string()),
            // Readable 12-hour timestamp in the file name: 20260721_01_04_39_PM.
            // Seconds are kept (not just hour_minute) so two calls for the same
            // id in the same minute don't collide and overwrite each other's log.
            time: now.format("%Y%m%d_%I_%M_%S_%p").to_string(),
            started_at: now.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            route: route.to_string(),
            endpoint: RefCell::new(String::new()),
            dir: Self::log_dir(),
            base_url: RefCell::new(String::new()),
            params: RefCell::new(Vec::new()),
            steps: RefCell::new(Vec::new()),
            images: RefCell::new(Vec::new()),
            ai_responses: RefCell::new(Vec::new()),
            backend_response: RefCell::new(None),
        }
    }

    // Same, with an explicit destination — lets tests assert the on-disk shape
    // without touching `WOW_LOG_DIR`, which is process-global and would leak
    // between tests running in parallel.
    #[cfg(test)]
    fn new_in(route: &str, dir: &str) -> Self {
        let mut this = Self::new(route);
        this.dir = dir.to_string();
        this
    }

    // Update the id once it is known (e.g. after parsing the request).
    // The final file name uses whatever id is set when the logger is dropped.
    pub fn set_id(&self, id: &str) {
        let id = id.trim();
        if !id.is_empty() {
            *self.id.borrow_mut() = id.to_string();
        }
    }

    /// The endpoint as called: method plus the full path, query included.
    ///
    /// `route:` in the header is the bare path (the viewer filters on it and
    /// shows it as a column); this is the line a reader needs to reproduce the
    /// call, which is why both are recorded.
    pub fn set_endpoint(&self, method: &str, path_and_query: &str) {
        // Redacted here rather than at the call sites: this line is derived from
        // the raw URI, so leaving it to callers means one of them eventually
        // forgets and a token lands in the file.
        let safe = match path_and_query.split_once('?') {
            Some((path, query)) => format!("{path}?{}", redact_query(query)),
            None => path_and_query.to_string(),
        };
        *self.endpoint.borrow_mut() = format!("{method} {safe}");
    }

    /// Request input. `source` names the carrier — `query`, `form`, `json`,
    /// `multipart` — because one call can have several and "which one held the
    /// bad value" is the usual question.
    pub fn params(&self, source: &str, value: &Value) {
        self.params
            .borrow_mut()
            .push(format!("{source}: {}", render(value)));
    }

    /// One step, with a millisecond-precision timestamp.
    pub fn step(&self, msg: impl AsRef<str>) {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f").to_string();
        self.steps
            .borrow_mut()
            .push(format!("[{ts}] {}", msg.as_ref()));
    }

    /// What the AI platform answered. `endpoint` is its path (`/recognize`,
    /// `/enroll`) — a verify call can hit more than one, and they are recorded
    /// in call order.
    pub fn ai_response(&self, endpoint: &str, status: u16, value: &Value) {
        self.ai_responses
            .borrow_mut()
            .push(format!("Response (AI {endpoint}) {status}: {}", render(value)));
    }

    /// What THIS service answered the caller. Last section in the file, because
    /// it is the outcome — and the thing most often being looked up.
    pub fn backend_response(&self, status: u16, value: &Value) {
        *self.backend_response.borrow_mut() =
            Some(format!("Response (backend) {status}: {}", render(value)));
    }

    /// Same, for a body that was not JSON — an HTML error page or a plain-text
    /// gateway message is still worth seeing.
    pub fn backend_response_text(&self, status: u16, body: &str) {
        *self.backend_response.borrow_mut() = Some(format!(
            "Response (backend) {status} [non-JSON]: {}",
            clip(body.to_string())
        ));
    }

    /// A file this call wrote, with the URL it can be opened at. Recorded as a
    /// step, since when it happened is part of the story.
    ///
    /// Both paths are kept on purpose: the filesystem path is what an operator
    /// needs on the box, the browsable one is what an admin needs from the log
    /// viewer. `browsable` is None when the file landed outside the served
    /// uploads folder — saying so beats inventing a URL that 404s.
    pub fn file(&self, fs_path: &str, browsable: Option<&str>) {
        match browsable {
            Some(path) => {
                let url = self.absolute(path);
                self.step(format!("File: {fs_path} | url: {url}"));
                // Also collected into the `Images:` section: the step line says
                // when the file was written, the section gathers the URLs in one
                // place so they can be opened without reading the whole log.
                self.images.borrow_mut().push(url);
            }
            None => self.step(format!(
                "File: {fs_path} | url: <not under the served uploads folder>"
            )),
        }
    }

    /// The public origin this service is reached at, e.g. `https://erp.du.ac.bd`
    /// — set once per request so `file()` can emit an openable URL.
    pub fn set_base_url(&self, base: &str) {
        *self.base_url.borrow_mut() = base.trim_end_matches('/').to_string();
    }

    /// Prefix a served path with the base origin, when one is known.
    ///
    /// Falls back to the bare path rather than guessing a host: a URL pointing
    /// at the wrong origin is worse than an honest relative path, because it
    /// looks clickable and 404s somewhere unrelated.
    fn absolute(&self, path: &str) -> String {
        let base = self.base_url.borrow();
        if base.is_empty() || !path.starts_with('/') {
            path.to_string()
        } else {
            format!("{base}{path}")
        }
    }

    fn log_dir() -> String {
        std::env::var("WOW_LOG_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "./uploads/log".to_string())
    }

    // Replace path-unfriendly characters so an odd id can't escape the log dir.
    fn sanitize(id: &str) -> String {
        let cleaned: String = id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        if cleaned.is_empty() { "unknown".to_string() } else { cleaned }
    }

    /// The file body: the four sections, in the order a reader wants them.
    ///
    /// `Params:` and `Steps:` are always emitted, even when empty — a fixed
    /// skeleton means a reader can tell "nothing was sent" from "this log is
    /// from an older build". The response sections appear only when there was
    /// one, since most calls never reach the AI platform.
    fn body(&self) -> String {
        let indent = |lines: &[String]| -> String {
            if lines.is_empty() {
                "  (none)".to_string()
            } else {
                lines
                    .iter()
                    .map(|l| format!("  {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        let mut out = String::new();
        out.push_str("Params:\n");
        out.push_str(&indent(&self.params.borrow()));
        out.push_str("\n\nSteps:\n");
        out.push_str(&indent(&self.steps.borrow()));

        // Only when the call actually wrote files — most do not.
        let images = self.images.borrow();
        if !images.is_empty() {
            out.push_str("\n\nImages:\n");
            out.push_str(&indent(&images));
        }

        for line in self.ai_responses.borrow().iter() {
            out.push_str("\n\n");
            out.push_str(line);
        }
        if let Some(line) = self.backend_response.borrow().as_ref() {
            out.push_str("\n\n");
            out.push_str(line);
        }
        out.push('\n');
        out
    }
}

impl Drop for StepLogger {
    fn drop(&mut self) {
        let dir = &self.dir;
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("StepLogger: create_dir_all {dir} failed: {e}");
            return;
        }
        let id = Self::sanitize(&self.id.borrow());
        let path = format!("{}/{}_{}.log", dir.trim_end_matches('/'), id, self.time);

        let mut content = String::new();
        content.push_str(&format!("route: {}\n", self.route));
        content.push_str(&format!("id: {}\n", self.id.borrow()));
        content.push_str(&format!("started_at: {}\n", self.started_at));
        let endpoint = self.endpoint.borrow();
        if !endpoint.is_empty() {
            content.push_str(&format!("endpoint: {endpoint}\n"));
        }
        content.push_str("----\n");
        content.push_str(&self.body());

        if let Err(e) = std::fs::write(&path, content) {
            eprintln!("StepLogger: write {path} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Unique per test, so parallel tests never share a folder.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("duerp_steplog_{}_{}", std::process::id(), tag));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_keeps_safe_ids_and_neutralises_traversal() {
        assert_eq!(StepLogger::sanitize("2020111007"), "2020111007");
        assert_eq!(StepLogger::sanitize("emp_453-20"), "emp_453-20");
        // Separators and dots become underscores, so no id can walk out of the
        // folder or fabricate a second `.log` extension.
        assert_eq!(StepLogger::sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(StepLogger::sanitize("a/b"), "a_b");
        assert_eq!(StepLogger::sanitize("///"), "___");
        assert_eq!(StepLogger::sanitize(""), "unknown");
    }

    #[test]
    fn set_id_ignores_blank_input() {
        let dir = temp_dir("setid");
        let log = StepLogger::new_in("ext-api/test", dir.to_str().unwrap());
        log.set_id("   ");
        assert_eq!(*log.id.borrow(), "unknown", "blank must not clobber the id");
        log.set_id("  ext_app_1  ");
        assert_eq!(*log.id.borrow(), "ext_app_1", "surrounding space is trimmed");
        drop(log);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn secrets_are_masked_at_every_depth_and_spelling() {
        let v = json!({
            "id": "2020111007",
            "password": "hunter2",
            "X-App-Password": "hunter2",
            "accessToken": "eyJhbGciOi",
            "nested": { "api_key": "abc", "keep": "visible" },
            "list": [{ "Authorization": "Bearer x" }, { "plain": "ok" }]
        });
        let out = serde_json::to_string(&redact(&v)).unwrap();
        for leaked in ["hunter2", "eyJhbGciOi", "abc", "Bearer x"] {
            assert!(!out.contains(leaked), "{leaked} leaked into {out}");
        }
        assert!(out.contains("2020111007"));
        assert!(out.contains("visible"));
        assert!(out.contains("ok"));
        assert!(out.contains("\"password\":\"[redacted]\""));
    }

    #[test]
    fn academic_session_is_not_mistaken_for_a_credential() {
        // "session" means the academic year here; masking it would strip the
        // most common query parameter in the ERP out of every log.
        let v = query_to_json("session=2024-25&session_id=abc&access_token=xyz");
        assert_eq!(v["session"], "2024-25", "academic session must stay readable");
        assert_eq!(v["session_id"], "[redacted]");
        assert_eq!(v["access_token"], "[redacted]");
    }

    #[test]
    fn query_is_decoded_and_masked() {
        let v = query_to_json("id=2020111007&name=two%20words&token=abc123&flag");
        assert_eq!(v["id"], "2020111007");
        assert_eq!(v["name"], "two words", "percent escapes are decoded");
        assert_eq!(v["token"], "[redacted]");
        assert_eq!(v["flag"], "", "a valueless key is still recorded");
        assert_eq!(query_to_json(""), json!({}));
    }

    #[test]
    fn oversized_payloads_are_clipped_and_say_so() {
        let big = json!({ "blob": "x".repeat(MAX_LOGGED_CHARS * 2) });
        let out = render(&big);
        assert!(out.contains("truncated"), "clip must announce itself");
        assert!(
            out.chars().count() < MAX_LOGGED_CHARS + 100,
            "clipped output stays bounded, got {} chars",
            out.chars().count()
        );
    }

    // ---- the sectioned file layout --------------------------------------

    #[test]
    fn file_is_written_in_endpoint_params_steps_responses_order() {
        let dir = temp_dir("sections");
        {
            let log = StepLogger::new_in(
                "ext-api/wow-attendance/verify",
                dir.to_str().unwrap(),
            );
            log.set_id("2020111007");
            log.set_endpoint("POST", "/ext-api/wow-attendance/verify?id=2020111007");
            log.params("query", &json!({ "id": "2020111007" }));
            log.params("form", &json!({ "device_info": "{...}" }));
            log.step("request received");
            log.file(
                "/srv/uploads/wow_attendance/live/a.jpg",
                Some("/uploads/wow_attendance/live/a.jpg"),
            );
            log.ai_response("/recognize", 200, &json!({ "recognized": true }));
            log.backend_response(200, &json!({ "success": true }));
        }

        let name = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".log"))
            .expect("one file per call");
        let content = std::fs::read_to_string(dir.join(&name)).unwrap();

        // Header: the viewer parses these three, plus the new endpoint line.
        let (header, body) = content.split_once("\n----\n").expect("separator present");
        assert!(header.contains("route: ext-api/wow-attendance/verify"));
        assert!(header.contains("id: 2020111007"));
        assert!(header.contains("started_at: "));
        assert!(
            header.contains("endpoint: POST /ext-api/wow-attendance/verify?id=2020111007"),
            "endpoint belongs in the header:\n{header}"
        );

        // Sections appear once each, in the requested order.
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("missing {needle} in:\n{body}"));
        let (params, steps, ai, backend) = (
            at("Params:"),
            at("Steps:"),
            at("Response (AI /recognize) 200:"),
            at("Response (backend) 200:"),
        );
        assert!(params < steps, "Params before Steps");
        assert!(steps < ai, "Steps before the AI response");
        assert!(ai < backend, "AI response before the backend response");

        // Contents land under the right heading.
        assert!(body.contains(r#"  query: {"id":"2020111007"}"#), "{body}");
        assert!(body.contains(r#"  form: {"device_info":"{...}"}"#), "{body}");
        assert!(body.contains("url: /uploads/wow_attendance/live/a.jpg"));
        assert!(body.contains(r#"Response (AI /recognize) 200: {"recognized":true}"#));
        assert!(body.contains(r#"Response (backend) 200: {"success":true}"#));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_sections_are_explicit_rather_than_missing() {
        // A call rejected before anything was parsed still gets the skeleton, so
        // "nothing was sent" is distinguishable from "older build, no sections".
        let dir = temp_dir("empty");
        {
            let log = StepLogger::new_in("ext-api/test", dir.to_str().unwrap());
            log.step("rejected immediately");
        }
        let name = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".log")).unwrap();
        let content = std::fs::read_to_string(dir.join(&name)).unwrap();

        assert!(content.contains("Params:\n  (none)"), "{content}");
        assert!(content.contains("[") && content.contains("rejected immediately"));
        // No AI call and no response recorded — those headings stay absent.
        assert!(!content.contains("Response (AI"), "{content}");
        assert!(!content.contains("Response (backend)"), "{content}");
        // And no endpoint line when it was never set.
        assert!(!content.contains("endpoint:"), "{content}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn several_ai_calls_are_kept_in_call_order() {
        let dir = temp_dir("multiai");
        {
            let log = StepLogger::new_in("ext-api/test", dir.to_str().unwrap());
            log.ai_response("/recognize", 200, &json!({ "step": 1 }));
            log.ai_response("/enroll", 502, &json!({ "step": 2 }));
        }
        let name = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".log")).unwrap();
        let content = std::fs::read_to_string(dir.join(&name)).unwrap();
        let first = content.find("Response (AI /recognize) 200").unwrap();
        let second = content.find("Response (AI /enroll) 502").unwrap();
        assert!(first < second, "AI replies keep call order:\n{content}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn images_get_an_openable_url_and_their_own_section() {
        let dir = temp_dir("images");
        {
            let log = StepLogger::new_in("ext-api/wow-attendance/enroll", dir.to_str().unwrap());
            log.set_base_url("https://erp.du.ac.bd/");
            log.step("parsing multipart body");
            log.file("/srv/uploads/wow_attendance/enrolled/a.jpg", Some("/uploads/wow_attendance/enrolled/a.jpg"));
            log.file("/srv/uploads/wow_attendance/enrolled/b.jpg", Some("/uploads/wow_attendance/enrolled/b.jpg"));
        }
        let name = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".log")).unwrap();
        let content = std::fs::read_to_string(dir.join(&name)).unwrap();

        // Absolute, and the trailing slash on the base does not double up.
        assert!(
            content.contains("https://erp.du.ac.bd/uploads/wow_attendance/enrolled/a.jpg"),
            "{content}"
        );
        assert!(!content.contains("bd//uploads"), "double slash in URL:\n{content}");

        // Both in the section, in the order they were written, after Steps.
        let images_at = content.find("Images:").expect("Images section");
        assert!(content.find("Steps:").unwrap() < images_at, "Images comes after Steps");
        let section = &content[images_at..];
        let a = section.find("/enrolled/a.jpg").unwrap();
        let b = section.find("/enrolled/b.jpg").unwrap();
        assert!(a < b, "images keep write order");

        // The step line still records WHEN, alongside the URL.
        assert!(content.contains("File: /srv/uploads/wow_attendance/enrolled/a.jpg | url: https://"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_a_base_url_the_path_stays_relative_rather_than_guessing() {
        // A wrong origin looks clickable and 404s somewhere unrelated, which is
        // worse than an honest relative path.
        let dir = temp_dir("nobase");
        {
            let log = StepLogger::new_in("ext-api/test", dir.to_str().unwrap());
            log.file("/srv/uploads/wow_attendance/live/a.jpg", Some("/uploads/wow_attendance/live/a.jpg"));
        }
        let name = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".log")).unwrap();
        let content = std::fs::read_to_string(dir.join(&name)).unwrap();
        assert!(content.contains("url: /uploads/wow_attendance/live/a.jpg"), "{content}");
        assert!(!content.contains("http"), "no host was invented:\n{content}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_outside_the_served_folder_says_so_instead_of_linking() {
        let dir = temp_dir("unserved");
        {
            let log = StepLogger::new_in("ext-api/test", dir.to_str().unwrap());
            log.set_base_url("https://erp.du.ac.bd");
            log.file("/tmp/scratch/a.jpg", None);
        }
        let name = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".log")).unwrap();
        let content = std::fs::read_to_string(dir.join(&name)).unwrap();
        assert!(content.contains("not under the served uploads folder"), "{content}");
        // Nothing unopenable lands in the Images section.
        assert!(!content.contains("Images:"), "{content}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_endpoint_line_does_not_leak_what_params_masked() {
        // Regression: the endpoint line is built from the raw URI, so it was
        // showing `token=abc123` in full while Params showed `[redacted]`.
        assert_eq!(
            redact_query("session=2024-25&token=abc123&id=7"),
            "session=2024-25&token=[redacted]&id=7"
        );
        // Percent-encoded key names are matched after decoding.
        assert_eq!(redact_query("access%5Ftoken=xyz"), "access%5Ftoken=[redacted]");
        // Shape is preserved for valueless and empty inputs.
        assert_eq!(redact_query("flag"), "flag");
        assert_eq!(redact_query(""), "");

        let dir = temp_dir("endpoint_redact");
        {
            let log = StepLogger::new_in("ext-api/test", dir.to_str().unwrap());
            log.set_endpoint("POST", "/ext-api/test?id=7&token=abc123");
        }
        let name = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".log")).unwrap();
        let content = std::fs::read_to_string(dir.join(&name)).unwrap();
        assert!(!content.contains("abc123"), "token leaked:\n{content}");
        assert!(content.contains("endpoint: POST /ext-api/test?id=7&token=[redacted]"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drop_creates_the_destination_folder_when_missing() {
        // First call after a deploy lands on a folder that does not exist yet;
        // the log must still be written rather than silently dropped.
        let base = temp_dir("mkdir");
        let nested = base.join("deep").join("log");
        assert!(!nested.exists());

        drop(StepLogger::new_in("ext-api/test", nested.to_str().unwrap()));

        let files: Vec<_> = std::fs::read_dir(&nested)
            .expect("folder created on demand")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1, "the log survived the missing folder");
        std::fs::remove_dir_all(&base).ok();
    }
}
