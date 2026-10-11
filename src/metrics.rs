//! A handful of Prometheus counters, hand-rolled: plain atomics on the hot path, text rendered on scrape.

use std::{fmt::Write, sync::atomic::{AtomicU64, Ordering}};

pub struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

pub static REDIRECTS: Counter = Counter::new();
pub static MISSES: Counter = Counter::new();
pub static SAVES: Counter = Counter::new();
pub static DELETES: Counter = Counter::new();
pub static DENIED: Counter = Counter::new();
pub static REJECTED_POSTS: Counter = Counter::new();
pub static FLUSHES: Counter = Counter::new();
pub static FLUSH_ERRORS: Counter = Counter::new();

pub fn render(links: usize) -> String {
    let mut out = String::with_capacity(1024);
    let counters: [(&str, &str, &Counter); 8] =
        [("linkshrink_redirects_total", "Redirects to a link's URL.", &REDIRECTS),
         ("linkshrink_misses_total", "Unknown keywords (sent to the create page).", &MISSES),
         ("linkshrink_saves_total", "Links created or changed.", &SAVES),
         ("linkshrink_deletes_total", "Links deleted.", &DELETES),
         ("linkshrink_denied_total", "Requests without a believable gate identity (403).", &DENIED),
         ("linkshrink_rejected_posts_total", "Form posts refused by the Origin check.", &REJECTED_POSTS),
         ("linkshrink_flushes_total", "Hit-count flushes that wrote at least one row.", &FLUSHES),
         ("linkshrink_flush_errors_total", "Hit-count flushes that failed.", &FLUSH_ERRORS)];
    for (name, help, c) in counters {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}", c.get());
    }
    let _ = writeln!(out, "# HELP linkshrink_links Links stored.\n# TYPE linkshrink_links gauge\nlinkshrink_links {links}");
    out
}
