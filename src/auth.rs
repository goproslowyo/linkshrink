//! Who is asking. The Kanidm gate (Caddy forward_auth -> oauth2-proxy) signs people in and passes their username and
//! groups as X-Auth-Request-* headers. They are believed only when the TCP peer is Caddy AND the request carries
//! the backend secret in X-Gate-Auth exactly once (same rule as welcome and pager, iac #97). Nothing here is
//! anonymous: without a believable identity every page except /healthz is a 403.

use std::{net::{IpAddr, SocketAddr},
          sync::atomic::{AtomicBool, Ordering}};

use axum::{extract::{ConnectInfo, FromRequestParts, connect_info::MockConnectInfo},
           http::{HeaderMap, request::Parts}};
use tracing::warn;

use crate::{metrics, secretfile::SecretFile, web::{AppState, Denied}};

pub const GATE_AUTH: &str = "x-gate-auth";
pub const USER: &str = "x-auth-request-preferred-username";
pub const USER_FALLBACK: &str = "x-auth-request-user";
pub const GROUPS: &str = "x-auth-request-groups";
const MAX_GROUPS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub admin: bool,
}

impl User {
    pub fn may_edit(&self, owner: &str) -> bool {
        self.admin || self.name == owner
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Why {
    NotFromProxy,
    BadSecret,
    NoUser,
    NotMember,
}

pub struct Auth {
    trusted_proxy: IpAddr,
    secret: SecretFile,
    user_group: Option<String>,
    admin_group: String,
    warned: AtomicBool,
}

/// The value of a header that must appear exactly once.
fn single<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut vals = headers.get_all(name).iter();
    let first = vals.next()?;
    if vals.next().is_some() {
        return None;
    }
    first.to_str().ok()
}

/// A plain Kanidm name: "@domain" (the spn suffix) removed, then ^[a-z][a-z0-9._-]{0,63}$. Anything else is
/// dropped, not cleaned up.
fn plain_name(raw: &str) -> Option<&str> {
    let name = raw.trim();
    let name = name.split_once('@').map_or(name, |(n, _)| n);
    let mut chars = name.chars();
    let ok = name.len() <= 64
             && chars.next().is_some_and(|c| c.is_ascii_lowercase())
             && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    ok.then_some(name)
}

/// The comma-separated groups claim, each reduced by plain_name. Kanidm also lists group UUIDs: those fail
/// plain_name's first-letter rule only sometimes, so they are dropped explicitly.
pub fn parse_groups(header: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for g in header.split(',').filter_map(plain_name) {
        let uuid = g.len() == 36 && g.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
        if !uuid && !out.contains(&g) && out.len() < MAX_GROUPS {
            out.push(g);
        }
    }
    out
}

impl Auth {
    pub fn new(trusted_proxy: IpAddr, secret: SecretFile, user_group: Option<String>, admin_group: String) -> Self {
        Self { trusted_proxy: trusted_proxy.to_canonical(), secret, user_group, admin_group, warned: AtomicBool::new(false) }
    }

    pub fn is_proxy(&self, peer: Option<IpAddr>) -> bool {
        peer.is_some_and(|p| p.to_canonical() == self.trusted_proxy)
    }

    pub fn identify(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> Result<User, Why> {
        if !self.is_proxy(peer) {
            return Err(Why::NotFromProxy);
        }
        let secret_ok = single(headers, GATE_AUTH).is_some_and(|s| self.secret.accepts(s.as_bytes()));
        if !secret_ok {
            if !self.warned.swap(true, Ordering::Relaxed) {
                warn!("request from the trusted proxy without a valid X-Gate-Auth: gate headers ignored (Caddy \
                       header_up missing, or a spoofed peer?)");
            }
            return Err(Why::BadSecret);
        }
        let name = single(headers, USER).or_else(|| single(headers, USER_FALLBACK))
                                        .and_then(plain_name)
                                        .ok_or(Why::NoUser)?;
        let groups = single(headers, GROUPS).map(parse_groups).unwrap_or_default();
        if let Some(needed) = &self.user_group {
            if !groups.contains(&needed.as_str()) {
                return Err(Why::NotMember);
            }
        }
        Ok(User { name: name.to_string(), admin: groups.contains(&self.admin_group.as_str()) })
    }
}

/// The TCP peer. (MockConnectInfo is what axum's test layer inserts instead; nothing outside the process can add
/// request extensions.)
pub fn peer(parts: &Parts) -> Option<IpAddr> {
    parts.extensions
         .get::<ConnectInfo<SocketAddr>>()
         .map(|ci| ci.0.ip())
         .or_else(|| parts.extensions.get::<MockConnectInfo<SocketAddr>>().map(|m| m.0.ip()))
}

impl FromRequestParts<AppState> for User {
    type Rejection = Denied;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        state.auth.identify(peer(parts), &parts.headers).map_err(|why| {
                                                            metrics::DENIED.inc();
                                                            Denied(why)
                                                        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const CADDY: &str = "10.12.0.10";
    const S: &str = "test-gate-secret-0123456789";

    fn auth(user_group: Option<&str>) -> Auth {
        Auth::new(CADDY.parse().unwrap(), SecretFile::fixed(S.as_bytes()), user_group.map(String::from), "infra-admins".into())
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn caddy() -> Option<IpAddr> {
        Some(CADDY.parse().unwrap())
    }

    #[test]
    fn needs_the_proxy_and_the_secret() {
        let a = auth(None);
        let ok = headers(&[(GATE_AUTH, S), (USER, "alice@idm.wupinyin.co.uk")]);
        assert_eq!(a.identify(caddy(), &ok).unwrap(), User { name: "alice".into(), admin: false });
        assert_eq!(a.identify(Some("10.12.0.99".parse().unwrap()), &ok), Err(Why::NotFromProxy));
        assert_eq!(a.identify(None, &ok), Err(Why::NotFromProxy));
        let not_it = format!("{S}x");
        let wrong = headers(&[(GATE_AUTH, &not_it), (USER, "alice")]);
        assert_eq!(a.identify(caddy(), &wrong), Err(Why::BadSecret));
        let missing = headers(&[(USER, "alice")]);
        assert_eq!(a.identify(caddy(), &missing), Err(Why::BadSecret));
        let twice = headers(&[(GATE_AUTH, S), (GATE_AUTH, S), (USER, "alice")]);
        assert_eq!(a.identify(caddy(), &twice), Err(Why::BadSecret));
        let prefix = headers(&[(GATE_AUTH, &S[..10]), (USER, "alice")]);
        assert_eq!(a.identify(caddy(), &prefix), Err(Why::BadSecret));
    }

    #[test]
    fn ipv4_mapped_peer_counts_as_the_proxy() {
        let a = auth(None);
        let ok = headers(&[(GATE_AUTH, S), (USER, "alice")]);
        assert!(a.identify(Some("::ffff:10.12.0.10".parse().unwrap()), &ok).is_ok());
    }

    #[test]
    fn usernames_must_be_plain() {
        let a = auth(None);
        for bad in ["", "Alice", "al ice", "<b>", "1alice", "alice,bob"] {
            let h = headers(&[(GATE_AUTH, S), (USER, bad)]);
            assert_eq!(a.identify(caddy(), &h), Err(Why::NoUser), "{bad:?}");
        }
        let two = headers(&[(GATE_AUTH, S), (USER, "alice"), (USER, "bob")]);
        assert_eq!(a.identify(caddy(), &two), Err(Why::NoUser));
        let fallback = headers(&[(GATE_AUTH, S), (USER_FALLBACK, "bob@idm.wupinyin.co.uk")]);
        assert_eq!(a.identify(caddy(), &fallback).unwrap().name, "bob");
    }

    #[test]
    fn groups_decide_membership_and_admin() {
        let a = auth(Some("welcome-users"));
        let member = headers(&[(GATE_AUTH, S),
                               (USER, "alice"),
                               (GROUPS, "residents@idm.wupinyin.co.uk, welcome-users@idm.wupinyin.co.uk")]);
        assert_eq!(a.identify(caddy(), &member).unwrap(), User { name: "alice".into(), admin: false });
        let admin = headers(&[(GATE_AUTH, S),
                              (USER, "bob"),
                              (GROUPS, "welcome-users@idm.wupinyin.co.uk,infra-admins@idm.wupinyin.co.uk")]);
        assert!(a.identify(caddy(), &admin).unwrap().admin);
        let outsider = headers(&[(GATE_AUTH, S), (USER, "eve"), (GROUPS, "residents")]);
        assert_eq!(a.identify(caddy(), &outsider), Err(Why::NotMember));
        let none = headers(&[(GATE_AUTH, S), (USER, "eve")]);
        assert_eq!(a.identify(caddy(), &none), Err(Why::NotMember));
    }

    #[test]
    fn parse_groups_drops_junk() {
        assert_eq!(parse_groups("a@x, b ,a,Bad,00000000-0000-0000-0000-000000000000,,c-d"), vec!["a", "b", "c-d"]);
    }
}
