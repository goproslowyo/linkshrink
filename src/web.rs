//! Routes. `GET /<keyword>[/<rest>][?query]` is the hot path (the fallback route): a known keyword answers 307 to
//! its URL, an unknown one 307s to its create page. Everything else is the small UI around it.

use std::{borrow::Cow, sync::Arc};

use askama::Template;
use axum::{Form, Router,
           extract::{ConnectInfo, Path, Query, State},
           http::{HeaderMap, HeaderValue, Method, StatusCode, Uri,
                  header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION}},
           response::{Html, IntoResponse, Redirect, Response},
           routing::get};
use serde::Deserialize;
use tracing::{error, info};

use crate::{auth::{Auth, User, Why},
            metrics,
            store::{Change, Entry, Link, Store}};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub auth: Arc<Auth>,
    pub public_origin: Arc<str>,
}

/// Keywords the app's own pages use (and /oauth2, /logout, which Caddy's gate answers before us).
const RESERVED: &[&str] = &["edit", "links", "healthz", "metrics", "favicon.ico", "static", "oauth2", "logout", "robots.txt"];
const MAX_URL: usize = 2048;
const MAX_DESCRIPTION: usize = 500;
const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; \
                   base-uri 'none'";
const STYLE: &str = include_str!("../assets/style.css");
const FAVICON: &[u8] = include_bytes!("../assets/img/favicon.ico");

pub fn router(state: AppState) -> Router {
    Router::new().route("/", get(home))
                 .route("/links", get(|| async { Redirect::permanent("/") }))
                 .route("/edit", get(edit_query))
                 .route("/edit/{keyword}", get(edit_page).post(edit_post))
                 .route("/healthz", get(|| async { "ok" }))
                 .route("/metrics", get(metrics_page))
                 .route("/static/style.css", get(|| async { ([(CONTENT_TYPE, "text/css"), (CACHE_CONTROL, "max-age=86400")], STYLE) }))
                 .route("/favicon.ico", get(|| async { ([(CONTENT_TYPE, "image/x-icon"), (CACHE_CONTROL, "max-age=86400")], FAVICON) }))
                 .fallback(go)
                 .with_state(state)
}

/// A keyword: lower case, `[a-z0-9][a-z0-9._-]{0,63}`, not one of RESERVED. Lookups lower-case first, so
/// go/Wiki finds wiki. Borrowed (no allocation) when it already is lower case: the redirect hot path.
pub fn normalize(raw: &str) -> Option<Cow<'_, str>> {
    let b = raw.as_bytes();
    let ok = (1..=64).contains(&b.len())
             && b[0].is_ascii_alphanumeric()
             && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    if !ok {
        return None;
    }
    let k = if b.iter().any(u8::is_ascii_uppercase) { Cow::Owned(raw.to_ascii_lowercase()) } else { Cow::Borrowed(raw) };
    (!RESERVED.contains(&k.as_ref())).then_some(k)
}

/// A link target: an absolute http(s) URL with a host, at most MAX_URL bytes, no whitespace or control characters.
pub fn valid_target(raw: &str) -> Result<String, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("The URL is empty.");
    }
    if raw.len() > MAX_URL {
        return Err("The URL is too long (2048 characters at most).");
    }
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The URL can't contain spaces or control characters.");
    }
    let url = url::Url::parse(raw).map_err(|_| "That isn't a full URL (it needs https://...).")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err("Only http:// and https:// links are allowed.");
    }
    Ok(raw.to_string())
}

/// Where go/<keyword>/<rest>?<query> goes: `%s` in the URL is replaced by rest, otherwise rest is appended as more
/// path; the query is added to the URL's own. A #fragment in the URL stays at the end.
pub fn expand(url: &str, rest: &str, query: Option<&str>) -> String {
    let (base, fragment) = url.split_once('#').map_or((url, None), |(b, f)| (b, Some(f)));
    let mut out = if base.contains("%s") {
        base.replace("%s", rest)
    } else if rest.is_empty() {
        base.to_string()
    } else {
        let (path, q) = base.split_once('?').map_or((base, None), |(p, q)| (p, Some(q)));
        let mut s = String::with_capacity(base.len() + rest.len() + 1);
        s.push_str(path.trim_end_matches('/'));
        s.push('/');
        s.push_str(rest);
        if let Some(q) = q {
            s.push('?');
            s.push_str(q);
        }
        s
    };
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        out.push(if out.contains('?') { '&' } else { '?' });
        out.push_str(q);
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

fn redirect(status: StatusCode, location: &str) -> Response {
    match HeaderValue::from_str(location) {
        Ok(v) => (status, [(LOCATION, v)]).into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "bad redirect target").into_response(),
    }
}

/// The hot path.
async fn go(State(state): State<AppState>, _user: User, method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    let (keyword, rest) = path.split_once('/').unwrap_or((path, ""));
    let Some(keyword) = normalize(keyword) else {
        return page(StatusCode::NOT_FOUND, &Message { title: "Not a keyword",
                                                      body: "Keywords are letters, digits, dots, dashes and \
                                                             underscores (64 at most)." });
    };
    match state.store.get(&keyword) {
        Some(entry) => {
            entry.hit();
            metrics::REDIRECTS.inc();
            redirect(StatusCode::TEMPORARY_REDIRECT, &expand(&entry.link.url, rest, uri.query()))
        }
        None => {
            metrics::MISSES.inc();
            redirect(StatusCode::TEMPORARY_REDIRECT, &format!("/edit/{keyword}"))
        }
    }
}

fn html(status: StatusCode, body: String) -> Response {
    (status, [(CONTENT_SECURITY_POLICY, CSP), (CACHE_CONTROL, "private, no-store")], Html(body)).into_response()
}

fn page<T: Template>(status: StatusCode, t: &T) -> Response {
    match t.render() {
        Ok(body) => html(status, body),
        Err(e) => {
            error!("template: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn store_failed(e: impl std::fmt::Display) -> Response {
    error!("store: {e}");
    page(StatusCode::INTERNAL_SERVER_ERROR,
         &Message { title: "Something went wrong", body: "The link store had an error. Nothing was changed. Try again." })
}

#[derive(Template)]
#[template(path = "message.html")]
struct Message<'a> {
    title: &'a str,
    body: &'a str,
}

/// The 403 for requests without a believable gate identity.
pub struct Denied(pub Why);

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        let body = match self.0 {
            Why::NotMember => "You're signed in, but go links are for members. Ask an admin (help@wupinyin.co.uk).",
            _ => "Sign in through https://go.wupinyin.co.uk to use go links.",
        };
        page(StatusCode::FORBIDDEN, &Message { title: "Not signed in", body })
    }
}

struct Row {
    keyword: String,
    url: String,
    description: String,
    owner: String,
    hits: u64,
    private: bool,
    mine: bool,
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home<'a> {
    user: &'a User,
    rows: Vec<Row>,
    total: usize,
    filter: &'a str,
}

#[derive(Deserialize)]
struct HomeQuery {
    #[serde(default)]
    q: String,
}

async fn home(State(state): State<AppState>, user: User, Query(q): Query<HomeQuery>) -> Response {
    let filter = q.q.trim().to_ascii_lowercase();
    let all = state.store.all();
    let total = all.len();
    let rows = all.iter()
                  .filter(|e| !e.link.private || user.may_edit(&e.link.owner))
                  .filter(|e| {
                      filter.is_empty()
                      || e.link.keyword.contains(&filter)
                      || e.link.description.to_ascii_lowercase().contains(&filter)
                      || e.link.url.to_ascii_lowercase().contains(&filter)
                      || e.link.owner == filter
                  })
                  .map(|e: &Arc<Entry>| Row { keyword: e.link.keyword.clone(),
                                              url: e.link.url.clone(),
                                              description: e.link.description.clone(),
                                              owner: e.link.owner.clone(),
                                              hits: e.hits(),
                                              private: e.link.private,
                                              mine: e.link.owner == user.name })
                  .collect();
    page(StatusCode::OK, &Home { user: &user, rows, total, filter: &filter })
}

#[derive(Deserialize)]
struct EditQuery {
    #[serde(default)]
    keyword: String,
}

/// The "new link" box on the home page: GET /edit?keyword=x -> /edit/x.
async fn edit_query(_user: User, Query(q): Query<EditQuery>) -> Response {
    match normalize(q.keyword.trim()).map(Cow::into_owned) {
        Some(k) => Redirect::to(&format!("/edit/{k}")).into_response(),
        None => Redirect::to("/").into_response(),
    }
}

#[derive(Template)]
#[template(path = "edit.html")]
struct Edit<'a> {
    user: &'a User,
    keyword: &'a str,
    existing: Option<Link>,
    hits: u64,
    url: &'a str,
    description: &'a str,
    private: bool,
    owner: &'a str,
    can_edit: bool,
    error: Option<&'a str>,
    saved: bool,
    history: Vec<HistoryRow>,
    origin: &'a str,
}

struct HistoryRow {
    when: String,
    actor: String,
    action: String,
    url: String,
    owner: String,
}

impl From<Change> for HistoryRow {
    fn from(c: Change) -> Self {
        Self { when: fmt_time(c.at), actor: c.actor, action: c.action, url: c.url, owner: c.owner }
    }
}

/// UTC "YYYY-MM-DD HH:MM" without a date library (civil-from-days, Howard Hinnant).
pub fn fmt_time(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3600, secs % 3600 / 60)
}

#[derive(Deserialize)]
struct SavedQuery {
    #[serde(default)]
    saved: Option<String>,
}

async fn edit_page(State(state): State<AppState>, user: User, Path(raw): Path<String>, Query(q): Query<SavedQuery>)
                   -> Response {
    let Some(keyword) = normalize(&raw).map(Cow::into_owned) else { return Redirect::to("/").into_response() };
    if keyword != raw {
        return Redirect::to(&format!("/edit/{keyword}")).into_response();
    }
    let entry = state.store.get(&keyword);
    let history = entry.as_ref().map_or_else(Vec::new, |e| e.history.iter().cloned().map(HistoryRow::from).collect());
    let link = entry.as_ref().map(|e| e.link.clone());
    let (url, description, private, owner) = link.as_ref()
                                                 .map_or(("", "", false, user.name.as_str()), |l| {
                                                     (l.url.as_str(), l.description.as_str(), l.private, l.owner.as_str())
                                                 });
    let can_edit = link.as_ref().is_none_or(|l| user.may_edit(&l.owner));
    page(StatusCode::OK, &Edit { user: &user,
                                 keyword: &keyword,
                                 hits: entry.as_ref().map_or(0, |e| e.hits()),
                                 url,
                                 description,
                                 private,
                                 owner,
                                 can_edit,
                                 error: None,
                                 saved: q.saved.is_some(),
                                 history,
                                 origin: &state.public_origin,
                                 existing: link.clone() })
}

#[derive(Deserialize)]
struct EditForm {
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    private: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    action: String,
}

/// Form posts must come from our own pages: Origin equal to the public origin, or (browsers that send no Origin)
/// Sec-Fetch-Site: same-origin. The gate cookie is SameSite=Lax as well; this is the second lock.
fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    match headers.get("origin").and_then(|v| v.to_str().ok()) {
        Some(o) => o == origin,
        None => headers.get("sec-fetch-site").is_some_and(|v| v == "same-origin"),
    }
}

async fn edit_post(State(state): State<AppState>, user: User, Path(raw): Path<String>, headers: HeaderMap,
                   Form(form): Form<EditForm>)
                   -> Response {
    if !same_origin(&headers, &state.public_origin) {
        metrics::REJECTED_POSTS.inc();
        return page(StatusCode::FORBIDDEN, &Message { title: "Refused",
                                                      body: "That form didn't come from this site." });
    }
    let Some(keyword) = normalize(&raw).map(Cow::into_owned) else {
        return page(StatusCode::BAD_REQUEST, &Message { title: "Not a keyword",
                                                        body: "Keywords are letters, digits, dots, dashes and \
                                                               underscores (64 at most)." });
    };
    let existing = state.store.get(&keyword).map(|e| e.link.clone());
    if let Some(l) = &existing {
        if !user.may_edit(&l.owner) {
            return page(StatusCode::FORBIDDEN, &Message { title: "Not yours",
                                                          body: "Only the link's owner or an admin can change it." });
        }
    }

    if form.action == "delete" {
        return match state.store.delete(&keyword, &user.name).await {
            Ok(_) => {
                metrics::DELETES.inc();
                info!(user = %user.name, keyword, "deleted");
                Redirect::to("/").into_response()
            }
            Err(e) => store_failed(e),
        };
    }

    let description: String = form.description.trim().chars().take(MAX_DESCRIPTION).collect();
    let private = form.private.is_some();
    // Owner: the creator; only an admin can hand a link to someone else.
    let owner = match (&existing, form.owner.as_deref().map(str::trim)) {
        (_, Some(o)) if user.admin && !o.is_empty() => o.to_ascii_lowercase(),
        (Some(l), _) => l.owner.clone(),
        (None, _) => user.name.clone(),
    };
    let url = match valid_target(&form.url) {
        Ok(u) => u,
        Err(why) => {
            return page(StatusCode::BAD_REQUEST, &Edit { user: &user,
                                                         keyword: &keyword,
                                                         hits: 0,
                                                         url: form.url.trim(),
                                                         description: &description,
                                                         private,
                                                         owner: &owner,
                                                         can_edit: true,
                                                         error: Some(why),
                                                         saved: false,
                                                         history: Vec::new(),
                                                         origin: &state.public_origin,
                                                         existing });
        }
    };
    let link = Link { keyword: keyword.clone(),
                      url,
                      owner,
                      description,
                      private,
                      created_at: 0,
                      updated_at: 0,
                      updated_by: String::new() };
    match state.store.save(link, &user.name).await {
        Ok(e) => {
            metrics::SAVES.inc();
            info!(user = %user.name, keyword, url = %e.link.url, owner = %e.link.owner, "saved");
            Redirect::to(&format!("/edit/{keyword}?saved=1")).into_response()
        }
        Err(e) => store_failed(e),
    }
}

/// Prometheus, for the host's scraper only: never through Caddy (Caddy also answers /metrics with 404 itself).
async fn metrics_page(State(state): State<AppState>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>) -> Response {
    if state.auth.is_proxy(Some(peer.ip())) {
        return StatusCode::NOT_FOUND.into_response();
    }
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], metrics::render(state.store.len())).into_response()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::{body::Body,
               extract::connect_info::MockConnectInfo,
               http::{Request, header}};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    const SECRET: &str = "test-gate-secret-0123456789";
    const ORIGIN: &str = "https://go.wupinyin.co.uk";

    fn mkapp(name: &str, peer: &str) -> (Router, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("linkshrink-web-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState { store: Arc::new(Store::open(&dir.join("links.db")).unwrap()),
                               auth: Arc::new(Auth::new("10.12.0.10".parse().unwrap(),
                                                        crate::secretfile::SecretFile::fixed(SECRET.as_bytes()),
                                                        Some("welcome-users".into()),
                                                        "infra-admins".into())),
                               public_origin: ORIGIN.into() };
        let peer: SocketAddr = format!("{peer}:40000").parse().unwrap();
        (router(state).layer(MockConnectInfo(peer)), dir)
    }

    fn req(method: &str, uri: &str, user: Option<&str>, admin: bool) -> axum::http::request::Builder {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(u) = user {
            let groups = if admin { "welcome-users@idm.x,infra-admins@idm.x" } else { "welcome-users@idm.x" };
            b = b.header("x-gate-auth", SECRET)
                 .header("x-auth-request-preferred-username", format!("{u}@idm.wupinyin.co.uk"))
                 .header("x-auth-request-groups", groups);
        }
        b
    }

    async fn send(app: &Router, r: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let res = app.clone().oneshot(r).await.unwrap();
        let (parts, body) = res.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (parts.status, parts.headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get(app: &Router, uri: &str, user: Option<&str>) -> (StatusCode, HeaderMap, String) {
        send(app, req("GET", uri, user, false).body(Body::empty()).unwrap()).await
    }

    async fn post(app: &Router, uri: &str, user: &str, admin: bool, origin: Option<&str>, form: &str)
                  -> (StatusCode, HeaderMap, String) {
        let mut b = req("POST", uri, Some(user), admin).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        send(app, b.body(Body::from(form.to_string())).unwrap()).await
    }

    fn location(h: &HeaderMap) -> &str {
        h.get(LOCATION).unwrap().to_str().unwrap()
    }

    #[tokio::test]
    async fn never_anonymous() {
        let (app, dir) = mkapp("anon", "10.12.0.10");
        for path in ["/", "/wiki", "/edit/wiki", "/links/x", "/edit?keyword=x"] {
            assert_eq!(get(&app, path, None).await.0, StatusCode::FORBIDDEN, "{path}");
        }
        assert_eq!(get(&app, "/healthz", None).await.0, StatusCode::OK);
        // right headers, wrong peer: not believed
        let (other, dir2) = mkapp("anon-peer", "10.12.0.99");
        assert_eq!(get(&other, "/", Some("alice")).await.0, StatusCode::FORBIDDEN);
        // signed in but not a member
        let r = Request::get("/").header("x-gate-auth", SECRET)
                                 .header("x-auth-request-preferred-username", "eve")
                                 .header("x-auth-request-groups", "residents")
                                 .body(Body::empty())
                                 .unwrap();
        assert_eq!(send(&app, r).await.0, StatusCode::FORBIDDEN);
        let _ = (std::fs::remove_dir_all(dir), std::fs::remove_dir_all(dir2));
    }

    #[tokio::test]
    async fn create_follow_and_pass_through() {
        let (app, dir) = mkapp("follow", "10.12.0.10");
        let (s, h, _) = get(&app, "/Wiki", Some("alice")).await;
        assert_eq!((s, location(&h)), (StatusCode::TEMPORARY_REDIRECT, "/edit/wiki"));

        let (s, h, _) = post(&app, "/edit/wiki", "alice", false, Some(ORIGIN),
                             "url=https%3A%2F%2Fwiki.example%2Fbooks%2F&description=the+wiki&action=save").await;
        assert_eq!((s, location(&h)), (StatusCode::SEE_OTHER, "/edit/wiki?saved=1"));

        let (s, h, _) = get(&app, "/WIKI", Some("bob")).await;
        assert_eq!((s, location(&h)), (StatusCode::TEMPORARY_REDIRECT, "https://wiki.example/books/"));
        let (_, h, _) = get(&app, "/wiki/page/x?y=1", Some("bob")).await;
        assert_eq!(location(&h), "https://wiki.example/books/page/x?y=1");

        post(&app, "/edit/gh", "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fgithub.com%2F%25s%3Ftab%3D1&action=save").await;
        let (_, h, _) = get(&app, "/gh/wupinyin/iac?x=2", Some("bob")).await;
        assert_eq!(location(&h), "https://github.com/wupinyin/iac?tab=1&x=2");

        let (s, _, body) = get(&app, "/", Some("bob")).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("go/wiki") && body.contains("the wiki"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn owners_and_admins() {
        let (app, dir) = mkapp("owners", "10.12.0.10");
        post(&app, "/edit/mine", "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fa.example%2F&action=save").await;
        let (s, _, _) = post(&app, "/edit/mine", "bob", false, Some(ORIGIN), "url=https%3A%2F%2Fevil.example%2F&action=save").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = post(&app, "/edit/mine", "bob", false, Some(ORIGIN), "action=delete").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (_, _, page) = get(&app, "/edit/mine", Some("bob")).await;
        assert!(page.contains("Only alice or an admin"));
        // a non-admin can't take ownership through the form
        post(&app, "/edit/mine", "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fa.example%2F2&owner=bob&action=save").await;
        let (_, _, page) = get(&app, "/edit/mine", Some("alice")).await;
        assert!(page.contains("Owner alice"));
        // an admin can change and hand over
        let (s, _, _) = post(&app, "/edit/mine", "root", true, Some(ORIGIN), "url=https%3A%2F%2Fa.example%2F3&owner=bob&action=save").await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        let (s, _, _) = post(&app, "/edit/mine", "bob", false, Some(ORIGIN), "action=delete").await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(location(&get(&app, "/mine", Some("bob")).await.1), "/edit/mine");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn forms_must_come_from_our_origin() {
        let (app, dir) = mkapp("origin", "10.12.0.10");
        let form = "url=https%3A%2F%2Fa.example%2F&action=save";
        assert_eq!(post(&app, "/edit/a", "alice", false, None, form).await.0, StatusCode::FORBIDDEN);
        assert_eq!(post(&app, "/edit/a", "alice", false, Some("https://evil.example"), form).await.0, StatusCode::FORBIDDEN);
        let r = req("POST", "/edit/a", Some("alice"), false).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                                                           .header("sec-fetch-site", "same-origin")
                                                           .body(Body::from(form))
                                                           .unwrap();
        let (s, _, body) = send(&app, r).await;
        assert_eq!(s, StatusCode::SEE_OTHER, "{body}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bad_input_is_refused_and_escaped() {
        let (app, dir) = mkapp("input", "10.12.0.10");
        for bad in ["javascript%3Aalert(1)", "data%3Atext%2Fhtml%2Cx", "ftp%3A%2F%2Fx", "https%3A%2F%2F", "not+a+url", ""] {
            let (s, _, _) = post(&app, "/edit/x", "alice", false, Some(ORIGIN), &format!("url={bad}&action=save")).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        }
        for reserved in ["/edit/edit", "/edit/metrics", "/edit/oauth2"] {
            let (s, _, _) = post(&app, reserved, "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fa.example%2F&action=save").await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{reserved}");
        }
        post(&app, "/edit/x", "alice", false, Some(ORIGIN),
             "url=https%3A%2F%2Fa.example%2F%3Fa%3D%22%3E%3Cscript%3E&description=%3Cscript%3Ealert(1)%3C%2Fscript%3E&action=save").await;
        let (_, _, home) = get(&app, "/", Some("alice")).await;
        assert!(!home.contains("<script>"), "{home}");
        assert!(home.contains("&#60;script&#62;") || home.contains("&lt;script&gt;"), "{home}");
        let (_, h, _) = get(&app, "/", Some("alice")).await;
        assert!(h.get(CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap().contains("default-src 'none'"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_gate_secret_is_never_shown() {
        let (app, dir) = mkapp("secret", "10.12.0.10");
        post(&app, "/edit/a", "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fa.example%2F&action=save").await;
        for path in ["/", "/edit/a", "/edit/new", "/a", "/nope", "/edit/edit", "/%00", "/metrics"] {
            let (_, h, body) = get(&app, path, Some("alice")).await;
            assert!(!body.contains(SECRET), "{path} body");
            assert!(!format!("{h:?}").contains(SECRET), "{path} headers");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn metrics_only_off_the_proxy() {
        let (via_caddy, d1) = mkapp("metrics-caddy", "10.12.0.10");
        assert_eq!(get(&via_caddy, "/metrics", None).await.0, StatusCode::NOT_FOUND);
        let (scraper, d2) = mkapp("metrics-host", "10.12.0.1");
        let (s, _, body) = get(&scraper, "/metrics", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("linkshrink_redirects_total"));
        let _ = (std::fs::remove_dir_all(d1), std::fs::remove_dir_all(d2));
    }

    #[test]
    fn expand_cases() {
        assert_eq!(expand("https://a.example/x/", "", None), "https://a.example/x/");
        assert_eq!(expand("https://a.example/x/", "y/z", None), "https://a.example/x/y/z");
        assert_eq!(expand("https://a.example/x?k=v", "y", Some("q=1")), "https://a.example/x/y?k=v&q=1");
        assert_eq!(expand("https://a.example/s?q=%s", "rust", None), "https://a.example/s?q=rust");
        assert_eq!(expand("https://a.example/p#frag", "y", Some("q=1")), "https://a.example/p/y?q=1#frag");
    }

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(fmt_time(1_791_763_200), "2026-10-12 00:00 UTC");
    }
}
