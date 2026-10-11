// go/ links: the keyboard flow for the box. The pages work without this file; it only adds the steps:
//   name ⏎  →  paste the link ⏎  →  the link is saved and selected for copying.
// A link pasted as the name gets a short name instead. Esc steps back, / jumps to the box (or the list's filter).
// No framework, no dependencies; user text only ever goes in via textContent (CSP: script-src 'self').
(() => {
  "use strict";
  const $ = (s) => document.querySelector(s);
  const typing = (el) => el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);

  document.addEventListener("keydown", (e) => {
    if (e.key === "/" && !e.metaKey && !e.ctrlKey && !e.altKey && !typing(document.activeElement)) {
      const t = $("#name") || $("#q");
      if (t) { e.preventDefault(); t.focus(); t.select(); }
    }
  });

  // forms that ask first (delete)
  for (const f of document.querySelectorAll("form[data-confirm]")) {
    f.addEventListener("submit", (e) => { if (!confirm(f.dataset.confirm)) e.preventDefault(); });
  }

  const selectCopy = () => {
    const a = $("#copy");
    if (!a) return;
    const r = document.createRange();
    r.selectNodeContents(a);
    const s = getSelection();
    s.removeAllRanges();
    s.addRange(r);
  };

  const box = $("#box");
  if (!box) return;
  const name = $("#name"), url = $("#url"), drawer = $("#drawer");
  const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
  const isLink = (v) => /^https?:\/\/\S+$/i.test(v);
  const mac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  const bare = (v) => v.trim().replace(/^go\//i, "");

  const fit = () => { name.size = Math.max(3, name.value.length + 1); };
  const step = (s) => {
    box.dataset.step = s;
    if (s === "url") { fit(); url.focus(); url.select(); }
    else { name.removeAttribute("size"); name.focus(); }
  };
  const close = () => { drawer.className = "drawer"; drawer.textContent = ""; };
  const say = (kind, text) => {
    drawer.textContent = "";
    drawer.className = "drawer open " + kind;
    const s = document.createElement("span");
    s.textContent = text;
    drawer.append(s);
  };
  const done = (link, hidden) => {
    drawer.textContent = "";
    drawer.className = "drawer open ok";
    const a = document.createElement("a");
    a.id = "copy";
    a.href = link;
    a.textContent = link;
    const h = document.createElement("span");
    h.className = "hint";
    h.textContent = mac ? "⌘C to copy" : "Ctrl+C to copy";
    drawer.append(a, h);
    if (hidden) {
      const n = document.createElement("span");
      n.className = "note";
      const t = document.createElement("span");
      t.className = "tag hidden";
      t.textContent = "hidden";
      n.append(t, " only you and admins see it in the list");
      drawer.append(n);
    }
    url.blur();
    selectCopy();
  };

  let busy = false;
  const save = async () => {
    const n = bare(name.value), u = url.value.trim();
    if (!u) { step("url"); return; }
    if (busy) return;
    busy = true;
    try {
      const r = await fetch("/new", {
        method: "POST",
        body: new URLSearchParams({ name: n, url: u }),
        headers: { accept: "application/json" },
      });
      let j;
      try { j = await r.json(); } catch { j = { ok: false, error: `The server answered ${r.status}.` }; }
      if (!j.ok) { say("err", j.error); return; }
      name.value = j.name;
      url.value = j.url;
      box.dataset.step = "url";
      fit();
      if (location.pathname !== "/edit/" + j.name) history.replaceState(null, "", "/edit/" + j.name);
      done(j.link, j.hidden);
    } catch {
      say("err", "Couldn't reach go/ (signed out? try reloading the page).");
    } finally {
      busy = false;
    }
  };

  name.addEventListener("keydown", async (e) => {
    if (e.key === "Escape") { e.preventDefault(); name.value = ""; close(); return; }
    if (e.key !== "Enter") return;
    e.preventDefault();
    const v = bare(name.value);
    if (!v) return;
    if (isLink(v)) { url.value = v; name.value = ""; save(); return; } // a pasted link: give it a short name
    if (!NAME.test(v)) { say("err", "Names are letters, digits, dots, dashes and underscores (64 at most)."); return; }
    close();
    try {
      const r = await fetch("/api/url/" + encodeURIComponent(v), { headers: { accept: "application/json" } });
      const j = await r.json();
      if (!r.ok) { say("err", j.error || `The server answered ${r.status}.`); return; }
      if (j.exists && !j.can_edit) { say("err", `go/${j.name} is ${j.owner}'s link → ${j.url}`); return; }
      if (j.alias) say("info", `go/${j.alias} is another name for go/${j.name}.`);
      name.value = j.name;
      url.value = j.exists ? j.url : "";
    } catch { /* the save will say what's wrong */ }
    step("url");
  });
  name.addEventListener("input", () => { if (drawer.classList.contains("ok")) close(); });

  url.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.preventDefault(); close(); step("name"); }
    else if (e.key === "Enter") { e.preventDefault(); save(); }
  });
  url.addEventListener("paste", () => { if (drawer.classList.contains("ok")) close(); });

  box.addEventListener("submit", (e) => { e.preventDefault(); save(); });
  box.classList.add("js");

  if ($("#copy")) { box.dataset.step = "url"; fit(); selectCopy(); }
  else step(name.value ? "url" : "name");
})();
