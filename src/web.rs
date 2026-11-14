//! Dev pages telling Scrawl which OpenCode session they belong to.
//!
//! A project adds `<script src="http://127.0.0.1:47777/sdk.js" data-session="ses_…"></script>`
//! (or `data-directory`) to its dev build. The script registers the page with the ingest server
//! and heartbeats while the tab is open; a Mac capture of that tab is then sent to the session
//! (`routing`). Registrations last `TTL_MS` past the last heartbeat.
//!
//! Without the ingest token, only a page on a loopback origin, calling from this Mac, may
//! register, and only for its own origin. Any other origin must send the token as a bearer.
//!
//! With the `agentContext` experiment on, a loopback page also posts what it sees under clicks
//! and marks, plus errors, failed requests and video playback, to `/web/context` (`crate::page`).

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::privacy::{CleanUrl, UrlKey};
use crate::routing::{is_loopback_host, parse_url};

/// How long a registration lasts after its last heartbeat. The script beats every 30 s.
pub const TTL_MS: u64 = 120_000;
const MAX_ENTRIES: usize = 64;
const SDK_JS: &str = include_str!("sdk.js");

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub origin: String,
    /// The page, without its query, fragment and login.
    pub url: CleanUrl,
    /// Names the whole URL the tab registered with, and shows nothing of it: what tells this
    /// tab from another one on the same page (`?thread=A`, `#/b`).
    #[serde(default)]
    pub page: UrlKey,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// A random ID per page load, so two tabs on one origin come and go separately.
    #[serde(default)]
    pub tab: String,
    #[serde(default)]
    pub seen_ms: u64,
}

#[derive(Debug, Default)]
pub struct Registry {
    entries: Vec<Registration>,
}

impl Registry {
    pub fn register(&mut self, mut reg: Registration, now: u64) {
        reg.seen_ms = now;
        self.entries.retain(|e| {
            now.saturating_sub(e.seen_ms) <= TTL_MS && !(e.origin == reg.origin && e.tab == reg.tab)
        });
        self.entries.push(reg);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.sort_by_key(|e| std::cmp::Reverse(e.seen_ms));
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    pub fn unregister(&mut self, origin: &str, tab: &str) {
        self.entries
            .retain(|e| !(e.origin == origin && e.tab == tab));
    }

    pub fn live(&self, now: u64) -> Vec<Registration> {
        self.entries
            .iter()
            .filter(|e| now.saturating_sub(e.seen_ms) <= TTL_MS)
            .cloned()
            .collect()
    }

    /// The live registration for `url`'s origin: the tab that registered with that whole URL
    /// (query and fragment included), else the most recently heard from. The registry holds
    /// no whole URL to compare: it compares their names (`UrlKey`), made with `secret`.
    pub fn lookup(&self, url: &str, secret: &str, now: u64) -> Option<Registration> {
        let origin = parse_url(url)?.origin();
        let page = UrlKey::of(secret, url);
        let live = self
            .live(now)
            .into_iter()
            .filter(|e| e.origin == origin)
            .collect::<Vec<_>>();
        live.iter()
            .filter(|e| e.page == page)
            .max_by_key(|e| e.seen_ms)
            .or_else(|| live.iter().max_by_key(|e| e.seen_ms))
            .cloned()
    }
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    entries: Vec::new(),
});
/// Whether this process owns the ingest port, and so holds the registrations.
static SERVING: AtomicBool = AtomicBool::new(false);

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn set_serving() {
    SERVING.store(true, Ordering::Release);
}

/// The live registration for `url` (a tab's whole URL), from this process when it runs the
/// ingest server, else from the one that does. Both name URLs with the ingest token.
pub fn lookup(url: &str) -> Option<Registration> {
    let now = crate::now_epoch_ms();
    let token = crate::handoff::ensure_ingest_token().ok()?;
    if SERVING.load(Ordering::Acquire) {
        return registry().lookup(url, &token, now);
    }
    // `SCRAWL_INGEST_PORT` points at a `scrawl serve --port` other than the app's.
    let port = std::env::var("SCRAWL_INGEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(crate::app_identity::Variant::current().ingest_port());
    let mut remote = Registry::default();
    for reg in fetch_registrations(port, &token).ok()? {
        remote.entries.push(reg);
    }
    remote.lookup(url, &token, now)
}

/// The running server's live registrations (`GET /web/registrations` with the ingest token).
pub fn fetch_registrations(port: u16, token: &str) -> Result<Vec<Registration>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(400))
        .with_context(|| format!("Annotator isn't listening on port {port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET /web/registrations HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .context("Malformed response")?;
    anyhow::ensure!(
        head.starts_with("HTTP/1.1 200"),
        "Annotator answered {}",
        head.lines().next().unwrap_or("")
    );
    let value: serde_json::Value = serde_json::from_str(body)?;
    Ok(serde_json::from_value(
        value.get("registrations").cloned().unwrap_or_default(),
    )?)
}

// MARK: Who may register

/// A browser `Origin` on this machine: `http(s)://localhost`, `*.localhost`, `127.x`, `[::1]`.
pub fn is_loopback_origin(origin: &str) -> bool {
    parse_url(origin).is_some_and(|u| is_loopback_host(&u.host))
}

/// Unauthenticated registration needs a loopback page calling from this Mac (non-browser
/// clients can claim any `Origin`, so the peer address matters too); anything else needs the
/// ingest token.
pub fn may_register(origin: Option<&str>, peer_loopback: bool, token_ok: bool) -> bool {
    token_ok || (peer_loopback && origin.is_some_and(is_loopback_origin))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    origin: Option<String>,
    url: Option<String>,
    title: Option<String>,
    session: Option<String>,
    directory: Option<String>,
    tab: Option<String>,
}

fn valid_session(id: &str) -> bool {
    id.starts_with("ses")
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The registration a request describes. Without the token the page's own `Origin` header is
/// used, whatever the body says. Its URL and title go through `crate::privacy`: the registry,
/// the log and `/web/registrations` never see a query, a fragment or a login. The whole URL
/// is used once, here, to name the tab (`UrlKey`, made with `secret`), and is then dropped.
fn registration_from(
    body: Body,
    header_origin: Option<&str>,
    token_ok: bool,
    secret: &str,
) -> std::result::Result<Registration, &'static str> {
    let claimed = if token_ok {
        body.origin
            .as_deref()
            .or(header_origin)
            .or(body.url.as_deref())
    } else {
        header_origin
    };
    let origin = claimed
        .and_then(parse_url)
        .ok_or("missing or invalid origin")?
        .origin();
    let whole = body
        .url
        .filter(|u| u.len() <= 4096 && parse_url(u).is_some_and(|p| p.origin() == origin))
        .unwrap_or_else(|| format!("{origin}/"));
    let page = UrlKey::of(secret, &whole);
    let url = CleanUrl::new(&whole).unwrap_or_else(|| CleanUrl::from(format!("{origin}/")));
    let session = body
        .session
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if session.as_deref().is_some_and(|s| !valid_session(s)) {
        return Err("invalid session ID");
    }
    let directory = body
        .directory
        .map(|d| d.trim().trim_end_matches('/').to_owned())
        .filter(|d| !d.is_empty());
    if directory
        .as_deref()
        .is_some_and(|d| !d.starts_with('/') || d.len() > 1024 || d.contains('\0'))
    {
        return Err("directory must be an absolute path");
    }
    let tab: String = body
        .tab
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(32)
        .collect();
    Ok(Registration {
        origin,
        url,
        page,
        // A page's title can hold its whole URL.
        title: crate::privacy::text(&body.title.unwrap_or_default()).unwrap_or_default(),
        session,
        directory,
        tab,
        seen_ms: 0,
    })
}

// MARK: HTTP

fn header_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    origin: Option<&str>,
) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(origin) = origin {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nAccess-Control-Allow-Private-Network: true\r\nAccess-Control-Max-Age: 600\r\n"
        ));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_json(
    stream: &mut TcpStream,
    status: &str,
    body: &serde_json::Value,
    origin: Option<&str>,
) {
    respond(
        stream,
        status,
        "application/json",
        body.to_string().as_bytes(),
        origin,
    );
}

/// What the app's settings say to a dev page.
#[derive(Clone, Copy, Debug)]
pub struct PageSettings {
    /// A loopback page may send what it sees (`crate::page`): app state is on.
    pub context: bool,
    /// The modifier held to draw (Settings › Shortcuts): the page names what is under the
    /// pointer while it is held.
    pub draw: crate::settings::DrawModifier,
}

impl PageSettings {
    fn current() -> Self {
        Self {
            context: crate::handoff::app_state(),
            draw: crate::settings::draw_modifier(),
        }
    }
}

/// Answers the SDK's routes; `false` when the request is for something else (the ScrawlKit
/// routes, which need the token).
pub fn handle(stream: &mut TcpStream, header: &str, expected_token: &str) -> Result<bool> {
    handle_with(stream, header, expected_token, &PageSettings::current)
}

/// `handle` with the settings a page is told read through `settings`, when a route needs them.
fn handle_with(
    stream: &mut TcpStream,
    header: &str,
    expected_token: &str,
    settings: &dyn Fn() -> PageSettings,
) -> Result<bool> {
    let first = header.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or("");
    let is_web = path.starts_with("/web/");
    if !(path == "/sdk.js" || is_web) {
        return Ok(false);
    }
    let origin = header_value(header, "origin").filter(|o| *o != "null");
    let token_ok = header_value(header, "authorization")
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .is_some_and(|t| crate::phone::constant_time_eq(t.trim(), expected_token));
    let peer_loopback = stream.peer_addr().is_ok_and(|a| a.ip().is_loopback());

    match (method, path) {
        ("GET", "/sdk.js") => {
            respond(
                stream,
                "200 OK",
                "text/javascript; charset=utf-8",
                SDK_JS.as_bytes(),
                None,
            );
        }
        ("OPTIONS", _) => respond(stream, "204 No Content", "text/plain", b"", origin),
        ("GET", "/web/registrations") => {
            if token_ok {
                let live = registry().live(crate::now_epoch_ms());
                respond_json(
                    stream,
                    "200 OK",
                    &serde_json::json!({ "registrations": live }),
                    None,
                );
            } else {
                respond_json(
                    stream,
                    "401 Unauthorized",
                    &serde_json::json!({"ok": false}),
                    None,
                );
            }
        }
        ("POST", "/web/context") => {
            let len: usize = header_value(header, "content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            anyhow::ensure!(len <= 64 * 1024, "Page context too large");
            let mut bytes = vec![0_u8; len];
            stream.read_exact(&mut bytes)?;
            // Loopback pages only, token or not.
            let Some(page_origin) = origin
                .filter(|o| peer_loopback && is_loopback_origin(o))
                .and_then(parse_url)
                .map(|u| u.origin())
            else {
                respond_json(
                    stream,
                    "403 Forbidden",
                    &serde_json::json!({"ok": false, "error": "only loopback pages send context"}),
                    None,
                );
                return Ok(true);
            };
            let wanted = settings().context;
            if wanted && let Ok(post) = serde_json::from_slice::<crate::page::Post>(&bytes) {
                crate::page::record(&page_origin, post);
            }
            respond_json(
                stream,
                "200 OK",
                &serde_json::json!({"ok": true, "context": wanted}),
                origin,
            );
        }
        ("POST", "/web/register" | "/web/unregister") => {
            let len: usize = header_value(header, "content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            anyhow::ensure!(len <= 16 * 1024, "Registration too large");
            // Read the body even when refusing, so the answer isn't lost to a reset.
            let mut bytes = vec![0_u8; len];
            stream.read_exact(&mut bytes)?;
            if !may_register(origin, peer_loopback, token_ok) {
                respond_json(
                    stream,
                    "403 Forbidden",
                    &serde_json::json!({"ok": false, "error": "only loopback pages may register without the ingest token"}),
                    None,
                );
                return Ok(true);
            }
            let body: Body = serde_json::from_slice(&bytes).unwrap_or_default();
            match registration_from(body, origin, token_ok, expected_token) {
                Ok(reg) if path == "/web/unregister" => {
                    registry().unregister(&reg.origin, &reg.tab);
                    respond_json(stream, "200 OK", &serde_json::json!({"ok": true}), origin);
                }
                Ok(reg) => {
                    crate::capture::scrawl_log(format!(
                        "web: {} registered (session {}, directory {})",
                        reg.url,
                        reg.session.as_deref().unwrap_or("-"),
                        reg.directory.as_deref().unwrap_or("-")
                    ));
                    // `context`: the page may send what it sees (`crate::page`); loopback only.
                    // `draw`: the modifier whose moves the page names what is under.
                    let settings = settings();
                    let context = origin.is_some_and(is_loopback_origin) && settings.context;
                    registry().register(reg, crate::now_epoch_ms());
                    respond_json(
                        stream,
                        "200 OK",
                        &serde_json::json!({
                            "ok": true, "ttlMs": TTL_MS, "context": context,
                            "draw": settings.draw.id(),
                        }),
                        origin,
                    );
                }
                Err(error) => respond_json(
                    stream,
                    "400 Bad Request",
                    &serde_json::json!({"ok": false, "error": error}),
                    origin,
                ),
            }
        }
        _ => respond_json(
            stream,
            "404 Not Found",
            &serde_json::json!({"ok": false}),
            origin,
        ),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DrawModifier;

    #[test]
    fn loopback_origins_register_without_the_token_and_others_need_it() {
        for origin in [
            "http://localhost:5173",
            "http://127.0.0.1:3000",
            "https://app.localhost",
            "http://[::1]:8080",
        ] {
            assert!(may_register(Some(origin), true, false), "{origin}");
            // A non-browser client elsewhere can send any Origin.
            assert!(!may_register(Some(origin), false, false), "{origin}");
        }
        for origin in [
            "https://example.com",
            "http://studio.example.ts.net:5173",
            "http://localhost.evil.com",
            "http://192.168.1.4:5173",
        ] {
            assert!(!may_register(Some(origin), true, false), "{origin}");
            assert!(may_register(Some(origin), false, true), "{origin}");
        }
        assert!(!may_register(None, true, false));
    }

    #[test]
    fn unauthenticated_pages_register_only_their_own_origin() {
        let body = Body {
            origin: Some("https://victim.example".into()),
            url: Some("https://victim.example/x".into()),
            session: Some("ses_abc".into()),
            tab: Some("t1!".into()),
            ..Body::default()
        };
        let reg = registration_from(body, Some("http://localhost:5173"), false, "tok").unwrap();
        assert_eq!(reg.origin, "http://localhost:5173");
        assert_eq!(reg.url, "http://localhost:5173/");
        assert_eq!(reg.tab, "t1");
        assert_eq!(reg.session.as_deref(), Some("ses_abc"));

        let bad = Body {
            session: Some("ses_a; rm -rf".into()),
            ..Body::default()
        };
        assert!(registration_from(bad, Some("http://localhost:1"), false, "tok").is_err());
        let relative = Body {
            directory: Some("site".into()),
            ..Body::default()
        };
        assert!(registration_from(relative, Some("http://localhost:1"), false, "tok").is_err());
        assert!(registration_from(Body::default(), None, false, "tok").is_err());

        let with_token = Body {
            url: Some("https://studio.ts.net:5173/a".into()),
            directory: Some("/p/app/".into()),
            ..Body::default()
        };
        let reg = registration_from(with_token, None, true, "tok").unwrap();
        assert_eq!(reg.origin, "https://studio.ts.net:5173");
        assert_eq!(reg.directory.as_deref(), Some("/p/app"));
    }

    #[test]
    fn privacy_registrations_keep_no_url_query_fragment_or_login() {
        let origin = "http://localhost:5173";
        let body = |url: &str, tab: &str| Body {
            url: Some(url.into()),
            title: Some(format!("Fixture {url}")),
            session: Some(format!("ses_{tab}")),
            tab: Some(tab.into()),
            ..Body::default()
        };
        for url in [
            "http://localhost:5173/reset?token=SECRET-MARKER-URL#SECRET-MARKER-FRAG",
            "http://localhost:5173/reset#SECRET-MARKER-FRAG",
            "http://localhost:5173/reset?SECRET-MARKER-URL",
            "http://kit:SECRET-MARKER@localhost:5173/reset",
        ] {
            let reg = registration_from(body(url, "t1"), Some(origin), false, "tok").unwrap();
            assert_eq!(reg.url.to_string(), "http://localhost:5173/reset", "{url}");
            assert_eq!(reg.title, "Fixture http://localhost:5173/reset", "{url}");
            // What the log line prints, what the registry keeps and what `/web/registrations`
            // answers.
            let stored = format!("{reg:?} {}", serde_json::to_string(&reg).unwrap());
            assert!(!stored.contains("SECRET-MARKER"), "{stored}");
        }
        // A registration from another process is cleaned when it is read.
        let remote: Registration = serde_json::from_value(serde_json::json!({
            "origin": origin, "url": "http://localhost:5173/a?token=SECRET-MARKER-URL"
        }))
        .unwrap();
        assert_eq!(remote.url.to_string(), "http://localhost:5173/a");

        // The tab's own registration is still found by the tab's whole URL.
        let mut registry = Registry::default();
        for (path, tab, at) in [("a", "1", 1_000), ("b", "2", 2_000)] {
            let url = format!("{origin}/{path}?token=SECRET-MARKER-URL#SECRET-MARKER-FRAG");
            let reg = registration_from(body(&url, tab), Some(origin), false, "tok").unwrap();
            registry.register(reg, at);
        }
        let found = registry
            .lookup(
                "http://localhost:5173/a?token=SECRET-MARKER-URL#SECRET-MARKER-FRAG",
                "tok",
                3_000,
            )
            .unwrap();
        assert_eq!(found.session.as_deref(), Some("ses_1"));
        assert!(!format!("{registry:?}").contains("SECRET-MARKER"));
    }

    #[test]
    fn a_tab_is_found_by_its_whole_url_and_only_its_page_is_kept() {
        let origin = "http://localhost:5173";
        let mut registry = Registry::default();
        let mut at = 1_000;
        let mut register = |registry: &mut Registry, url: &str, session: &str| {
            let body = Body {
                url: Some(url.into()),
                session: Some(session.into()),
                tab: Some(session.into()),
                ..Body::default()
            };
            at += 1_000;
            registry.register(
                registration_from(body, Some(origin), false, "tok").unwrap(),
                at,
            );
        };
        // Two tabs that differ in the query, two in the fragment, and two in a path that is
        // longer than a stored URL: each is registered after the one before it.
        let long = |end: &str| format!("{origin}/{}/{end}", "p".repeat(220));
        let tabs = [
            (format!("{origin}/?thread=SECRET-MARKER-A"), "ses_qa"),
            (format!("{origin}/?thread=SECRET-MARKER-B"), "ses_qb"),
            (format!("{origin}/#/thread/SECRET-MARKER-A"), "ses_fa"),
            (format!("{origin}/#/thread/SECRET-MARKER-B"), "ses_fb"),
            (long("a"), "ses_la"),
            (long("b"), "ses_lb"),
        ];
        for (url, session) in &tabs {
            register(&mut registry, url, session);
        }
        for (url, session) in &tabs {
            let found = registry
                .lookup(url, "tok", 60_000)
                .and_then(|reg| reg.session);
            assert_eq!(found.as_deref(), Some(*session), "{url}");
        }
        // A page of the origin that no tab registered goes to the newest tab, as before.
        let other = registry.lookup("http://localhost:5173/?thread=C", "tok", 60_000);
        assert_eq!(other.and_then(|reg| reg.session).as_deref(), Some("ses_lb"));
        // The names are made with the server's secret: without it they match nothing.
        let (url, _) = &tabs[0];
        let guessed = registry.lookup(url, "another", 60_000);
        assert_eq!(
            guessed.and_then(|reg| reg.session).as_deref(),
            Some("ses_lb")
        );
        // What the registry holds and what `/web/registrations` answers: the page only.
        let kept = format!(
            "{registry:?} {}",
            serde_json::to_string(&registry.live(60_000)).unwrap()
        );
        assert!(
            !kept.contains("SECRET-MARKER") && !kept.contains("thread"),
            "{kept}"
        );
    }

    fn reg(origin: &str, url: &str, tab: &str, session: &str) -> Registration {
        Registration {
            origin: origin.into(),
            url: url.into(),
            page: UrlKey::of("tok", url),
            tab: tab.into(),
            session: Some(session.into()),
            ..Registration::default()
        }
    }

    #[test]
    fn registrations_expire_and_prefer_the_exact_tab() {
        let mut registry = Registry::default();
        let o = "http://localhost:5173";
        registry.register(reg(o, "http://localhost:5173/a", "1", "ses_a"), 1_000);
        registry.register(reg(o, "http://localhost:5173/b", "2", "ses_b"), 2_000);
        let session =
            |r: &Registry, url: &str, now| r.lookup(url, "tok", now).and_then(|r| r.session);
        assert_eq!(
            session(&registry, "http://localhost:5173/a", 3_000).as_deref(),
            Some("ses_a")
        );
        // Another page on the origin goes to the newest tab.
        assert_eq!(
            session(&registry, "http://localhost:5173/c", 3_000).as_deref(),
            Some("ses_b")
        );
        assert_eq!(session(&registry, "http://localhost:3000/", 3_000), None);
        // Tab 2 stops heartbeating; tab 1 heartbeats on.
        registry.register(reg(o, "http://localhost:5173/a", "1", "ses_a"), 100_000);
        assert_eq!(
            session(&registry, "http://localhost:5173/c", 2_000 + TTL_MS + 1).as_deref(),
            Some("ses_a")
        );
        assert_eq!(
            session(&registry, "http://localhost:5173/a", 100_000 + TTL_MS + 1),
            None
        );
        // A heartbeat replaces the tab's entry instead of adding one.
        registry.register(reg(o, "http://localhost:5173/a", "1", "ses_a"), 100_500);
        assert_eq!(registry.live(100_500).len(), 2);
        registry.unregister(o, "1");
        assert_eq!(registry.live(100_500).len(), 1);
    }

    fn request(raw: &str, token: &str) -> String {
        request_with(raw, token, &PageSettings::current)
    }

    fn request_with(raw: &str, token: &str, settings: &dyn Fn() -> PageSettings) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let raw = raw.to_owned();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut out = String::new();
            stream.read_to_string(&mut out).unwrap();
            out
        });
        let (mut stream, _) = listener.accept().unwrap();
        let mut header = Vec::new();
        let mut byte = [0_u8; 1];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let header = String::from_utf8(header).unwrap();
        assert!(handle_with(&mut stream, &header, token, settings).unwrap());
        drop(stream);
        client.join().unwrap()
    }

    #[test]
    fn a_registration_tells_the_page_the_draw_modifier() {
        let body = r#"{"url":"http://localhost:6124/","session":"ses_draw","tab":"dd"}"#;
        for draw in DrawModifier::ALL {
            let reply = request_with(
                &format!(
                    "POST /web/register HTTP/1.1\r\nOrigin: http://localhost:6124\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
                "tok",
                &|| PageSettings {
                    context: false,
                    draw,
                },
            );
            assert!(
                reply.contains(&format!(r#""draw":"{}""#, draw.id())),
                "{reply}"
            );
        }
    }

    /// `cargo test sdk_page -- --ignored`: with ⌥ as the draw modifier the page names what is
    /// under ⌥ moves, and ⇧ moves are ordinary moves.
    #[test]
    #[ignore = "runs sdk.js in headless Google Chrome"]
    fn sdk_page_marks_with_the_configured_draw_modifier() {
        for (draw, held, other) in [
            (DrawModifier::Option, "altKey", "shiftKey"),
            (DrawModifier::Shift, "shiftKey", "altKey"),
            (DrawModifier::Command, "metaKey", "shiftKey"),
        ] {
            let driver = r#"
    fire(document.body, "pointermove", Object.assign({ OTHER: true }, over($("nick-label"))));
    fire(document.body, "pointermove", Object.assign({ HELD: true, OTHER: true }, over($("reset"))));
    fire(document.body, "pointermove", Object.assign({ HELD: true }, over($("save"))));
    fire($("name"), "pointerdown");
"#
            .replace("HELD", held)
            .replace("OTHER", other);
            let run = run_fixture(
                PageSettings {
                    context: true,
                    draw,
                },
                &driver,
            );
            assert_eq!(run.seen("mark"), ["<button#save> “Save”"], "{draw:?}");
            assert_eq!(run.seen("pointer"), ["<input#name> “Your name”"]);
        }
    }

    /// Google Chrome, which runs the real `sdk.js` in a real page.
    fn chrome() -> Option<std::path::PathBuf> {
        [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.exists())
    }

    /// What one run of the privacy fixture gave.
    struct PageRun {
        origin: String,
        /// Every body the page posted to `/web/*`, with its path.
        posts: Vec<(String, String)>,
        /// What `crate::page` kept of it.
        kept: Vec<crate::page::Event>,
    }

    impl PageRun {
        fn posted(&self, path: &str) -> Vec<&str> {
            self.posts
                .iter()
                .filter(|(to, _)| to == path)
                .map(|(_, body)| body.as_str())
                .collect()
        }

        /// `describe` of what the page saw under each event of `kind`, in order.
        fn seen(&self, kind: &str) -> Vec<String> {
            self.kept
                .iter()
                .filter(|event| event.kind == kind)
                .filter_map(|event| event.target.as_ref())
                .map(crate::page::describe)
                .collect()
        }
    }

    /// Opens `Tests/fixtures/appshot-secret.html` in headless Chrome, with a marker in its
    /// query and fragment, from a loopback server that answers the SDK's routes through
    /// `handle_with`. Once the SDK watches the page, `driver` runs (JavaScript; `$` finds an
    /// element by id, `fire` sends it a pointer event, `over` gives its middle as `clientX` and
    /// `clientY`). Returns when the page has posted what it saw.
    fn run_fixture(settings: PageSettings, driver: &str) -> PageRun {
        // One page at a time: `crate::page` keeps every page's events in one log, and a run
        // reads back its own by their time.
        static ONE_PAGE: Mutex<()> = Mutex::new(());
        let _one = ONE_PAGE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let chrome = chrome().expect("Google Chrome in /Applications");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("Tests/fixtures/appshot-secret.html");
        let bootstrap = r#"<script>
(() => {
  const real = window.fetch.bind(window);
  const mine = (url, init) => {
    const pending = real(url, init);
    if (String(url).endsWith("/web/context")) pending.then(() => real("/done", { method: "POST" }));
    return pending;
  };
  window.fetch = mine;
  // The SDK watches the page once it has put its own `fetch` in place.
  const wait = setInterval(() => {
    if (window.fetch === mine) return;
    clearInterval(wait);
    const $ = (id) => document.getElementById(id);
    const fire = (el, type, init) =>
      el.dispatchEvent(new PointerEvent(type, Object.assign({ bubbles: true, composed: true }, init)));
    const over = (el) => {
      const box = el.getBoundingClientRect();
      return { clientX: box.left + box.width / 2, clientY: box.top + box.height / 2 };
    };
    DRIVER
  }, 20);
})();
</script>
<script src="/sdk.js" data-session="ses_fixture"></script>
</body>"#;
        let html = std::fs::read_to_string(&fixture)
            .unwrap()
            .replace("</body>", &bootstrap.replace("DRIVER", driver));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let profile = std::env::temp_dir().join(format!(
            "scrawl-sdk-chrome-{}-{}",
            std::process::id(),
            crate::now_epoch_ms()
        ));
        let mut browser = std::process::Command::new(chrome)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--window-size=900,1100",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!(
                "{origin}/fixture.html?token=SECRET-MARKER-URL#SECRET-MARKER-FRAGMENT"
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let from = crate::now_epoch_ms();
        let began = std::time::Instant::now();
        let mut posts = Vec::new();
        let mut done = false;
        while !done && began.elapsed() < Duration::from_secs(40) {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0_u8; 1];
            while !header.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
                header.push(byte[0]);
            }
            let header = String::from_utf8_lossy(&header).into_owned();
            let mut first = header.lines().next().unwrap_or("").split_whitespace();
            let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
            let path = path.split('?').next().unwrap_or("").to_owned();
            match (method, path.as_str()) {
                ("GET", "/fixture.html") => {
                    respond(
                        &mut stream,
                        "200 OK",
                        "text/html; charset=utf-8",
                        html.as_bytes(),
                        None,
                    );
                }
                ("POST", "/done") => {
                    respond(&mut stream, "200 OK", "text/plain", b"", None);
                    done = true;
                }
                _ if path == "/sdk.js" || path.starts_with("/web/") => {
                    // The body as the page sent it, read without taking it from the handler.
                    let len: usize = header_value(&header, "content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0_u8; len];
                    let waited = std::time::Instant::now();
                    while stream.peek(&mut body).unwrap_or(0) < len
                        && waited.elapsed() < Duration::from_secs(5)
                    {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    posts.push((path, String::from_utf8_lossy(&body).into_owned()));
                    handle_with(&mut stream, &header, "tok", &|| settings).unwrap();
                }
                _ => respond(&mut stream, "404 Not Found", "text/plain", b"", None),
            }
        }
        let _ = browser.kill();
        let _ = browser.wait();
        let _ = std::fs::remove_dir_all(&profile);
        assert!(done, "the page never posted what it saw: {posts:?}");
        PageRun {
            kept: crate::page::between(from, crate::now_epoch_ms()),
            origin,
            posts,
        }
    }

    /// `cargo test sdk_page -- --ignored`: the real `sdk.js` over the privacy fixture.
    #[test]
    #[ignore = "runs sdk.js in headless Google Chrome"]
    fn sdk_page_sends_and_keeps_no_typed_text() {
        let run = run_fixture(
            PageSettings {
                context: true,
                draw: DrawModifier::Shift,
            },
            r#"
    $("name").value = "SECRET-MARKER-INPUT";
    $("password").value = "SECRET-MARKER-PASSWORD";
    $("notes").value = "SECRET-MARKER-NOTES";
    $("nick").innerHTML = "<b id='bold'>SECRET-MARKER-NICK</b>";
    for (const id of ["bold", "nick", "nick-label", "name", "password", "notes", "save"]) {
      fire($(id), "pointerdown");
    }
"#,
        );
        // What was typed never leaves the page.
        let sent = run.posted("/web/context").join("\n");
        assert!(!sent.contains("SECRET-MARKER"), "{sent}");
        // Each click names its element; a field gives the name its developer wrote.
        assert_eq!(
            run.seen("pointer"),
            [
                "<span#nick>",
                "<span#nick>",
                "<label#nick-label> “Nickname”",
                "<input#name> “Your name”",
                "<input#password>",
                "<textarea#notes>",
                "<button#save> “Save”",
            ]
        );
        assert!(!format!("{:?}", run.kept).contains("SECRET-MARKER"));
        // The registration: the page sends its whole address, the app names the tab with it
        // and keeps the page only. Nothing kept of the address or the title holds a marker.
        let live: Vec<Registration> = registry()
            .live(crate::now_epoch_ms())
            .into_iter()
            .filter(|reg| reg.origin == run.origin)
            .collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].url, format!("{}/fixture.html", run.origin).as_str());
        let stored = format!("{live:?} {}", serde_json::to_string(&live).unwrap());
        assert!(!stored.contains("SECRET-MARKER"), "{stored}");
        // The tab is found by the whole address its browser shows, and by no other query.
        let whole = format!(
            "{}/fixture.html?token=SECRET-MARKER-URL#SECRET-MARKER-FRAGMENT",
            run.origin
        );
        assert_eq!(live[0].page, UrlKey::of("tok", &whole));
        assert_ne!(
            live[0].page,
            UrlKey::of("tok", &format!("{}/fixture.html?token=other", run.origin))
        );
    }

    /// `cargo test sdk_page -- --ignored`: an element is a field by any role in its `role`
    /// list, in any case.
    #[test]
    #[ignore = "runs sdk.js in headless Google Chrome"]
    fn sdk_page_treats_any_text_entry_role_as_a_field() {
        let run = run_fixture(
            PageSettings {
                context: true,
                draw: DrawModifier::Shift,
            },
            r#"
    $("role-box").innerHTML = "<i id='inner'>SECRET-MARKER-ROLE</i>";
    $("spin-box").textContent = "SECRET-MARKER-SPIN";
    for (const id of ["inner", "role-box", "role-label", "spin-box", "spin-label"]) {
      fire($(id), "pointerdown");
    }
    fire(document.body, "pointermove", Object.assign({ shiftKey: true }, over($("role-box"))));
"#,
        );
        let sent = run.posted("/web/context").join("\n");
        assert!(!sent.contains("SECRET-MARKER"), "{sent}");
        assert_eq!(
            run.seen("pointer"),
            [
                "<span#role-box.box>",
                "<span#role-box.box>",
                "<label#role-label> “Handle”",
                "<span#spin-box.box>",
                "<label#spin-label> “Count”",
            ]
        );
        assert_eq!(run.seen("mark"), ["<span#role-box.box>"]);
        assert!(!format!("{:?}", run.kept).contains("SECRET-MARKER"));
    }

    #[test]
    fn http_routes_serve_the_script_and_check_registrations() {
        let js = request("GET /sdk.js HTTP/1.1\r\nHost: x\r\n\r\n", "tok");
        assert!(
            js.starts_with("HTTP/1.1 200") && js.contains("/web/register"),
            "{js}"
        );

        let body = r#"{"url":"http://localhost:6123/","session":"ses_webtest","tab":"zz"}"#;
        let ok = request(
            &format!(
                "POST /web/register HTTP/1.1\r\nOrigin: http://localhost:6123\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            "tok",
        );
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("Access-Control-Allow-Origin: http://localhost:6123"));
        assert_eq!(
            registry()
                .lookup("http://localhost:6123/", "tok", crate::now_epoch_ms())
                .and_then(|r| r.session)
                .as_deref(),
            Some("ses_webtest")
        );

        let remote = request(
            &format!(
                "POST /web/register HTTP/1.1\r\nOrigin: https://example.com\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            "tok",
        );
        assert!(remote.starts_with("HTTP/1.1 403"), "{remote}");
        let authed = request(
            &format!(
                "POST /web/register HTTP/1.1\r\nOrigin: https://example.com\r\nAuthorization: Bearer tok\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            "tok",
        );
        assert!(authed.starts_with("HTTP/1.1 200"), "{authed}");

        let list = request("GET /web/registrations HTTP/1.1\r\n\r\n", "tok");
        assert!(list.starts_with("HTTP/1.1 401"), "{list}");
        let list = request(
            "GET /web/registrations HTTP/1.1\r\nAuthorization: Bearer tok\r\n\r\n",
            "tok",
        );
        assert!(list.contains("ses_webtest"), "{list}");

        let unregister = request(
            &format!(
                "POST /web/unregister HTTP/1.1\r\nOrigin: http://localhost:6123\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            "tok",
        );
        assert!(unregister.starts_with("HTTP/1.1 200"), "{unregister}");

        // Page context is loopback-only even with the token, and ignored with the experiment off.
        let context = r#"{"tab":"zz","events":[]}"#;
        let refused = request(
            &format!(
                "POST /web/context HTTP/1.1\r\nOrigin: https://example.com\r\nAuthorization: Bearer tok\r\nContent-Length: {}\r\n\r\n{context}",
                context.len()
            ),
            "tok",
        );
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        let accepted = request(
            &format!(
                "POST /web/context HTTP/1.1\r\nOrigin: http://localhost:6123\r\nContent-Length: {}\r\n\r\n{context}",
                context.len()
            ),
            "tok",
        );
        assert!(
            accepted.starts_with("HTTP/1.1 200") && accepted.contains(r#""context":false"#),
            "{accepted}"
        );
        assert!(
            registry()
                .lookup("http://localhost:6123/", "tok", crate::now_epoch_ms())
                .is_none()
        );
    }
}
