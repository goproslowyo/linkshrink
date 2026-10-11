//! Routes. `GET /<keyword>[/<rest>][?query]` is the hot path (the fallback route): a known name (or alias) answers
//! 307 to its URL, an unknown one 307s to its create page. Everything else is the small UI around it:
//!
//!   GET  /                 the box: type a name ⏎, paste the URL ⏎, done (or paste a URL first: a short name is made)
//!   GET  /edit/<name>      the box for that name, plus its details, aliases and history
//!   POST /new              save from the box (name optional); JSON when asked (Accept: application/json), else 303
//!   POST /edit/<name>      action=save|delete|alias_add|alias_remove (details form, delete, aliases); JSON or 303
//!   GET  /api/url/<name>   what the box needs to know about a name (JSON)
//!   GET  /links            every link: filter, sort by hits/name/recent/stale, mine only
//!
//! Pages are server-rendered and work without JavaScript; /static/app.js (no framework, a few KB) adds the keyboard flow.

use std::{borrow::Cow, sync::Arc};

use askama::Template;
use axum::{Form, Json, Router,
           extract::{ConnectInfo, Path, Query, State},
           http::{HeaderMap, HeaderValue, Method, StatusCode, Uri,
                  header::{ACCEPT, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION}},
           response::{Html, IntoResponse, Redirect, Response},
           routing::{MethodRouter, get, post}};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::{auth::{Auth, User, Why},
            metrics,
            store::{Change, Entry, Link, Store, StoreError, now}};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub auth: Arc<Auth>,
    pub public_origin: Arc<str>,
    /// who to ask for access (the "not a member" page)
    pub help: Arc<str>,
    /// Cache-Control for a known name's redirect: "private, max-age=N", or no-store when N is 0
    redirect_cache: HeaderValue,
}

impl AppState {
    pub fn new(store: Arc<Store>, auth: Arc<Auth>, public_origin: &str, help: &str, redirect_cache_secs: u64) -> Self {
        let redirect_cache = if redirect_cache_secs == 0 {
            HeaderValue::from_static("private, no-store")
        } else {
            HeaderValue::from_str(&format!("private, max-age={redirect_cache_secs}")).expect("ascii")
        };
        Self { store, auth, public_origin: public_origin.into(), help: help.into(), redirect_cache }
    }
}

/// Names the app's own pages use (and /oauth2, /logout, which Caddy's gate answers before us).
const RESERVED: &[&str] = &["edit", "links", "new", "api", "healthz", "metrics", "favicon.ico", "static", "oauth2", "logout",
                            "robots.txt"];
const MAX_URL: usize = 2048;
const MAX_DESCRIPTION: usize = 500;
const CSP: &str = "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'self'; img-src 'self'; \
                   form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
const STYLE: &str = include_str!("../assets/style.css");
const SCRIPT: &str = include_str!("../assets/app.js");
const FAVICON: &[u8] = include_bytes!("../assets/img/favicon.ico");
/// Cache-buster for the static files (they're cached for a day).
pub const ASSET_VERSION: &str = env!("CARGO_PKG_VERSION");

fn asset(ctype: &'static str, body: &'static [u8]) -> MethodRouter<AppState> {
    get(move || async move { ([(CONTENT_TYPE, ctype), (CACHE_CONTROL, "private, max-age=86400")], body) })
}

pub fn router(state: AppState) -> Router {
    Router::new().route("/", get(home))
                 .route("/links", get(links_page))
                 .route("/new", post(new_post))
                 .route("/edit", get(edit_query))
                 .route("/edit/{keyword}", get(edit_page).post(edit_post))
                 .route("/api/url/{keyword}", get(api_url))
                 .route("/healthz", get(|| async { "ok" }))
                 .route("/metrics", get(metrics_page))
                 .route("/static/style.css", asset("text/css; charset=utf-8", STYLE.as_bytes()))
                 .route("/static/app.js", asset("text/javascript; charset=utf-8", SCRIPT.as_bytes()))
                 .route("/favicon.ico", asset("image/x-icon", FAVICON))
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

const BAD_NAME: &str = "Names are letters, digits, dots, dashes and underscores (64 at most), and not one of the \
                        app's own words (edit, links, new, api, ...).";

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

/// Characters a decoded rest-of-path must not carry raw into a Location header (`/` stays a separator).
const REST_ESCAPE: &percent_encoding::AsciiSet =
    &percent_encoding::CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`').add(b'#').add(b'?').add(b'{').add(b'}');

/// What someone typed into a search shortcut, decoded: (name, rest of the path re-escaped, query).
fn typed(raw: &str) -> (String, String, Option<String>) {
    let text = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
    let text = text.trim();
    let (path, query) = text.split_once('?').map_or((text, None), |(p, q)| (p, Some(q.to_string())));
    let (name, rest) = path.split_once(['/', ' ']).unwrap_or((path, ""));
    let rest = rest.trim();
    (name.to_string(), percent_encoding::utf8_percent_encode(rest, REST_ESCAPE).to_string(), query)
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
    let (first, _) = path.split_once('/').unwrap_or((path, ""));
    // A browser search shortcut (`go` -> https://go.wupinyin.co.uk/%s) encodes what was typed, so `go git/infra/x?y=1`
    // arrives as /git%2Finfra%2Fx%3Fy%3D1. When the name itself is encoded, decode the whole thing once and split it
    // again (the first space also ends the name: `go w kanidm reauth` searches "kanidm reauth"). Plain paths never take this branch.
    let decoded;
    let (keyword, rest, query) = if first.contains('%') {
        decoded = typed(path);
        (decoded.0.as_str(), decoded.1.as_str(), decoded.2.as_deref().or(uri.query()))
    } else {
        let (k, r) = path.split_once('/').unwrap_or((path, ""));
        (k, r, uri.query())
    };
    let Some(keyword) = normalize(keyword) else {
        return page(StatusCode::NOT_FOUND, &Message { title: "Not a name", body: BAD_NAME });
    };
    match state.store.get(&keyword) {
        Some(entry) => {
            entry.hit();
            metrics::REDIRECTS.inc();
            let mut r = redirect(StatusCode::TEMPORARY_REDIRECT, &expand(&entry.link.url, rest, query));
            r.headers_mut().insert(CACHE_CONTROL, state.redirect_cache.clone());
            r
        }
        None => {
            metrics::MISSES.inc();
            // never cached: once someone creates the name, the next visit must see it
            let mut r = redirect(StatusCode::TEMPORARY_REDIRECT, &format!("/edit/{keyword}"));
            r.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            r
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

#[derive(Template)]
#[template(path = "message.html")]
struct Message<'a> {
    title: &'a str,
    body: &'a str,
}

/// The 403 for requests without a believable gate identity: why, who to ask (LINKSHRINK_HELP), the public origin.
pub struct Denied(pub Why, pub Arc<str>, pub Arc<str>);

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        let (title, body) = match self.0 {
            Why::NotMember => ("Not a member", format!("You're signed in, but go links are for members. {}", self.1)),
            _ => ("Not signed in", format!("Sign in through {} to use go links.", self.2)),
        };
        page(StatusCode::FORBIDDEN, &Message { title, body: &body })
    }
}

/// "3 d ago" from unix seconds (0 = never).
pub fn ago(t: i64, now: i64) -> String {
    if t <= 0 {
        return "never".into();
    }
    let s = (now - t).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        86_400..5_184_000 => format!("{} d ago", s / 86_400),
        5_184_000..63_072_000 => format!("{} mo ago", s / 2_592_000),
        _ => format!("{} y ago", s / 31_536_000),
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

// ---------------------------------------------------------------------------------------------------------- the box

struct HistoryRow {
    when: String,
    actor: String,
    action: String,
    url: String,
    owner: String,
}

impl From<&Change> for HistoryRow {
    fn from(c: &Change) -> Self {
        Self { when: fmt_time(c.at), actor: c.actor.clone(), action: c.action.clone(), url: c.url.clone(), owner: c.owner.clone() }
    }
}

/// What the box page shows about an existing link.
struct Details {
    owner: String,
    description: String,
    private: bool,
    generated: bool,
    hits: u64,
    last_used: String,
    created: String,
    aliases: Vec<String>,
    history: Vec<HistoryRow>,
}

impl Details {
    fn of(e: &Entry) -> Self {
        Self { owner: e.link.owner.clone(),
               description: e.link.description.clone(),
               private: e.link.private,
               generated: e.link.generated,
               hits: e.hits(),
               last_used: ago(e.last_used(), now()),
               created: fmt_time(e.link.created_at),
               aliases: e.aliases.clone(),
               history: e.history.iter().map(HistoryRow::from).collect() }
    }
}

#[derive(Template)]
#[template(path = "box.html")]
struct BoxPage<'a> {
    user: &'a User,
    v: &'a str,
    origin: &'a str,
    /// the name in the box ("" on the front page)
    name: &'a str,
    /// the URL in the box
    url: &'a str,
    /// set when `name` is an existing link
    link: Option<Details>,
    can_edit: bool,
    saved: bool,
}

async fn home(State(state): State<AppState>, user: User) -> Response {
    page(StatusCode::OK, &BoxPage { user: &user,
                                    v: ASSET_VERSION,
                                    origin: &state.public_origin,
                                    name: "",
                                    url: "",
                                    link: None,
                                    can_edit: true,
                                    saved: false })
}

#[derive(Deserialize)]
struct EditQuery {
    #[serde(default)]
    keyword: String,
}

/// GET /edit?keyword=x -> /edit/x (forms without JavaScript; also the gate probe's "accepted" answer, a 303 to /).
async fn edit_query(_user: User, Query(q): Query<EditQuery>) -> Response {
    match normalize(q.keyword.trim()) {
        Some(k) => Redirect::to(&format!("/edit/{k}")).into_response(),
        None => Redirect::to("/").into_response(),
    }
}

#[derive(Deserialize)]
struct SavedQuery {
    #[serde(default)]
    saved: Option<String>,
}

async fn edit_page(State(state): State<AppState>, user: User, Path(raw): Path<String>, Query(q): Query<SavedQuery>)
                   -> Response {
    let Some(keyword) = normalize(&raw).map(Cow::into_owned) else { return Redirect::to("/").into_response() };
    let entry = state.store.get(&keyword);
    // an alias, or a different spelling: show the link under its own name
    let canonical = entry.as_ref().map_or(keyword.as_str(), |e| e.link.keyword.as_str());
    if canonical != raw {
        let saved = if q.saved.is_some() { "?saved=1" } else { "" };
        return Redirect::to(&format!("/edit/{canonical}{saved}")).into_response();
    }
    let link = entry.as_deref().map(Details::of);
    let can_edit = entry.as_ref().is_none_or(|e| user.may_edit(&e.link.owner));
    let url = entry.as_ref().map(|e| e.link.url.clone()).unwrap_or_default();
    page(StatusCode::OK, &BoxPage { user: &user,
                                    v: ASSET_VERSION,
                                    origin: &state.public_origin,
                                    name: &keyword,
                                    url: &url,
                                    link,
                                    can_edit,
                                    saved: q.saved.is_some() })
}

// ------------------------------------------------------------------------------------------------------- saving

/// Form posts must come from our own pages: Origin equal to the public origin, or (browsers that send no Origin)
/// Sec-Fetch-Site: same-origin. The gate cookie is SameSite=Lax as well; this is the second lock.
fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    match headers.get("origin").and_then(|v| v.to_str().ok()) {
        Some(o) => o == origin,
        None => headers.get("sec-fetch-site").is_some_and(|v| v == "same-origin"),
    }
}

fn wants_json(headers: &HeaderMap) -> bool {
    headers.get(ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("application/json"))
}

#[derive(Serialize, Default)]
struct Answer {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    url: String,
    /// the full link to copy (public origin + name)
    #[serde(skip_serializing_if = "String::is_empty")]
    link: String,
    /// hidden from the list (only its owner and admins see it there)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
}

/// An error, as JSON (the box's script) or as a page (plain forms).
fn fail(json: bool, status: StatusCode, msg: &str) -> Response {
    if json {
        (status, Json(Answer { ok: false, error: Some(msg.into()), ..Default::default() })).into_response()
    } else {
        page(status, &Message { title: "Not saved", body: msg })
    }
}

fn store_failed(json: bool, e: StoreError) -> Response {
    match e {
        StoreError::Taken(_) => fail(json, StatusCode::CONFLICT, &format!("Not saved: {e}.")),
        StoreError::NotFound => fail(json, StatusCode::NOT_FOUND, "There's no such link (any more)."),
        e => {
            error!("store: {e}");
            fail(json, StatusCode::INTERNAL_SERVER_ERROR, "The link store had an error. Nothing was changed. Try again.")
        }
    }
}

/// Saved: JSON for the box's script, else back to the link's page with the "saved" drawer open.
fn saved(json: bool, state: &AppState, e: &Entry) -> Response {
    if json {
        Json(Answer { ok: true,
                      error: None,
                      name: e.link.keyword.clone(),
                      url: e.link.url.clone(),
                      link: format!("{}/{}", state.public_origin, e.link.keyword),
                      hidden: e.link.private }).into_response()
    } else {
        Redirect::to(&format!("/edit/{}?saved=1", e.link.keyword)).into_response()
    }
}

#[derive(Deserialize)]
struct NewForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
}

/// The box: name (optional) + URL. A new name is the user's; an existing one they may edit gets the new URL (its
/// description, unlisted flag and aliases stay). No name: a free short name is made up, unlisted.
async fn new_post(State(state): State<AppState>, user: User, headers: HeaderMap, Form(form): Form<NewForm>) -> Response {
    let json = wants_json(&headers);
    if !same_origin(&headers, &state.public_origin) {
        metrics::REJECTED_POSTS.inc();
        return fail(json, StatusCode::FORBIDDEN, "That form didn't come from this site.");
    }
    let url = match valid_target(&form.url) {
        Ok(u) => u,
        Err(why) => return fail(json, StatusCode::BAD_REQUEST, why),
    };
    let wanted = form.name.trim().trim_start_matches("go/");
    let link = if wanted.is_empty() {
        let mut l = Link::new(&state.store.free_name(), &url, &user.name);
        (l.generated, l.private) = (true, true);
        l
    } else {
        let Some(keyword) = normalize(wanted).map(Cow::into_owned) else {
            return fail(json, StatusCode::BAD_REQUEST, BAD_NAME);
        };
        match state.store.get(&keyword) {
            Some(e) if e.link.keyword != keyword => {
                return fail(json, StatusCode::CONFLICT, &format!("go/{keyword} is another name for go/{}.", e.link.keyword));
            }
            Some(e) if !user.may_edit(&e.link.owner) => {
                return fail(json, StatusCode::FORBIDDEN, &format!("go/{keyword} is {}'s link.", e.link.owner));
            }
            Some(e) => Link { url, ..e.link.clone() },
            None => Link::new(&keyword, &url, &user.name),
        }
    };
    match state.store.save(link, &user.name).await {
        Ok(e) => {
            metrics::SAVES.inc();
            info!(user = %user.name, keyword = %e.link.keyword, url = %e.link.url, generated = e.link.generated, "saved");
            saved(json, &state, &e)
        }
        Err(e) => store_failed(json, e),
    }
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
    alias: String,
    #[serde(default)]
    action: String,
}

/// The details forms on /edit/<name>: save (URL, description, unlisted, owner), delete, alias_add, alias_remove.
async fn edit_post(State(state): State<AppState>, user: User, Path(raw): Path<String>, headers: HeaderMap,
                   Form(form): Form<EditForm>)
                   -> Response {
    let json = wants_json(&headers);
    if !same_origin(&headers, &state.public_origin) {
        metrics::REJECTED_POSTS.inc();
        return fail(json, StatusCode::FORBIDDEN, "That form didn't come from this site.");
    }
    let Some(keyword) = normalize(&raw).map(Cow::into_owned) else {
        return fail(json, StatusCode::BAD_REQUEST, BAD_NAME);
    };
    let existing = state.store.get(&keyword);
    if let Some(e) = &existing {
        if e.link.keyword != keyword {
            return fail(json, StatusCode::CONFLICT, &format!("go/{keyword} is another name for go/{}.", e.link.keyword));
        }
        if !user.may_edit(&e.link.owner) {
            return fail(json, StatusCode::FORBIDDEN, "Only the link's owner or an admin can change it.");
        }
    }

    match form.action.as_str() {
        "delete" => {
            return match state.store.delete(&keyword, &user.name).await {
                Ok(_) => {
                    metrics::DELETES.inc();
                    info!(user = %user.name, keyword, "deleted");
                    if json {
                        Json(Answer { ok: true, ..Default::default() }).into_response()
                    } else {
                        Redirect::to("/").into_response()
                    }
                }
                Err(e) => store_failed(json, e),
            };
        }
        "alias_add" | "alias_remove" => {
            if existing.is_none() {
                return fail(json, StatusCode::NOT_FOUND, "Save the link first, then add other names.");
            }
            let Some(alias) = normalize(form.alias.trim().trim_start_matches("go/")).map(Cow::into_owned) else {
                return fail(json, StatusCode::BAD_REQUEST, BAD_NAME);
            };
            let done = if form.action == "alias_add" {
                state.store.add_alias(&keyword, &alias, &user.name).await
            } else if existing.as_ref().is_some_and(|e| e.aliases.contains(&alias)) {
                state.store.remove_alias(&alias, &user.name).await
            } else {
                Err(StoreError::NotFound)
            };
            return match done {
                Ok(e) => {
                    metrics::SAVES.inc();
                    info!(user = %user.name, keyword, alias, action = form.action, "alias");
                    saved(json, &state, &e)
                }
                Err(e) => store_failed(json, e),
            };
        }
        _ => {}
    }

    let description: String = form.description.trim().chars().take(MAX_DESCRIPTION).collect();
    // Owner: the creator; only an admin can hand a link to someone else.
    let owner = match (&existing, form.owner.as_deref().map(str::trim)) {
        (_, Some(o)) if user.admin && !o.is_empty() => o.to_ascii_lowercase(),
        (Some(e), _) => e.link.owner.clone(),
        (None, _) => user.name.clone(),
    };
    let url = match valid_target(&form.url) {
        Ok(u) => u,
        Err(why) => return fail(json, StatusCode::BAD_REQUEST, why),
    };
    let mut link = existing.as_ref().map_or_else(|| Link::new(&keyword, &url, &owner), |e| e.link.clone());
    (link.url, link.owner, link.description, link.private) = (url, owner, description, form.private.is_some());
    match state.store.save(link, &user.name).await {
        Ok(e) => {
            metrics::SAVES.inc();
            info!(user = %user.name, keyword, url = %e.link.url, owner = %e.link.owner, "saved");
            saved(json, &state, &e)
        }
        Err(e) => store_failed(json, e),
    }
}

#[derive(Serialize)]
struct UrlInfo {
    name: String,
    exists: bool,
    /// set when the asked name is an alias: `name` is then the link's own name
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    url: String,
    owner: String,
    can_edit: bool,
}

/// What the box's script needs after "name ⏎": is it taken, by whom, may I change it.
async fn api_url(State(state): State<AppState>, user: User, Path(raw): Path<String>) -> Response {
    let Some(keyword) = normalize(&raw).map(Cow::into_owned) else {
        return fail(true, StatusCode::BAD_REQUEST, BAD_NAME);
    };
    let info = match state.store.get(&keyword) {
        Some(e) => UrlInfo { alias: (e.link.keyword != keyword).then(|| keyword.clone()),
                             name: e.link.keyword.clone(),
                             exists: true,
                             url: e.link.url.clone(),
                             owner: e.link.owner.clone(),
                             can_edit: user.may_edit(&e.link.owner) },
        None => UrlInfo { name: keyword, exists: false, alias: None, url: String::new(), owner: String::new(), can_edit: true },
    };
    ([(CACHE_CONTROL, "private, no-store")], Json(info)).into_response()
}

// ------------------------------------------------------------------------------------------------------- the list

struct Row {
    keyword: String,
    aliases: String,
    url: String,
    description: String,
    owner: String,
    hits: u64,
    last_used: String,
    private: bool,
    generated: bool,
    can_edit: bool,
    mine: bool,
}

#[derive(Template)]
#[template(path = "links.html")]
struct LinksPage<'a> {
    user: &'a User,
    v: &'a str,
    rows: Vec<Row>,
    total: usize,
    q: &'a str,
    sort: &'a str,
    mine: bool,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    sort: String,
    #[serde(default)]
    mine: Option<String>,
}

async fn links_page(State(state): State<AppState>, user: User, Query(lq): Query<ListQuery>) -> Response {
    let q = lq.q.trim().to_ascii_lowercase();
    let mine = lq.mine.is_some();
    let sort = match lq.sort.as_str() {
        s @ ("name" | "recent" | "stale") => s,
        _ => "hits",
    };
    let mut all = state.store.all(); // most used first
    let total = all.len();
    match sort {
        "name" => all.sort_by(|a, b| a.link.keyword.cmp(&b.link.keyword)),
        "recent" => all.sort_by_key(|e| std::cmp::Reverse(e.last_used())),
        "stale" => all.sort_by_key(|e| e.last_used()), // never used first
        _ => {}
    }
    let t = now();
    let rows = all.iter()
                  .filter(|e| if mine { e.link.owner == user.name } else { !e.link.private || user.may_edit(&e.link.owner) })
                  .filter(|e| {
                      q.is_empty()
                      || e.link.keyword.contains(&q)
                      || e.aliases.iter().any(|a| a.contains(&q))
                      || e.link.description.to_ascii_lowercase().contains(&q)
                      || e.link.url.to_ascii_lowercase().contains(&q)
                      || e.link.owner == q
                  })
                  .map(|e| Row { keyword: e.link.keyword.clone(),
                                 aliases: e.aliases.iter().map(|a| format!("go/{a}")).collect::<Vec<_>>().join(", "),
                                 url: e.link.url.clone(),
                                 description: e.link.description.clone(),
                                 owner: e.link.owner.clone(),
                                 hits: e.hits(),
                                 last_used: ago(e.last_used(), t),
                                 private: e.link.private,
                                 generated: e.link.generated,
                                 can_edit: user.may_edit(&e.link.owner),
                                 mine: e.link.owner == user.name })
                  .collect();
    page(StatusCode::OK, &LinksPage { user: &user, v: ASSET_VERSION, rows, total, q: &q, sort, mine })
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
        let auth = Arc::new(Auth::new("10.12.0.10".parse().unwrap(),
                                      crate::secretfile::SecretFile::fixed(SECRET.as_bytes()),
                                      Some("welcome-users".into()),
                                      "infra-admins".into()));
        let state = AppState::new(Arc::new(Store::open(&dir.join("links.db")).unwrap()), auth, ORIGIN, "Ask help@.", 60);
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

    async fn post_as(app: &Router, uri: &str, user: &str, admin: bool, origin: Option<&str>, json: bool, form: &str)
                     -> (StatusCode, HeaderMap, String) {
        let mut b = req("POST", uri, Some(user), admin).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        if json {
            b = b.header("accept", "application/json");
        }
        send(app, b.body(Body::from(form.to_string())).unwrap()).await
    }

    async fn post(app: &Router, uri: &str, user: &str, admin: bool, origin: Option<&str>, form: &str)
                  -> (StatusCode, HeaderMap, String) {
        post_as(app, uri, user, admin, origin, false, form).await
    }

    fn location(h: &HeaderMap) -> &str {
        h.get(LOCATION).unwrap().to_str().unwrap()
    }

    fn jget(body: &str, key: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| panic!("not JSON: {body}"));
        match &v[key] {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    #[tokio::test]
    async fn never_anonymous() {
        let (app, dir) = mkapp("anon", "10.12.0.10");
        for path in ["/", "/wiki", "/edit/wiki", "/links", "/edit?keyword=x", "/api/url/x"] {
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
    async fn the_box_flow() {
        let (app, dir) = mkapp("box", "10.12.0.10");
        // front page: the box, with the script
        let (s, h, body) = get(&app, "/", Some("alice")).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("id=\"name\"") && body.contains("/static/app.js?v="));
        assert!(h.get(CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap().contains("script-src 'self'"));
        // an unknown name: its create page (the box with the name filled in)
        let (s, h, _) = get(&app, "/Wiki", Some("alice")).await;
        assert_eq!((s, location(&h)), (StatusCode::TEMPORARY_REDIRECT, "/edit/wiki"));
        let (_, _, body) = get(&app, "/edit/wiki", Some("alice")).await;
        assert!(body.contains("value=\"wiki\""));
        // name + URL from the box, as the script sends it
        let (s, _, body) = post_as(&app, "/new", "alice", false, Some(ORIGIN), true, "name=wiki&url=https%3A%2F%2Fwiki.example%2Fbooks%2F").await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!((jget(&body, "ok"), jget(&body, "name"), jget(&body, "link")),
                   ("true".into(), "wiki".into(), "https://go.wupinyin.co.uk/wiki".into()));
        let (s, h, _) = get(&app, "/WIKI", Some("bob")).await;
        assert_eq!((s, location(&h)), (StatusCode::TEMPORARY_REDIRECT, "https://wiki.example/books/"));
        let (_, h, _) = get(&app, "/wiki/page/x?y=1", Some("bob")).await;
        assert_eq!(location(&h), "https://wiki.example/books/page/x?y=1");
        // the same box without JavaScript: a plain form post, back to the link's page with the drawer open
        let (s, h, _) = post(&app, "/new", "alice", false, Some(ORIGIN), "name=gh&url=https%3A%2F%2Fgithub.com%2F%25s%3Ftab%3D1").await;
        assert_eq!((s, location(&h)), (StatusCode::SEE_OTHER, "/edit/gh?saved=1"));
        let (_, _, body) = get(&app, "/edit/gh?saved=1", Some("alice")).await;
        assert!(body.contains("id=\"copy\"") && body.contains("https://go.wupinyin.co.uk/gh"));
        let (_, h, _) = get(&app, "/gh/wupinyin/iac?x=2", Some("bob")).await;
        assert_eq!(location(&h), "https://github.com/wupinyin/iac?tab=1&x=2");
        // what the script asks after "name ⏎"
        let (_, _, body) = get(&app, "/api/url/WIKI", Some("bob")).await;
        assert_eq!((jget(&body, "exists"), jget(&body, "owner"), jget(&body, "can_edit")),
                   ("true".into(), "alice".into(), "false".into()));
        let (_, _, body) = get(&app, "/api/url/nothing-here", Some("bob")).await;
        assert_eq!(jget(&body, "exists"), "false");
        // bob can't take alice's name from the box; alice re-saving keeps the description
        let (s, _, body) = post_as(&app, "/new", "bob", false, Some(ORIGIN), true, "name=wiki&url=https%3A%2F%2Fevil.example%2F").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(jget(&body, "error").contains("alice"));
        post(&app, "/edit/wiki", "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fwiki.example%2Fbooks%2F&description=the+wiki&action=save").await;
        post_as(&app, "/new", "alice", false, Some(ORIGIN), true, "name=wiki&url=https%3A%2F%2Fwiki.example%2F").await;
        let (_, _, body) = get(&app, "/links", Some("bob")).await;
        assert!(body.contains("go/wiki") && body.contains("the wiki") && body.contains("https://wiki.example/\""), "{body}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn short_names_for_pasted_urls() {
        let (app, dir) = mkapp("short", "10.12.0.10");
        let (s, _, body) = post_as(&app, "/new", "alice", false, Some(ORIGIN), true, "name=&url=https%3A%2F%2Fexample.com%2Fa%2Fvery%2Flong%2Fpath").await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let name = jget(&body, "name");
        assert_eq!(name.len(), 5);
        let (_, h, _) = get(&app, &format!("/{name}"), Some("bob")).await;
        assert_eq!(location(&h), "https://example.com/a/very/long/path");
        // unlisted: bob's list doesn't show it, alice's does (tagged "short")
        assert!(!get(&app, "/links", Some("bob")).await.2.contains(&format!("go/{name}")));
        let mine = get(&app, "/links?mine=1", Some("alice")).await.2;
        assert!(mine.contains(&format!("go/{name}")) && mine.contains("short") && mine.contains(">hidden"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn hidden_links_are_marked_for_those_who_see_them() {
        let (app, dir) = mkapp("hidden", "10.12.0.10");
        post(&app, "/edit/secret-plan", "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fplan.example%2F&private=1&action=save").await;
        post(&app, "/new", "alice", false, Some(ORIGIN), "name=open&url=https%3A%2F%2Fopen.example%2F").await;
        // others: not in the list (the name still works)
        let bob = get(&app, "/links", Some("bob")).await.2;
        assert!(!bob.contains("go/secret-plan") && bob.contains("go/open"));
        assert_eq!(location(&get(&app, "/secret-plan", Some("bob")).await.1), "https://plan.example/");
        // the owner: in the list, marked hidden (no "alice's", it's hers)
        let alice = get(&app, "/links", Some("alice")).await.2;
        assert!(alice.contains("go/secret-plan") && alice.contains("class=\"hidden\"") && alice.contains(">hidden</span>"), "{alice}");
        assert!(!alice.contains("alice&#39;s") && !alice.contains("alice's"));
        // an admin: in the list, marked hidden with the owner
        let root = send(&app, req("GET", "/links", Some("root"), true).body(Body::empty()).unwrap()).await.2;
        assert!(root.contains("go/secret-plan") && (root.contains("hidden · alice&#39;s") || root.contains("hidden · alice's")), "{root}");
        // its page says so; a short link's save answer says so
        assert!(get(&app, "/edit/secret-plan", Some("alice")).await.2.contains("tag hidden"));
        let (_, _, body) = post_as(&app, "/new", "alice", false, Some(ORIGIN), true, "url=https%3A%2F%2Fx.example%2F").await;
        assert_eq!(jget(&body, "hidden"), "true");
        let (_, _, body) = post_as(&app, "/new", "alice", false, Some(ORIGIN), true, "name=vis&url=https%3A%2F%2Fx.example%2F").await;
        assert_eq!(jget(&body, "hidden"), "null", "not hidden: no field");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn aliases() {
        let (app, dir) = mkapp("alias", "10.12.0.10");
        post(&app, "/new", "alice", false, Some(ORIGIN), "name=wiki&url=https%3A%2F%2Fwiki.example%2F").await;
        let (s, h, _) = post(&app, "/edit/wiki", "alice", false, Some(ORIGIN), "action=alias_add&alias=Docs").await;
        assert_eq!((s, location(&h)), (StatusCode::SEE_OTHER, "/edit/wiki?saved=1"));
        assert_eq!(location(&get(&app, "/docs/x", Some("bob")).await.1), "https://wiki.example/x");
        // the alias's page is the link's page; its name can't be taken or saved over
        assert_eq!(location(&get(&app, "/edit/docs", Some("bob")).await.1), "/edit/wiki");
        let (s, _, _) = post_as(&app, "/new", "bob", false, Some(ORIGIN), true, "name=docs&url=https%3A%2F%2Fx.example%2F").await;
        assert_eq!(s, StatusCode::CONFLICT);
        post(&app, "/new", "bob", false, Some(ORIGIN), "name=bobs&url=https%3A%2F%2Fb.example%2F").await;
        let (s, _, _) = post(&app, "/edit/bobs", "bob", false, Some(ORIGIN), "action=alias_add&alias=wiki").await;
        assert_eq!(s, StatusCode::CONFLICT, "an alias can't be another link's name");
        // only the owner (or an admin) adds or removes them
        let (s, _, _) = post(&app, "/edit/wiki", "bob", false, Some(ORIGIN), "action=alias_add&alias=bobwiki").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let page = get(&app, "/edit/wiki", Some("alice")).await.2;
        assert!(page.contains("go/docs"));
        assert!(get(&app, "/links", Some("bob")).await.2.contains("also go/docs"));
        let (s, _, _) = post(&app, "/edit/wiki", "alice", false, Some(ORIGIN), "action=alias_remove&alias=docs").await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(location(&get(&app, "/docs", Some("bob")).await.1), "/edit/docs");
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
        assert!(page.contains("alice's link"), "{page}");
        // a non-admin can't take ownership through the form
        post(&app, "/edit/mine", "alice", false, Some(ORIGIN), "url=https%3A%2F%2Fa.example%2F2&owner=bob&action=save").await;
        assert!(get(&app, "/edit/mine", Some("alice")).await.2.contains("owner alice"));
        // an admin can change and hand over
        let (s, _, _) = post(&app, "/edit/mine", "root", true, Some(ORIGIN), "url=https%3A%2F%2Fa.example%2F3&owner=bob&action=save").await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        let (s, _, body) = post_as(&app, "/edit/mine", "bob", false, Some(ORIGIN), true, "action=delete").await;
        assert_eq!((s, jget(&body, "ok")), (StatusCode::OK, "true".into()));
        assert_eq!(location(&get(&app, "/mine", Some("bob")).await.1), "/edit/mine");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn forms_must_come_from_our_origin() {
        let (app, dir) = mkapp("origin", "10.12.0.10");
        let form = "url=https%3A%2F%2Fa.example%2F&action=save";
        for uri in ["/edit/a", "/new"] {
            assert_eq!(post(&app, uri, "alice", false, None, form).await.0, StatusCode::FORBIDDEN);
            assert_eq!(post(&app, uri, "alice", false, Some("https://evil.example"), form).await.0, StatusCode::FORBIDDEN);
        }
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
            let (s, _, _) = post_as(&app, "/new", "alice", false, Some(ORIGIN), true, &format!("name=x&url={bad}")).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        }
        for reserved in ["edit", "metrics", "oauth2", "new", "api", "bad name"] {
            let (s, _, _) = post_as(&app, "/new", "alice", false, Some(ORIGIN), true, &format!("name={reserved}&url=https%3A%2F%2Fa.example%2F")).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{reserved}");
        }
        post(&app, "/edit/x", "alice", false, Some(ORIGIN),
             "url=https%3A%2F%2Fa.example%2F%3Fa%3D%22%3E%3Cscript%3E&description=%3Cscript%3Ealert(1)%3C%2Fscript%3E&action=save").await;
        for page in ["/links", "/edit/x"] {
            let body = get(&app, page, Some("alice")).await.2;
            assert!(!body.contains("<script>"), "{page}: {body}");
            assert!(body.contains("&#60;script&#62;") || body.contains("&lt;script&gt;"), "{page}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn links_sorting() {
        let (app, dir) = mkapp("sorting", "10.12.0.10");
        for n in ["aaa", "bbb", "ccc"] {
            post(&app, "/new", "alice", false, Some(ORIGIN), &format!("name={n}&url=https%3A%2F%2F{n}.example%2F")).await;
        }
        get(&app, "/bbb", Some("bob")).await;
        get(&app, "/bbb", Some("bob")).await;
        get(&app, "/ccc", Some("bob")).await;
        let order = |body: String| {
            let mut v: Vec<(usize, &str)> = ["aaa", "bbb", "ccc"].iter().map(|n| (body.find(&format!("go/{n}<")).unwrap(), *n)).collect();
            v.sort();
            v.into_iter().map(|(_, n)| n).collect::<Vec<_>>()
        };
        assert_eq!(order(get(&app, "/links", Some("bob")).await.2), ["bbb", "ccc", "aaa"]);
        assert_eq!(order(get(&app, "/links?sort=name", Some("bob")).await.2), ["aaa", "bbb", "ccc"]);
        assert_eq!(order(get(&app, "/links?sort=stale", Some("bob")).await.2)[0], "aaa", "never used first");
        assert!(get(&app, "/links?sort=stale", Some("bob")).await.2.contains("never"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_gate_secret_is_never_shown() {
        let (app, dir) = mkapp("secret", "10.12.0.10");
        post(&app, "/new", "alice", false, Some(ORIGIN), "name=a&url=https%3A%2F%2Fa.example%2F").await;
        for path in ["/", "/links", "/edit/a", "/edit/new-one", "/a", "/nope", "/edit/edit", "/%00", "/metrics", "/api/url/a",
                     "/static/app.js"]
        {
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

    #[tokio::test]
    async fn redirect_caching_and_denied_texts() {
        let (app, dir) = mkapp("cache", "10.12.0.10");
        post(&app, "/new", "alice", false, Some(ORIGIN), "name=wiki&url=https%3A%2F%2Fwiki.example%2F").await;
        let (_, h, _) = get(&app, "/wiki", Some("bob")).await;
        assert_eq!(h.get(CACHE_CONTROL).unwrap(), "private, max-age=60");
        let (_, h, _) = get(&app, "/not-yet", Some("bob")).await;
        assert_eq!(h.get(CACHE_CONTROL).unwrap(), "private, no-store", "a create-page redirect is never cached");
        let (_, _, body) = get(&app, "/", None).await;
        assert!(body.contains("Sign in through https://go.wupinyin.co.uk"));
        let r = Request::get("/").header("x-gate-auth", SECRET)
                                 .header("x-auth-request-preferred-username", "eve")
                                 .header("x-auth-request-groups", "residents")
                                 .body(Body::empty())
                                 .unwrap();
        assert!(send(&app, r).await.2.contains("Ask help@."));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn search_shortcut_paths_are_decoded() {
        let (app, dir) = mkapp("typed", "10.12.0.10");
        post(&app, "/new", "alice", false, Some(ORIGIN), "name=git&url=https%3A%2F%2Fgit.example%2F").await;
        post(&app, "/new", "alice", false, Some(ORIGIN), "name=w&url=https%3A%2F%2Fwiki.example%2Fsearch%3Fterm%3D%25s").await;
        let loc = |p: &'static str| {
            let app = app.clone();
            async move { location(&get(&app, p, Some("bob")).await.1).to_string() }
        };
        assert_eq!(loc("/git%2Finfra%2Foci-images").await, "https://git.example/infra/oci-images");
        assert_eq!(loc("/GIT%2Finfra%2Fx%3Ftab%3D1").await, "https://git.example/infra/x?tab=1");
        assert_eq!(loc("/w%20kanidm%20reauth").await, "https://wiki.example/search?term=kanidm%20reauth");
        assert_eq!(loc("/w%2Fkanidm").await, "https://wiki.example/search?term=kanidm");
        assert_eq!(loc("/git%2Fa%20b%3Cc").await, "https://git.example/a%20b%3Cc");
        // a plain path keeps its own encoding untouched
        assert_eq!(loc("/git/a%2Fb").await, "https://git.example/a%2Fb");
        assert_eq!(loc("/nothing%2Fhere").await, "/edit/nothing");
        let _ = std::fs::remove_dir_all(dir);
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
        let t = 1_791_763_200;
        assert_eq!(ago(0, t), "never");
        assert_eq!(ago(t - 5, t), "just now");
        assert_eq!(ago(t - 3 * 3600, t), "3 h ago");
        assert_eq!(ago(t - 10 * 86_400, t), "10 d ago");
        assert_eq!(ago(t - 90 * 86_400, t), "3 mo ago");
    }
}
