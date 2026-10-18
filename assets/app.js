(function () {
  "use strict";

  function relative(ms) {
    var s = Math.round((Date.now() - ms) / 1000);
    if (s < 45) return "just now";
    var m = Math.round(s / 60);
    if (m < 60) return m + " min ago";
    var h = Math.round(m / 60);
    if (h < 24) return h + " h ago";
    var d = Math.round(h / 24);
    if (d < 14) return d + " d ago";
    return new Date(ms).toLocaleDateString();
  }

  function clock(ms) {
    var d = new Date(ms), hm = ("0" + d.getHours()).slice(-2) + ":" + ("0" + d.getMinutes()).slice(-2);
    if (d.toDateString() === new Date().toDateString()) return hm;
    return d.toLocaleDateString(undefined, { day: "numeric", month: "short" }) + " " + hm;
  }

  function times() {
    document.querySelectorAll("time[datetime]").forEach(function (t) {
      var ms = Date.parse(t.getAttribute("datetime"));
      if (!isNaN(ms)) {
        t.textContent = t.classList.contains("clock") ? clock(ms) : relative(ms);
        t.title = new Date(ms).toLocaleString();
      }
    });
  }

  function dur(ms) {
    var s = Math.max(0, Math.ceil(ms / 1000));
    if (s < 60) return s + "s";
    if (s < 3600) return Math.floor(s / 60) + "m" + ("0" + (s % 60)).slice(-2) + "s";
    return Math.floor(s / 3600) + "h" + ("0" + Math.floor((s % 3600) / 60)).slice(-2) + "m";
  }

  function deadlines() {
    document.querySelectorAll("[data-deadline]").forEach(function (d) {
      var at = Number(d.getAttribute("data-deadline"));
      if (at) d.textContent = dur(at - Date.now()) + " left";
    });
  }

  function coalesce(fn) {
    var running = false, again = false;
    function run() {
      if (running) { again = true; return; }
      running = true;
      again = false;
      Promise.resolve().then(fn).catch(function () {}).then(function () {
        running = false;
        if (again) run();
      });
    }
    return run;
  }

  // The open state the page last set on each disclosure. Updates may open one, never close it,
  // and one the reader toggled keeps theirs.
  function defaults(root) {
    if (root.nodeName === "DETAILS") root.amxOpen = root.open;
    root.querySelectorAll("details").forEach(function (d) { d.amxOpen = d.open; });
  }
  document.addEventListener("toggle", function (e) {
    var d = e.target;
    if ("amxOpen" in d && d.open !== d.amxOpen) d.amxMine = true;
  }, true);

  // Updates `el` in place to match `to`, so focus, selection and scroll stay where they are.
  function morph(el, to) {
    if (el.nodeType !== 1) {
      if (el.nodeValue !== to.nodeValue) el.nodeValue = to.nodeValue;
      return;
    }
    var keep = el.nodeName === "DETAILS" ? "open" : "";
    [].forEach.call(to.attributes, function (a) {
      if (a.name !== keep && el.getAttribute(a.name) !== a.value) el.setAttribute(a.name, a.value);
    });
    Array.from(el.attributes).forEach(function (a) {
      if (a.name !== keep && !to.hasAttribute(a.name)) el.removeAttribute(a.name);
    });
    if (keep && !el.amxMine && to.hasAttribute("open")) el.open = el.amxOpen = true;
    children(el, to);
  }

  // Children with an id are matched by id, the rest by position.
  function children(el, to) {
    var cur = el.firstChild;
    Array.from(to.childNodes).forEach(function (n) {
      var m = n.id ? document.getElementById(n.id) : cur;
      if (m && m.parentNode === el && m.nodeName === n.nodeName && (m.id || "") === (n.id || "")) {
        if (m === cur) cur = cur.nextSibling;
        else el.insertBefore(m, cur);
        morph(m, n);
      } else {
        el.insertBefore(n, cur);
        if (n.nodeType === 1) defaults(n);
      }
    });
    while (cur) {
      var next = cur.nextSibling;
      el.removeChild(cur);
      cur = next;
    }
  }

  // Announced once per change of a review's status: the verdict on a review page, each row on the index.
  function statuses() {
    var m = {};
    document.querySelectorAll("#main .vt, #main .rv-pr").forEach(function (e) {
      var sr = e.querySelector(".sr");
      if (sr) m[e.firstChild.nodeValue] = e.firstChild.nodeValue + ": " + sr.textContent.replace(/^, /, "");
      else m[""] = e.textContent;
    });
    return m;
  }

  function swap(html) {
    var main = document.getElementById("main"), focused = document.activeElement, before = statuses();
    var t = document.createElement("template");
    t.innerHTML = html;
    children(main, t.content);
    if (focused && focused !== document.activeElement && focused.isConnected) focused.focus({ preventScroll: true });
    var root = main.firstElementChild;
    if (root && root.hasAttribute("data-title")) document.title = root.getAttribute("data-title");
    var after = statuses(), said = document.getElementById("said");
    var changed = Object.keys(after).filter(function (k) { return k in before && before[k] !== after[k]; });
    if (changed.length) {
      var text = changed.map(function (k) { return after[k]; }).join(". ");
      if (said.textContent !== text) said.textContent = text;
    }
    times();
    deadlines();
  }

  // Polls instead of holding a connection open: browsers allow about six per host, for all tabs.
  // A 404 means the page can no longer change: it shows the server's "No review" and stops.
  var tag, timer, gone = false;
  var refresh = coalesce(function () {
    clearTimeout(timer);
    if (gone || document.hidden) return;
    var live = document.getElementById("live");
    return fetch(location.pathname + location.search, { headers: { "X-Amx-Partial": "1", "If-None-Match": tag }, cache: "no-store" })
      .then(function (r) {
        gone = r.status === 404;
        if (r.status !== 304 && !r.ok && !gone) throw new Error(r.statusText);
        tag = r.headers.get("ETag") || "";
        return r.status === 304 ? null : r.text();
      })
      .then(function (html) {
        if (html !== null) swap(html);
        return true;
      }, function () { return false; })
      .then(function (ok) {
        var text = ok ? "" : "Reconnecting…";
        if (live.textContent !== text) live.textContent = text;
        if (!gone) timer = setTimeout(refresh, ok ? 3000 : 10000);
      });
  });

  document.addEventListener("DOMContentLoaded", function () {
    var main = document.getElementById("main");
    tag = main.getAttribute("data-etag") || "";
    defaults(main);
    times();
    deadlines();
    setInterval(times, 30000);
    setInterval(deadlines, 1000);
    document.addEventListener("visibilitychange", refresh);
    refresh();
  });
})();
