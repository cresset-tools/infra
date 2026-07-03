//! bougie-collector: first-party ingest for bougie's opt-in telemetry
//! and user-initiated diagnostic reports.
//!
//! The contract is bougie's TELEMETRY.md: every accepted field and
//! value is enumerated there; anything else is dropped, not stored.
//! IP addresses are used in memory for rate limiting only and are
//! never written anywhere (nginx in front runs with access_log off —
//! see hosts/telemetry/configuration.nix).
//!
//! Vocabularies are mirrored from the `bougie-telemetry` crate
//! (crates/bougie-telemetry in cresset-tools/bougie). TODO: once a
//! bougie release publishes that crate to crates.io, depend on it and
//! delete the copies below.

use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
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
const MAX_DECOMPRESSED: u64 = 1024 * 1024; // zip-bomb guard
const RATE_LIMIT_PER_HOUR: u32 = 120;
const RAW_RETENTION_DAYS: i64 = 400;
const DIAGNOSE_RETENTION_DAYS: i64 = 180;
const BACKUPS_KEPT: usize = 7;

// ---- vocabularies (mirror of bougie-telemetry; see module docs) ----

const COMMANDS: &[&str] = &[
    "init", "new", "ext", "add", "remove", "lock", "tree", "outdated", "sync", "run",
    "php", "node", "patches", "composer", "tool", "tool-exec", "cache", "self",
    "telemetry", "__telemetry-flush", "diagnose", "server", "services", "projects",
    "make", "format", "start", "stop", "unknown",
];
const OUTCOMES: &[&str] = &[
    "ok", "network", "index-signature", "manifest-hash", "blob-hash", "resolution",
    "unknown-target", "yanked", "lock-held", "filesystem", "self-update", "usage",
    "panic", "other",
];
const OSES: &[&str] = &["linux", "macos", "windows", "other"];
const ARCHES: &[&str] = &["x86_64", "aarch64", "other"];
const LIBCS: &[&str] = &["gnu", "musl", "none"];
const INSTALL_METHODS: &[&str] = &["installer", "cargo", "docker", "unknown"];
const BUCKETS: &[&str] = &["0", "1-5", "6-15", "16-40", "41-100", "100+"];
const PHP_SOURCES: &[&str] = &["managed", "system"];
const SERVICES: &[&str] =
    &["mariadb", "redis", "opensearch", "rabbitmq", "mailpit", "mkcert", "server"];
const EXTENSIONS: &[&str] = &[
    "amqp", "apcu", "ast", "bcmath", "bz2", "calendar", "curl", "dba", "dom", "ds",
    "enchant", "event", "exif", "ffi", "fileinfo", "ftp", "gd", "gettext", "gmp",
    "gnupg", "iconv", "igbinary", "imagick", "imap", "intl", "ldap", "mbstring",
    "memcached", "mongodb", "msgpack", "mysqli", "oci8", "opcache", "openswoole",
    "pcntl", "pdo_mysql", "pdo_pgsql", "pdo_sqlite", "pdo_sqlsrv", "pgsql", "phar",
    "posix", "protobuf", "pspell", "redis", "shmop", "simplexml", "snmp", "soap",
    "sockets", "sodium", "sqlite3", "sqlsrv", "ssh2", "swoole", "sysvmsg", "sysvsem",
    "sysvshm", "tidy", "uuid", "xdebug", "xhprof", "xml", "xmlreader", "xmlwriter",
    "xsl", "yaml", "zip", "zstd",
];

const ENVELOPE_KEYS: &[&str] = &[
    "schema", "event", "ts", "install_id", "invocation", "bougie_version",
    "build_sha", "os", "arch", "libc", "ci", "install_method",
];
const COMMAND_KEYS: &[&str] = &[
    "name", "duration_ms", "outcome", "exit_code", "resolve_ms", "vendor_ms",
    "packages_installed", "php_version", "php_flavor", "php_source", "extensions",
    "services", "direct_deps", "total_deps",
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
        .route("/v1/batch", post(batch))
        .route("/v1/diagnose", post(diagnose))
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
            services TEXT, direct_deps TEXT, total_deps TEXT
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
        );",
    )?;
    Ok(db)
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
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
            rusqlite::params![
                day, ts, install_id, invocation, version, build_sha, os, arch, libc,
                ci, method, name, duration, outcome, exit_code, resolve_ms, vendor_ms,
                packages_installed, php_version, php_flavor, php_source, extensions,
                services, direct_deps, total_deps
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
    // Loose shape check: diagnose payloads are user-reviewed free-form
    // reports, not allowlisted telemetry.
    if value.get("schema_version").and_then(|v| v.as_u64()) != Some(1) {
        return reject(StatusCode::BAD_REQUEST);
    }
    let db = app.db.lock().unwrap();
    let suffix: String = db
        .query_row("SELECT lower(hex(randomblob(4)))", [], |r| r.get(0))
        .unwrap_or_else(|_| "00000000".into());
    let id = format!("diag-{suffix}");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let ok = db
        .execute(
            "INSERT INTO diagnose_reports (id, received_at, body) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, now, value.to_string()],
        )
        .is_ok();
    if ok {
        (StatusCode::OK, axum::Json(serde_json::json!({ "id": id })))
    } else {
        reject(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

// ---- maintenance ----

fn maintain(app: &App) {
    let db = app.db.lock().unwrap();
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
    fn validators() {
        assert!(hour_ts_ok("2026-07-03T09:00:00Z"));
        assert!(!hour_ts_ok("2026-07-03T09:41:27Z"));
        assert!(version_ok("0.40.0") && !version_ok("0.40") && !version_ok("a.b.c"));
        assert!(sha_ok("0123456ab") && !sha_ok("xyz"));
        assert!(frame_ok("<bougie_cli::Cli as clap::Parser>::parse"));
        assert!(!frame_ok("openssl::connect"));
    }
}
