//! bougie-collector: first-party ingest for bougie's opt-in telemetry
//! and user-initiated diagnostic reports.
//!
//! The contract is bougie's TELEMETRY.md: every accepted field and
//! value is enumerated there; anything else is dropped, not stored.
//! IP addresses are used in memory for rate limiting only and are
//! never written anywhere (nginx in front runs with access_log off —
//! see hosts/telemetry/configuration.nix).
//!
//! The growable vocabularies (commands, outcomes, extensions,
//! services) come straight from the published `bougie-telemetry`
//! crate — one source of truth with the client; bumping that
//! dependency is how the allowlist widens. Only the tiny fixed sets
//! (os/arch/libc/…) live here.

use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use rusqlite::Connection;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_BODY: usize = 256 * 1024;
/// Diagnose reports carry service-log tails since schema 2 (bougie's
/// DIAGNOSE_PLAN.md); the client budgets the markdown to 384 KiB, so
/// 1 MiB leaves comfortable JSON-escaping headroom. Route-scoped —
/// `/v1/batch` stays at [`MAX_BODY`].
const DIAGNOSE_MAX_BODY: usize = 1024 * 1024;
/// Sanity cap on the `report_md` string itself inside a schema-2
/// diagnose payload.
const REPORT_MD_MAX: usize = 512 * 1024;
const MAX_DECOMPRESSED: u64 = 1024 * 1024; // zip-bomb guard
const RATE_LIMIT_PER_HOUR: u32 = 120;
const RAW_RETENTION_DAYS: i64 = 400;
const DIAGNOSE_RETENTION_DAYS: i64 = 180;
const BACKUPS_KEPT: usize = 7;

// ---- vocabularies ----

use bougie_telemetry::event::{COMMAND_VOCAB as COMMANDS, OUTCOME_VOCAB as OUTCOMES};
use bougie_telemetry::probe::{EXTENSION_VOCAB as EXTENSIONS, SERVICE_VOCAB as SERVICES};

const OSES: &[&str] = &["linux", "macos", "windows", "other"];
const ARCHES: &[&str] = &["x86_64", "aarch64", "other"];
const LIBCS: &[&str] = &["gnu", "musl", "none"];
const INSTALL_METHODS: &[&str] = &["installer", "cargo", "docker", "unknown"];
const BUCKETS: &[&str] = &["0", "1-5", "6-15", "16-40", "41-100", "100+"];
const PHP_SOURCES: &[&str] = &["managed", "system"];

const ENVELOPE_KEYS: &[&str] = &[
    "schema", "event", "ts", "install_id", "invocation", "bougie_version",
    "build_sha", "os", "arch", "libc", "ci", "install_method",
];
const COMMAND_KEYS: &[&str] = &[
    "name", "duration_ms", "outcome", "exit_code", "resolve_ms", "vendor_ms",
    "autoload_ms", "download_bytes", "cache_hit_pct", "packages_installed",
    "php_version", "php_flavor", "php_source", "extensions", "services",
    "direct_deps", "total_deps",
];
const CRASH_KEYS: &[&str] = &["command", "fingerprint", "frames", "message"];

// ---- state ----

struct App {
    db: Mutex<Connection>,
    limiter: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    db_path: PathBuf,
}

fn main() {
    let db_path = PathBuf::from(
        std::env::var("BOUGIE_COLLECTOR_DB")
            .unwrap_or_else(|_| "/var/lib/bougie-collector/collector.db".into()),
    );
    let listen: SocketAddr = std::env::var("BOUGIE_COLLECTOR_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .expect("BOUGIE_COLLECTOR_LISTEN must be host:port");

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).expect("creating db dir");
    }
    let db = open_db(&db_path).expect("opening database");
    let app = Arc::new(App {
        db: Mutex::new(db),
        limiter: Mutex::new(HashMap::new()),
        db_path,
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(serve(app, listen));
}

async fn serve(app: Arc<App>, listen: SocketAddr) {
    let router = Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/", get(dashboard))
        .route("/data.json", get(data))
        .route("/v1/batch", post(batch))
        .route(
            "/v1/diagnose",
            post(diagnose).layer(DefaultBodyLimit::max(DIAGNOSE_MAX_BODY)),
        )
        // /admin/* is reachable only through nginx's basic-auth
        // location (the collector listens on loopback); it does no
        // auth of its own. See hosts/telemetry/configuration.nix.
        .route("/admin/diagnose", get(admin_list))
        .route("/admin/diagnose/{id}", get(admin_detail))
        .route("/admin/diagnose/{id}/raw", get(admin_raw))
        .route("/admin/diagnose/{id}/delete", post(admin_delete))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(app.clone());

    // Daily maintenance: retention pruning + on-disk backup rotation.
    tokio::spawn(async move {
        loop {
            maintain(&app);
            tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
        }
    });

    let listener = tokio::net::TcpListener::bind(listen).await.expect("bind");
    eprintln!("bougie-collector listening on {listen}");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .expect("server");
}

fn open_db(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let db = Connection::open(path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS command_events (
            received_day TEXT NOT NULL, ts TEXT, install_id TEXT, invocation TEXT,
            version TEXT, build_sha TEXT, os TEXT, arch TEXT, libc TEXT, ci INT,
            install_method TEXT, name TEXT, duration_ms INT, outcome TEXT,
            exit_code INT, resolve_ms INT, vendor_ms INT, packages_installed INT,
            php_version TEXT, php_flavor TEXT, php_source TEXT, extensions TEXT,
            services TEXT, direct_deps TEXT, total_deps TEXT, autoload_ms INT,
            download_bytes INT, cache_hit_pct INT
        );
        CREATE INDEX IF NOT EXISTS idx_cmd_day ON command_events(received_day);
        CREATE TABLE IF NOT EXISTS crash_events (
            received_day TEXT NOT NULL, ts TEXT, install_id TEXT, invocation TEXT,
            version TEXT, build_sha TEXT, os TEXT, arch TEXT, libc TEXT, ci INT,
            install_method TEXT, command TEXT, fingerprint TEXT, frames TEXT,
            message TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_crash_day ON crash_events(received_day);
        CREATE TABLE IF NOT EXISTS diagnose_reports (
            id TEXT PRIMARY KEY, received_at INT NOT NULL, body TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS daily_summary (
            day TEXT PRIMARY KEY, events INT NOT NULL, installs INT NOT NULL,
            crashes INT NOT NULL, ci_events INT, ci_installs INT,
            interactive_installs INT, ci_crashes INT
        );
        CREATE TABLE IF NOT EXISTS daily_dim (
            day TEXT NOT NULL, dim TEXT NOT NULL, key TEXT NOT NULL,
            count INT NOT NULL, PRIMARY KEY (day, dim, key)
        );",
    )?;
    // Schema v2 migration (2026-07: perf fields). ALTER fails once the
    // column exists; that failure is the idempotence mechanism.
    for column in ["autoload_ms INT", "download_bytes INT", "cache_hit_pct INT"] {
        let _ = db.execute(&format!("ALTER TABLE command_events ADD COLUMN {column}"), []);
    }
    // Schema v3 migration (2026-07: CI split in the daily rollup).
    // Nullable on purpose: days whose raw rows were pruned before this
    // migration keep NULL, which /data.json maps to "unknown split".
    for column in
        ["ci_events INT", "ci_installs INT", "interactive_installs INT", "ci_crashes INT"]
    {
        let _ = db.execute(&format!("ALTER TABLE daily_summary ADD COLUMN {column}"), []);
    }
    Ok(db)
}

// ---- rollups (phase 6) ----
//
// Aggregates survive the 400-day raw retention: days whose raw rows
// were pruned keep their frozen rollup rows, so the public dashboard's
// history is indefinite while raw events stay bounded. Only days that
// still have raw data are ever recomputed.

fn rollup_all(db: &Connection) {
    let mut days: Vec<String> = Vec::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT DISTINCT received_day FROM command_events
         UNION SELECT DISTINCT received_day FROM crash_events",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            days.extend(rows.flatten());
        }
    }
    for day in days {
        let _ = rollup_day(db, &day);
    }
}

fn rollup_day(db: &Connection, day: &str) -> rusqlite::Result<()> {
    let count = |sql: &str| -> rusqlite::Result<i64> {
        db.query_row(sql, [day], |r| r.get(0))
    };
    let events = count("SELECT count(*) FROM command_events WHERE received_day = ?1")?;
    let installs = count(
        "SELECT count(DISTINCT install_id) FROM command_events WHERE received_day = ?1",
    )?;
    let crashes = count("SELECT count(*) FROM crash_events WHERE received_day = ?1")?;
    if events == 0 && crashes == 0 {
        return Ok(());
    }
    // CI split: ephemeral runners mint a fresh install_id per run, so
    // the interactive distinct count is computed directly rather than
    // derived by subtraction.
    let ci_events =
        count("SELECT count(*) FROM command_events WHERE received_day = ?1 AND ci = 1")?;
    let ci_installs = count(
        "SELECT count(DISTINCT install_id) FROM command_events
         WHERE received_day = ?1 AND ci = 1",
    )?;
    let interactive_installs = count(
        "SELECT count(DISTINCT install_id) FROM command_events
         WHERE received_day = ?1 AND ci = 0",
    )?;
    let ci_crashes =
        count("SELECT count(*) FROM crash_events WHERE received_day = ?1 AND ci = 1")?;
    db.execute(
        "INSERT OR REPLACE INTO daily_summary
         (day, events, installs, crashes, ci_events, ci_installs,
          interactive_installs, ci_crashes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            day,
            events,
            installs,
            crashes,
            ci_events,
            ci_installs,
            interactive_installs,
            ci_crashes
        ],
    )?;
    db.execute("DELETE FROM daily_dim WHERE day = ?1", [day])?;

    let grouped: &[(&str, &str)] = &[
        ("command", "SELECT name, count(*) FROM command_events WHERE received_day = ?1 GROUP BY 1"),
        ("outcome", "SELECT outcome, count(*) FROM command_events WHERE received_day = ?1 GROUP BY 1"),
        // command × outcome cross for failures only: which verbs fail,
        // and into which category — the `other` share per verb is what
        // drives the bougie error-taxonomy widening.
        ("failure", "SELECT name || ' → ' || outcome, count(*) FROM command_events WHERE received_day = ?1 AND outcome != 'ok' GROUP BY 1"),
        // Same cross weighted by distinct installs per day. Summed over
        // days this reads as install-days: a runaway retry loop counts
        // once per day instead of once per event (a real machine burst
        // 1,555 events into one hour and buried every other row).
        ("failure-installs", "SELECT name || ' → ' || outcome, count(DISTINCT install_id) FROM command_events WHERE received_day = ?1 AND outcome != 'ok' GROUP BY 1"),
        ("version", "SELECT version, count(*) FROM command_events WHERE received_day = ?1 GROUP BY 1"),
        ("platform", "SELECT os || '/' || arch || '/' || libc, count(*) FROM command_events WHERE received_day = ?1 GROUP BY 1"),
        ("ci", "SELECT CASE WHEN ci = 1 THEN 'ci' ELSE 'interactive' END, count(*) FROM command_events WHERE received_day = ?1 GROUP BY 1"),
        ("php", "SELECT php_version, count(*) FROM command_events WHERE received_day = ?1 AND php_version IS NOT NULL GROUP BY 1"),
        ("crash", "SELECT fingerprint || ' ' || command, count(*) FROM crash_events WHERE received_day = ?1 GROUP BY 1"),
    ];
    for (dim, sql) in grouped {
        let mut stmt = db.prepare(sql)?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([day], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .flatten()
            .collect();
        for (key, n) in rows {
            db.execute(
                "INSERT OR REPLACE INTO daily_dim VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![day, dim, key, n],
            )?;
        }
    }
    // Comma-joined vocab lists flatten into per-name counts.
    for (dim, column) in [("extension", "extensions"), ("service", "services")] {
        let mut stmt = db.prepare(&format!(
            "SELECT {column} FROM command_events
             WHERE received_day = ?1 AND {column} IS NOT NULL AND {column} != ''"
        ))?;
        let lists: Vec<String> =
            stmt.query_map([day], |r| r.get::<_, String>(0))?.flatten().collect();
        let mut counts: HashMap<&str, i64> = HashMap::new();
        for list in &lists {
            for name in list.split(',') {
                *counts.entry(name).or_default() += 1;
            }
        }
        for (key, n) in counts {
            db.execute(
                "INSERT OR REPLACE INTO daily_dim VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![day, dim, key, n],
            )?;
        }
    }
    Ok(())
}

// ---- rate limiting (in-memory only; the IP is never persisted) ----

fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    // nginx on the same box terminates TLS and proxies with
    // X-Forwarded-For; trust it only when the peer is loopback.
    if peer.ip().is_loopback() {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = xff.split(',').next() {
                if let Ok(ip) = first.trim().parse() {
                    return ip;
                }
            }
        }
    }
    peer.ip()
}

fn rate_limited(app: &App, ip: IpAddr) -> bool {
    let mut limiter = app.limiter.lock().unwrap();
    let now = Instant::now();
    // Cheap periodic cleanup: drop expired windows once the map grows.
    if limiter.len() > 10_000 {
        limiter.retain(|_, (start, _)| now.duration_since(*start).as_secs() < 3600);
    }
    let entry = limiter.entry(ip).or_insert((now, 0));
    if now.duration_since(entry.0).as_secs() >= 3600 {
        *entry = (now, 0);
    }
    entry.1 += 1;
    entry.1 > RATE_LIMIT_PER_HOUR
}

// ---- /v1/batch ----

async fn batch(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    if rate_limited(&app, client_ip(&headers, peer)) {
        return StatusCode::TOO_MANY_REQUESTS;
    }
    let gzipped = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    let text = if gzipped {
        let mut out = String::new();
        let mut decoder = flate2::read::GzDecoder::new(&body[..]).take(MAX_DECOMPRESSED);
        if decoder.read_to_string(&mut out).is_err() {
            // Undecodable input still gets 204: the client must never
            // learn to retry harder (TELEMETRY_PLAN.md collector spec).
            return StatusCode::NO_CONTENT;
        }
        out
    } else {
        String::from_utf8_lossy(&body).into_owned()
    };

    let day = today();
    let db = app.db.lock().unwrap();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        // Invalid lines are dropped silently — the allowlist is the
        // contract, not a negotiation.
        let _ = insert_event(&db, &day, &value);
    }
    StatusCode::NO_CONTENT
}

fn insert_event(db: &Connection, day: &str, v: &serde_json::Value) -> Option<()> {
    let obj = v.as_object()?;
    if obj.get("schema")?.as_u64()? != 1 {
        return None;
    }
    let event = obj.get("event")?.as_str()?;
    let (extra_keys, is_command) = match event {
        "command" => (COMMAND_KEYS, true),
        "crash" => (CRASH_KEYS, false),
        _ => return None,
    };
    // Field allowlist: any key outside the contract kills the line.
    if !obj
        .keys()
        .all(|k| ENVELOPE_KEYS.contains(&k.as_str()) || extra_keys.contains(&k.as_str()))
    {
        return None;
    }

    // Envelope.
    let ts = obj.get("ts")?.as_str().filter(|t| hour_ts_ok(t))?;
    let install_id = obj.get("install_id")?.as_str().filter(|s| id_ok(s))?;
    let invocation = obj.get("invocation")?.as_str().filter(|s| uuid_ok(s))?;
    let version = obj.get("bougie_version")?.as_str().filter(|s| version_ok(s))?;
    let build_sha = match obj.get("build_sha") {
        None => None,
        Some(s) => Some(s.as_str().filter(|s| sha_ok(s))?),
    };
    let os = obj.get("os")?.as_str().filter(|s| OSES.contains(s))?;
    let arch = obj.get("arch")?.as_str().filter(|s| ARCHES.contains(s))?;
    let libc = obj.get("libc")?.as_str().filter(|s| LIBCS.contains(s))?;
    let ci = obj.get("ci")?.as_bool()?;
    let method = obj
        .get("install_method")?
        .as_str()
        .filter(|s| INSTALL_METHODS.contains(s))?;

    if is_command {
        let name = obj.get("name")?.as_str().filter(|s| COMMANDS.contains(s))?;
        let duration = obj.get("duration_ms")?.as_u64()?;
        let outcome = obj.get("outcome")?.as_str().filter(|s| OUTCOMES.contains(s))?;
        let exit_code = obj.get("exit_code")?.as_u64().filter(|c| *c <= 255)?;
        let opt_u64 = |k: &str| -> Option<Option<u64>> {
            match obj.get(k) {
                None => Some(None),
                Some(v) => v.as_u64().map(Some),
            }
        };
        let resolve_ms = opt_u64("resolve_ms")?;
        let vendor_ms = opt_u64("vendor_ms")?;
        let autoload_ms = opt_u64("autoload_ms")?;
        let download_bytes = opt_u64("download_bytes")?;
        let cache_hit_pct = opt_u64("cache_hit_pct")?.filter(|p| *p <= 100);
        // A >100 percentage is a noncompliant client: drop the field,
        // keep the event.
        let cache_hit_pct = match obj.get("cache_hit_pct") {
            Some(_) if cache_hit_pct.is_none() => None,
            _ => cache_hit_pct,
        };
        let packages_installed = opt_u64("packages_installed")?;
        let opt_vocab = |k: &str, vocab: &[&str]| -> Option<Option<String>> {
            match obj.get(k) {
                None => Some(None),
                Some(v) => {
                    let s = v.as_str()?;
                    vocab.contains(&s).then(|| Some(s.to_owned()))
                }
            }
        };
        let php_source = opt_vocab("php_source", PHP_SOURCES)?;
        let direct_deps = opt_vocab("direct_deps", BUCKETS)?;
        let total_deps = opt_vocab("total_deps", BUCKETS)?;
        let php_version = match obj.get("php_version") {
            None => None,
            Some(v) => Some(v.as_str().filter(|s| php_version_ok(s))?.to_owned()),
        };
        let php_flavor = match obj.get("php_flavor") {
            None => None,
            Some(v) => Some(v.as_str().filter(|s| token_ok(s))?.to_owned()),
        };
        let name_list = |k: &str, vocab: &[&str]| -> Option<Option<String>> {
            match obj.get(k) {
                None => Some(None),
                Some(v) => {
                    let arr = v.as_array()?;
                    if arr.len() > 32 {
                        return None;
                    }
                    let mut names = Vec::with_capacity(arr.len());
                    for item in arr {
                        let s = item.as_str()?;
                        if !vocab.contains(&s) {
                            return None;
                        }
                        names.push(s);
                    }
                    Some(Some(names.join(",")))
                }
            }
        };
        let extensions = name_list("extensions", EXTENSIONS)?;
        let services = name_list("services", SERVICES)?;

        db.execute(
            "INSERT INTO command_events VALUES
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28)",
            rusqlite::params![
                day, ts, install_id, invocation, version, build_sha, os, arch, libc,
                ci, method, name, duration, outcome, exit_code, resolve_ms, vendor_ms,
                packages_installed, php_version, php_flavor, php_source, extensions,
                services, direct_deps, total_deps, autoload_ms, download_bytes,
                cache_hit_pct
            ],
        )
        .ok()?;
    } else {
        let command = obj.get("command")?.as_str().filter(|s| COMMANDS.contains(s))?;
        let fingerprint = obj.get("fingerprint")?.as_str().filter(|s| fp_ok(s))?;
        let frames_val = obj.get("frames")?.as_array()?;
        if frames_val.is_empty() || frames_val.len() > 40 {
            return None;
        }
        let mut frames = Vec::with_capacity(frames_val.len());
        for f in frames_val {
            let s = f.as_str()?;
            if !frame_ok(s) {
                return None;
            }
            frames.push(s);
        }
        // The client scrubbed already; a path-shaped message here means
        // a noncompliant client — drop the message, keep the crash.
        let message = match obj.get("message") {
            None => None,
            Some(v) => {
                let s = v.as_str()?;
                (s.chars().count() <= 200 && !s.contains('/') && !s.contains('\\'))
                    .then(|| s.to_owned())
            }
        };
        db.execute(
            "INSERT INTO crash_events VALUES
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![
                day, ts, install_id, invocation, version, build_sha, os, arch, libc,
                ci, method, command, fingerprint, frames.join("\n"), message
            ],
        )
        .ok()?;
    }
    Some(())
}

// ---- /v1/diagnose ----

async fn diagnose(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    let reject = |code| (code, axum::Json(serde_json::json!({})));
    if rate_limited(&app, client_ip(&headers, peer)) {
        return reject(StatusCode::TOO_MANY_REQUESTS);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return reject(StatusCode::BAD_REQUEST);
    };
    if !diagnose_shape_ok(&value) {
        return reject(StatusCode::BAD_REQUEST);
    }
    let db = app.db.lock().unwrap();
    match diagnose_insert(&db, &value) {
        Some(id) => (StatusCode::OK, axum::Json(serde_json::json!({ "id": id }))),
        None => reject(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Accepted diagnose shapes. Both are user-reviewed correspondence,
/// not allowlisted telemetry, so the check stays loose:
///  - schema 1: the legacy free-form JSON report;
///  - schema 2: an envelope whose `report_md` string IS the report
///    (the user-edited Markdown, byte-for-byte what they reviewed).
fn diagnose_shape_ok(value: &serde_json::Value) -> bool {
    match value.get("schema_version").and_then(|v| v.as_u64()) {
        Some(1) => true,
        Some(2) => value
            .get("report_md")
            .and_then(|v| v.as_str())
            .is_some_and(|md| !md.trim().is_empty() && md.len() <= REPORT_MD_MAX),
        _ => false,
    }
}

fn diagnose_insert(db: &Connection, value: &serde_json::Value) -> Option<String> {
    let suffix: String = db
        .query_row("SELECT lower(hex(randomblob(4)))", [], |r| r.get(0))
        .unwrap_or_else(|_| "00000000".into());
    let id = format!("diag-{suffix}");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    db.execute(
        "INSERT INTO diagnose_reports (id, received_at, body) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, now, value.to_string()],
    )
    .ok()?;
    Some(id)
}

// ---- /admin/diagnose: the maintainer's report viewer ----
//
// Auth belongs to nginx (a basic-auth `location /admin/` in
// configuration.nix); the collector listens on loopback only, so
// anything reaching these handlers came through it. Responses are
// `no-store` — report content must never land in a shared cache.

const NO_STORE: (&str, &str) = ("cache-control", "no-store");
const HTML_UTF8: (&str, &str) = ("content-type", "text/html; charset=utf-8");
const ADMIN_LIST_LIMIT: usize = 200;

/// Shared page skeleton: self-contained, dark-mode aware, no external
/// assets (same stance as dashboard.html).
const ADMIN_STYLE: &str = "<!doctype html><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<style>\
body{font:14px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:72rem;\
padding:0 1rem;color:#1a1a2e;background:#fff}\
a{color:#3b5bdb}\
@media(prefers-color-scheme:dark){body{color:#e6e6f0;background:#15151f}a{color:#9bb4ff}}\
table{border-collapse:collapse;width:100%}\
td,th{text-align:left;padding:.35rem .6rem;border-bottom:1px solid #8884;\
vertical-align:top}\
pre{white-space:pre-wrap;overflow-x:auto;background:#8881;padding:1rem;\
border-radius:6px}\
form{display:inline}button{cursor:pointer}\
.muted{opacity:.65}\
</style>";

struct ReportRow {
    id: String,
    received_at: u64,
    body: serde_json::Value,
}

fn list_reports(db: &Connection, limit: usize) -> Vec<ReportRow> {
    let Ok(mut stmt) = db.prepare(
        "SELECT id, received_at, body FROM diagnose_reports
         ORDER BY received_at DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .map(|(id, received_at, body)| ReportRow {
            id,
            received_at: received_at.max(0) as u64,
            body: serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        })
        .collect()
}

fn load_report(db: &Connection, id: &str) -> Option<ReportRow> {
    db.query_row(
        "SELECT id, received_at, body FROM diagnose_reports WHERE id = ?1",
        [id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
    )
    .ok()
    .map(|(id, received_at, body)| ReportRow {
        id,
        received_at: received_at.max(0) as u64,
        body: serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
    })
}

fn delete_report(db: &Connection, id: &str) -> bool {
    db.execute("DELETE FROM diagnose_reports WHERE id = ?1", [id])
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// One-line list summary, derived at render time from the stored
/// body — so a report whose author redacted the command line in the
/// editor shows the redacted form here too, never a structured copy.
fn summarize(body: &serde_json::Value) -> String {
    let text = if let Some(md) = body.get("report_md").and_then(|v| v.as_str()) {
        md.lines()
            .find_map(|l| l.strip_prefix("command:"))
            .map(|rest| rest.trim().to_owned())
            .or_else(|| {
                md.lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_owned)
            })
            .unwrap_or_default()
    } else {
        // Schema 1: the structured report's failure argv.
        body.pointer("/failure/argv")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")
            })
            .unwrap_or_default()
    };
    text.chars().take(120).collect()
}

/// `bougie 0.45.0 · linux/x86_64/gnu` — both schemas carry these
/// top-level envelope facts.
fn report_meta(body: &serde_json::Value) -> String {
    let s = |k: &str| body.get(k).and_then(|v| v.as_str()).unwrap_or("?");
    format!(
        "bougie {} · {}/{}/{}",
        s("bougie_version"),
        s("os"),
        s("arch"),
        s("libc")
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Seconds since epoch → `YYYY-MM-DD HH:MM:SS` UTC (Hinnant civil
/// date, same math as [`today`]).
fn utc_datetime(secs: u64) -> String {
    let secs = secs as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let tod = secs.rem_euclid(86_400);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        if m <= 2 { y + 1 } else { y },
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn delete_form(id: &str) -> String {
    format!(
        "<form method=\"post\" action=\"/admin/diagnose/{id}/delete\" \
         onsubmit=\"return confirm('delete {id}?')\"><button>delete</button></form>"
    )
}

async fn admin_list(State(app): State<Arc<App>>) -> Response {
    let rows = {
        let db = app.db.lock().unwrap();
        list_reports(&db, ADMIN_LIST_LIMIT)
    };
    let mut html = format!(
        "{ADMIN_STYLE}<title>bougie diagnose reports</title>\
         <h1>diagnose reports</h1>\
         <p class=\"muted\">{} shown (newest first, retention 180 days)</p>\
         <table><tr><th>id</th><th>received (UTC)</th><th>build</th>\
         <th>summary</th><th></th></tr>",
        rows.len()
    );
    for row in &rows {
        html.push_str(&format!(
            "<tr><td><a href=\"/admin/diagnose/{id}\">{id}</a></td>\
             <td>{when}</td><td>{meta}</td><td>{summary}</td><td>{del}</td></tr>",
            id = row.id,
            when = utc_datetime(row.received_at),
            meta = escape_html(&report_meta(&row.body)),
            summary = escape_html(&summarize(&row.body)),
            del = delete_form(&row.id),
        ));
    }
    html.push_str("</table>");
    ([NO_STORE, HTML_UTF8], html).into_response()
}

async fn admin_detail(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let row = {
        let db = app.db.lock().unwrap();
        load_report(&db, &id)
    };
    let Some(row) = row else {
        return ([NO_STORE], StatusCode::NOT_FOUND).into_response();
    };
    // Schema 2 renders the markdown verbatim in a <pre> — monospace is
    // the right reading mode for log tails, and rendering markdown
    // would only obscure exactness. Schema 1 pretty-prints the JSON.
    let content = match row.body.get("report_md").and_then(|v| v.as_str()) {
        Some(md) => md.to_owned(),
        None => serde_json::to_string_pretty(&row.body).unwrap_or_default(),
    };
    let html = format!(
        "{ADMIN_STYLE}<title>{id}</title>\
         <p><a href=\"/admin/diagnose\">&larr; all reports</a></p>\
         <h1>{id}</h1>\
         <p class=\"muted\">received {when} UTC · {meta} · \
         <a href=\"/admin/diagnose/{id}/raw\">raw json</a></p>\
         <p>{del}</p>\
         <pre>{body}</pre>",
        id = row.id,
        when = utc_datetime(row.received_at),
        meta = escape_html(&report_meta(&row.body)),
        del = delete_form(&row.id),
        body = escape_html(&content),
    );
    ([NO_STORE, HTML_UTF8], html).into_response()
}

async fn admin_raw(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let row = {
        let db = app.db.lock().unwrap();
        load_report(&db, &id)
    };
    match row {
        Some(row) => (
            [NO_STORE, ("content-type", "application/json")],
            row.body.to_string(),
        )
            .into_response(),
        None => ([NO_STORE], StatusCode::NOT_FOUND).into_response(),
    }
}

/// Honors TELEMETRY.md's "deleted on request by report id" without
/// sqlite gymnastics on the box.
async fn admin_delete(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let deleted = {
        let db = app.db.lock().unwrap();
        delete_report(&db, &id)
    };
    if deleted {
        Redirect::to("/admin/diagnose").into_response()
    } else {
        ([NO_STORE], StatusCode::NOT_FOUND).into_response()
    }
}

// ---- public dashboard (phase 6): aggregates only, nothing raw ----

async fn dashboard() -> impl axum::response::IntoResponse {
    (
        [
            ("content-type", "text/html; charset=utf-8"),
            ("cache-control", "public, max-age=600"),
        ],
        include_str!("dashboard.html"),
    )
}

/// Aggregate export the dashboard renders. Contains only rollup data:
/// daily totals and closed-vocabulary distributions — never ids, never
/// raw events, never anything from diagnose reports.
async fn data(State(app): State<Arc<App>>) -> impl axum::response::IntoResponse {
    let db = app.db.lock().unwrap();
    // Keep "today" fresh: the daily loop only touches it once a day.
    let _ = rollup_day(&db, &today());

    let mut days: Vec<serde_json::Value> = Vec::new();
    // Pre-migration frozen days have NULL split columns: treat them as
    // "no CI traffic" (true in practice — the split predates any CI
    // integration shipping) so the dashboard math stays total-safe.
    if let Ok(mut stmt) = db.prepare(
        "SELECT day, events, installs, crashes,
                COALESCE(ci_events, 0), COALESCE(ci_installs, 0),
                COALESCE(interactive_installs, installs), COALESCE(ci_crashes, 0)
         FROM (SELECT * FROM daily_summary ORDER BY day DESC LIMIT 90)
         ORDER BY day ASC",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "day": r.get::<_, String>(0)?,
                "events": r.get::<_, i64>(1)?,
                "installs": r.get::<_, i64>(2)?,
                "crashes": r.get::<_, i64>(3)?,
                "ci_events": r.get::<_, i64>(4)?,
                "ci_installs": r.get::<_, i64>(5)?,
                "interactive_installs": r.get::<_, i64>(6)?,
                "ci_crashes": r.get::<_, i64>(7)?,
            }))
        }) {
            days.extend(rows.flatten());
        }
    }

    let mut dims: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT dim, key, SUM(count) FROM daily_dim
         GROUP BY dim, key ORDER BY 3 DESC",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        }) {
            for (dim, key, n) in rows.flatten() {
                let entry = dims.entry(dim).or_default();
                if entry.len() < 40 {
                    entry.push((key, n));
                }
            }
        }
    }
    let dims: serde_json::Map<String, serde_json::Value> = dims
        .into_iter()
        .map(|(dim, rows)| {
            (
                dim,
                serde_json::Value::Array(
                    rows.into_iter()
                        .map(|(k, n)| serde_json::json!([k, n]))
                        .collect(),
                ),
            )
        })
        .collect();

    (
        [("cache-control", "public, max-age=600")],
        axum::Json(serde_json::json!({
            "generated_day": today(),
            "days": days,
            "dims": dims,
        })),
    )
}

// ---- maintenance ----

fn maintain(app: &App) {
    let db = app.db.lock().unwrap();
    rollup_all(&db);
    let _ = db.execute(
        "DELETE FROM command_events WHERE received_day < date('now', ?1)",
        [format!("-{RAW_RETENTION_DAYS} days")],
    );
    let _ = db.execute(
        "DELETE FROM crash_events WHERE received_day < date('now', ?1)",
        [format!("-{RAW_RETENTION_DAYS} days")],
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = db.execute(
        "DELETE FROM diagnose_reports WHERE received_at < ?1",
        [now.saturating_sub(60 * 60 * 24 * DIAGNOSE_RETENTION_DAYS as u64)],
    );

    // Nightly on-disk backup, keep the newest BACKUPS_KEPT.
    let dir = app.db_path.parent().unwrap_or(std::path::Path::new(".")).join("backups");
    if std::fs::create_dir_all(&dir).is_ok() {
        let target = dir.join(format!("collector-{}.db", today()));
        if let Ok(mut dst) = Connection::open(&target) {
            if let Ok(backup) = rusqlite::backup::Backup::new(&db, &mut dst) {
                let _ = backup.run_to_completion(256, Duration::from_millis(50), None);
            }
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            files.sort();
            while files.len() > BACKUPS_KEPT {
                let _ = std::fs::remove_file(files.remove(0));
            }
        }
    }
}

// ---- field validators ----

fn today() -> String {
    // Days since epoch → civil date (Hinnant), UTC.
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

fn hour_ts_ok(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 20
        && t.ends_with(":00:00Z")
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[11..13].iter().all(u8::is_ascii_digit)
}

fn uuid_ok(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn id_ok(s: &str) -> bool {
    s == "unset" || uuid_ok(s)
}

fn version_ok(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
}

fn sha_ok(s: &str) -> bool {
    s.len() == 9 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn fp_ok(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn php_version_ok(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
}

fn token_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn frame_ok(s: &str) -> bool {
    if s.len() > 300 || s.contains('/') || s.contains('\\') {
        return false;
    }
    if s == "[external]" {
        return true;
    }
    if let Some(hex) = s.strip_prefix("+0x") {
        return !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    let t = s.trim_start_matches('<');
    ["bougie", "bgx", "sandbox_run", "std::", "core::", "alloc::"]
        .iter()
        .any(|p| t.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_app() -> App {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA journal_mode=MEMORY;").unwrap();
        // Reuse the real schema.
        let tmp = tempdir_path();
        drop(db);
        let db = open_db(&tmp).unwrap();
        App { db: Mutex::new(db), limiter: Mutex::new(HashMap::new()), db_path: tmp }
    }

    fn tempdir_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bougie-collector-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!(
            "t-{}.db",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    fn valid_command() -> serde_json::Value {
        serde_json::json!({
            "schema": 1, "event": "command", "ts": "2026-07-03T09:00:00Z",
            "install_id": "unset",
            "invocation": "00000000-0000-4000-8000-000000000000",
            "bougie_version": "0.40.0", "build_sha": "0123456ab",
            "os": "linux", "arch": "x86_64", "libc": "gnu", "ci": false,
            "install_method": "installer",
            "name": "sync", "duration_ms": 1234, "outcome": "ok", "exit_code": 0,
            "autoload_ms": 12, "download_bytes": 1048576, "cache_hit_pct": 88,
            "php_version": "8.4", "extensions": ["gd", "redis"],
            "total_deps": "16-40"
        })
    }

    #[test]
    fn valid_command_event_inserts() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        assert!(insert_event(&db, "2026-07-03", &valid_command()).is_some());
        let n: i64 =
            db.query_row("SELECT count(*) FROM command_events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn unknown_field_kills_the_line() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        let mut v = valid_command();
        v["surprise"] = serde_json::json!("field");
        assert!(insert_event(&db, "2026-07-03", &v).is_none());
    }

    #[test]
    fn unknown_values_kill_the_line() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        for (key, val) in [
            ("name", serde_json::json!("rm-rf")),
            ("outcome", serde_json::json!("meltdown")),
            ("os", serde_json::json!("temple")),
            ("extensions", serde_json::json!(["my-private-ext"])),
            ("total_deps", serde_json::json!("1000000")),
            ("ts", serde_json::json!("2026-07-03T09:41:27Z")), // sub-hour!
            ("php_version", serde_json::json!("8.4.7")),
        ] {
            let mut v = valid_command();
            v[key] = val;
            assert!(insert_event(&db, "2026-07-03", &v).is_none(), "{key}");
        }
    }

    #[test]
    fn out_of_range_cache_hit_pct_drops_field_not_event() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        let mut v = valid_command();
        v["cache_hit_pct"] = serde_json::json!(150);
        assert!(insert_event(&db, "2026-07-04", &v).is_some());
        let stored: Option<i64> = db
            .query_row("SELECT cache_hit_pct FROM command_events LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);
    }

    #[test]
    fn crash_event_frames_are_pattern_checked() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        let mut v = serde_json::json!({
            "schema": 1, "event": "crash", "ts": "2026-07-03T09:00:00Z",
            "install_id": "unset",
            "invocation": "00000000-0000-4000-8000-000000000000",
            "bougie_version": "0.40.0",
            "os": "linux", "arch": "x86_64", "libc": "gnu", "ci": false,
            "install_method": "unknown",
            "command": "sync", "fingerprint": "0123456789abcdef",
            "frames": ["bougie::run", "[external]", "+0x1a2b"],
            "message": "called `unwrap()` on a `None` value"
        });
        assert!(insert_event(&db, "2026-07-03", &v).is_some());
        v["frames"] = serde_json::json!(["/home/user/leak"]);
        assert!(insert_event(&db, "2026-07-03", &v).is_none());
    }

    #[test]
    fn path_shaped_crash_message_is_dropped_but_crash_kept() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        let v = serde_json::json!({
            "schema": 1, "event": "crash", "ts": "2026-07-03T09:00:00Z",
            "install_id": "unset",
            "invocation": "00000000-0000-4000-8000-000000000000",
            "bougie_version": "0.40.0",
            "os": "linux", "arch": "x86_64", "libc": "gnu", "ci": false,
            "install_method": "unknown",
            "command": "sync", "fingerprint": "0123456789abcdef",
            "frames": ["bougie::run"],
            "message": "leaked /etc/passwd"
        });
        // Noncompliant message → whole line is rejected? No: message is
        // Option-gated; a Some that fails the check nulls out via
        // `.then()` → insert proceeds with NULL message.
        assert!(insert_event(&db, "2026-07-03", &v).is_some());
        let msg: Option<String> = db
            .query_row("SELECT message FROM crash_events LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg, None);
    }

    #[test]
    fn rollup_aggregates_and_survives_raw_pruning() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        let mut a = valid_command();
        a["install_id"] = serde_json::json!("11111111-1111-4111-8111-111111111111");
        let mut b = valid_command();
        b["name"] = serde_json::json!("cache");
        b["install_id"] = serde_json::json!("22222222-2222-4222-8222-222222222222");
        b["outcome"] = serde_json::json!("other");
        b["exit_code"] = serde_json::json!(1);
        assert!(insert_event(&db, "2026-07-03", &a).is_some());
        assert!(insert_event(&db, "2026-07-03", &b).is_some());
        // b's install fails the same way twice more — a retry burst.
        assert!(insert_event(&db, "2026-07-03", &b).is_some());
        assert!(insert_event(&db, "2026-07-03", &b).is_some());
        rollup_day(&db, "2026-07-03").unwrap();

        let (events, installs): (i64, i64) = db
            .query_row(
                "SELECT events, installs FROM daily_summary WHERE day='2026-07-03'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((events, installs), (4, 2));
        let sync_n: i64 = db
            .query_row(
                "SELECT count FROM daily_dim WHERE day='2026-07-03' AND dim='command' AND key='sync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sync_n, 1);
        // extensions CSV flattens into per-name counts (both events
        // carried gd + redis).
        let gd: i64 = db
            .query_row(
                "SELECT count FROM daily_dim WHERE dim='extension' AND key='gd'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gd, 4);
        // The failure cross records only non-ok events (a's ok row must
        // not appear), and the install-weighted twin collapses b's
        // three-event burst to its one install.
        let failure_dim = |dim: &str| -> Vec<(String, i64)> {
            let mut stmt = db
                .prepare("SELECT key, count FROM daily_dim WHERE dim = ?1")
                .unwrap();
            let rows = stmt
                .query_map([dim], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .flatten()
                .collect();
            rows
        };
        assert_eq!(failure_dim("failure"), vec![("cache → other".to_owned(), 3)]);
        assert_eq!(failure_dim("failure-installs"), vec![("cache → other".to_owned(), 1)]);

        // Rollups are frozen once raw is gone: prune raw, re-rollup,
        // rows survive (rollup_day no-ops on empty days).
        db.execute("DELETE FROM command_events", []).unwrap();
        rollup_day(&db, "2026-07-03").unwrap();
        let still: i64 = db
            .query_row("SELECT events FROM daily_summary WHERE day='2026-07-03'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(still, 4);
    }

    #[test]
    fn rollup_splits_ci_from_interactive() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        let mut dev = valid_command();
        dev["install_id"] = serde_json::json!("11111111-1111-4111-8111-111111111111");
        let mut runner = valid_command();
        runner["ci"] = serde_json::json!(true);
        runner["install_id"] = serde_json::json!("22222222-2222-4222-8222-222222222222");
        assert!(insert_event(&db, "2026-07-04", &dev).is_some());
        assert!(insert_event(&db, "2026-07-04", &runner).is_some());
        // Same CI install id twice: events count, distinct installs don't.
        assert!(insert_event(&db, "2026-07-04", &runner).is_some());
        rollup_day(&db, "2026-07-04").unwrap();

        let row: (i64, i64, i64, i64, i64) = db
            .query_row(
                "SELECT events, installs, ci_events, ci_installs, interactive_installs
                 FROM daily_summary WHERE day='2026-07-04'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(row, (3, 2, 2, 1, 1));

        let dim = |key: &str| -> i64 {
            db.query_row(
                "SELECT count FROM daily_dim WHERE day='2026-07-04' AND dim='ci' AND key=?1",
                [key],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(dim("ci"), 2);
        assert_eq!(dim("interactive"), 1);
    }

    #[test]
    fn validators() {
        assert!(hour_ts_ok("2026-07-03T09:00:00Z"));
        assert!(!hour_ts_ok("2026-07-03T09:41:27Z"));
        assert!(version_ok("0.40.0") && !version_ok("0.40") && !version_ok("a.b.c"));
        assert!(sha_ok("0123456ab") && !sha_ok("xyz"));
        assert!(frame_ok("<bougie_cli::Cli as clap::Parser>::parse"));
        assert!(!frame_ok("openssl::connect"));
    }

    // ---- diagnose ingest (schemas 1 + 2) and the admin viewer ----

    fn v2_report(md: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 2, "bougie_version": "0.45.0",
            "os": "linux", "arch": "x86_64", "libc": "gnu",
            "report_md": md
        })
    }

    #[test]
    fn diagnose_shapes_v1_and_v2_accepted_others_rejected() {
        // v1: free-form JSON with the version marker.
        assert!(diagnose_shape_ok(&serde_json::json!({
            "schema_version": 1, "failure": {"argv": ["bougie", "start"]}
        })));
        // v2: the markdown IS the report.
        assert!(diagnose_shape_ok(&v2_report("# bougie diagnostic report\n")));
        // v2 without / with empty / with oversized report_md.
        assert!(!diagnose_shape_ok(&serde_json::json!({ "schema_version": 2 })));
        assert!(!diagnose_shape_ok(&v2_report("   \n")));
        assert!(!diagnose_shape_ok(&v2_report(&"x".repeat(REPORT_MD_MAX + 1))));
        // Unknown schema.
        assert!(!diagnose_shape_ok(&serde_json::json!({ "schema_version": 3 })));
        assert!(!diagnose_shape_ok(&serde_json::json!({})));
    }

    #[test]
    fn diagnose_insert_list_detail_delete_roundtrip() {
        let app = mem_app();
        let db = app.db.lock().unwrap();
        let md = "# bougie diagnostic report\n\n## last failure\n\n\
                  command:   bougie start\ncategory:  service_start_failed (exit 74)\n";
        let id = diagnose_insert(&db, &v2_report(md)).expect("insert");
        assert!(id.starts_with("diag-"), "{id}");

        let rows = list_reports(&db, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(summarize(&rows[0].body), "bougie start");
        assert_eq!(report_meta(&rows[0].body), "bougie 0.45.0 · linux/x86_64/gnu");

        let row = load_report(&db, &id).expect("detail");
        assert_eq!(
            row.body.get("report_md").and_then(|v| v.as_str()),
            Some(md)
        );

        assert!(delete_report(&db, &id));
        assert!(!delete_report(&db, &id), "second delete is a no-op");
        assert!(load_report(&db, &id).is_none());
    }

    #[test]
    fn summarize_falls_back_to_v1_argv() {
        let v1 = serde_json::json!({
            "schema_version": 1, "bougie_version": "0.43.2",
            "os": "linux", "arch": "x86_64", "libc": "gnu",
            "failure": {"argv": ["bougie", "sync", "--offline"]}
        });
        assert_eq!(summarize(&v1), "bougie sync --offline");
        assert_eq!(report_meta(&v1), "bougie 0.43.2 · linux/x86_64/gnu");
    }

    #[test]
    fn summaries_are_derived_from_the_markdown_only() {
        // A report whose author redacted the command line in the
        // editor must show the redacted form in the list too.
        let body = v2_report("# bougie diagnostic report\n\ncommand:   «redacted»\n");
        assert_eq!(summarize(&body), "«redacted»");
    }

    #[test]
    fn utc_datetime_formats_epoch_seconds() {
        assert_eq!(utc_datetime(0), "1970-01-01 00:00:00");
        // 2026-07-06 12:00:00 UTC.
        assert_eq!(utc_datetime(1_783_339_200), "2026-07-06 12:00:00");
    }

    #[test]
    fn escape_html_covers_the_dangerous_four() {
        assert_eq!(
            escape_html(r#"<pre a="b">&x</pre>"#),
            "&lt;pre a=&quot;b&quot;&gt;&amp;x&lt;/pre&gt;"
        );
    }
}
