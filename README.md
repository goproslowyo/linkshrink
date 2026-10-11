# linkshrink

go/ links: type `go/<name>` in a browser and land on the URL saved under that name. By d0nut and goproslowyo (2022-23);
this is wupinyin's fork, rewritten in 2026 and running at https://go.wupinyin.co.uk.

The core idea is unchanged from d0nut's version: every link lives in memory, so a redirect never waits on storage. A
redirect is one atomic load, one hash lookup and two relaxed atomic writes (hit count, last used): no lock, no I/O.

## Features

- **One box.** Type a name ⏎, paste the link ⏎, and the finished link drops down selected for copying. Paste a link as
  the name and it gets a 5-character short name. `/` jumps to the box, `Esc` steps back. It all works without
  JavaScript too.
- **Passthrough:** `go/gh/some/repo` with the URL `https://github.com/%s` goes to github.com/some/repo. Without `%s`,
  the rest is appended. The query string is merged in.
- **Browser search shortcuts work:** `go git/infra/x?y=1` arrives percent-encoded and is decoded back.
- **Aliases:** several names, one link, one hit count.
- **Hidden links:** only the owner and admins see them in the list, always marked as hidden.
- **Owners and admins:** the creator owns a link; admins can edit everything and hand links over.
- **The list:** filter; sort by most used, recently used, **stale first** or name; "mine".
- **History:** every change is in SQLite; the last 10 per link are kept in memory.
- **Prometheus counters** at `/metrics`, and `/healthz`.

## Identity: a trusted proxy

linkshrink has no login of its own. It sits behind a reverse proxy that signs people in (at wupinyin: Caddy
`forward_auth` to oauth2-proxy, with Kanidm as the IdP). The proxy passes `X-Auth-Request-Preferred-Username` /
`X-Auth-Request-User` and `X-Auth-Request-Groups`, the way oauth2-proxy does. Those headers are believed **only**
when the TCP peer is `LINKSHRINK_TRUSTED_PROXY` **and** the request carries the shared secret from
`LINKSHRINK_GATE_SECRET_FILE` in `X-Gate-Auth`. The file holds one line, or "current\nprevious" during a rotation; it is
re-read when it changes. Without a believable identity every route except `/healthz` answers 403.

## Settings (environment)

| Variable | Default | |
|---|---|---|
| `LINKSHRINK_LISTEN` | `127.0.0.1:8080` | |
| `LINKSHRINK_DB` | `/var/lib/linkshrink/links.db` | SQLite (WAL); `snapshot/links.db` next to it is a consistent copy for backups |
| `LINKSHRINK_TRUSTED_PROXY` | required | the proxy's address |
| `LINKSHRINK_GATE_SECRET_FILE` | required | the X-Gate-Auth secret (≥ 16 bytes) |
| `LINKSHRINK_USER_GROUP` | none | when set, users must also be in this group |
| `LINKSHRINK_ADMIN_GROUP` | `admins` | may edit and delete every link, sees every hidden link |
| `LINKSHRINK_PUBLIC_ORIGIN` | required | e.g. `https://go.example.com`: the only Origin a form post may come from, and the base of copied links |
| `LINKSHRINK_HELP` | `Ask an admin for access.` | shown to signed-in people who aren't in the user group |
| `LINKSHRINK_REDIRECT_CACHE_SECS` | `0` | let browsers keep a redirect this long (`private, max-age`): repeat visits skip the network; an edit can take that long to reach such a browser, and its repeat visits aren't counted. A redirect to a create page is never cached |
| `LINKSHRINK_FLUSH_SECS` | `5` | how often hit counts are written |
| `LINKSHRINK_LOG` | `info` | tracing filter |

`linkshrink healthcheck` GETs `/healthz` (exit 0 = healthy), for containers without curl.

## Running it yourself

`Dockerfile` builds a small image (one static binary, an empty base image, non-root, links in `/data`). `examples/`
has a Caddy + oauth2-proxy setup (`Caddyfile`) and the matching settings (`linkshrink.env`). Any sign-in proxy works
if it can (1) refuse unauthenticated requests, (2) pass the user and groups in the `X-Auth-Request-*` headers, and
(3) add a shared secret header. Run linkshrink where only that proxy can reach it.

## Build and test

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release          # fat LTO, static with musl; mimalloc as the allocator
```

## Where it runs (wupinyin)

This repo is the code's home. The image and deployment live in Forgejo `infra/oci-images`, directory `linkshrink/`:
the Containerfile (from scratch, built with cargo-auditable), the instance and deploy scripts, and the operator README.
That directory carries the same tree under `linkshrink/src` as a subtree. To bring changes over:

```sh
git -C <oci-images> subtree pull --prefix=linkshrink/src <this repo> main
```

or apply this repo's commits with `git format-patch` / `git am --directory=linkshrink/src`. The operator guide is in
the wiki: Host & Hypervisor → "go/ links (linkshrink): operating it".

## Performance

On 2 CPUs with 64 connections, against the 2023 version: redirects 2.3× faster (38k req/s, p50 1.4 ms), the list
page 41× faster, the link page 3×. Saving is slower (1.6k/s) because every save is fsync'd with a history row; the 2023
version wrote to Redis with persistence off. The full table is in `infra/oci-images` `linkshrink/README.md`.
