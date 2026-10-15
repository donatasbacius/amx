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

  function times() {
    document.querySelectorAll("time[datetime]").forEach(function (t) {
      var ms = Date.parse(t.getAttribute("datetime"));
      if (!isNaN(ms)) {
        t.textContent = relative(ms);
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
      if (at) d.textContent = dur(at - Date.now());
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

  var toggled = {};
  document.addEventListener("toggle", function (e) {
    if (e.target.id) toggled[e.target.id] = e.target.open;
  }, true);

  var last = null;
  function swap(html) {
    if (html === last) return;
    var main = document.getElementById("main");
    var focused = document.activeElement && main.contains(document.activeElement) ? document.activeElement.id : "";
    main.innerHTML = html;
    last = html;
    Object.keys(toggled).forEach(function (id) {
      var d = document.getElementById(id);
      if (d && d.tagName === "DETAILS") d.open = toggled[id];
    });
    if (focused && document.getElementById(focused)) document.getElementById(focused).focus();
    times();
    deadlines();
  }

  // Polls instead of holding a connection open: browsers allow about six per host, for all tabs.
  var tag = "", timer;
  var refresh = coalesce(function () {
    clearTimeout(timer);
    if (document.hidden) return;
    var live = document.getElementById("live");
    return fetch(location.pathname + location.search, { headers: { "X-Amx-Partial": "1", "If-None-Match": tag }, cache: "no-store" })
      .then(function (r) {
        if (r.status !== 304 && !r.ok) throw new Error(r.statusText);
        tag = r.headers.get("ETag") || "";
        return r.status === 304 ? null : r.text();
      })
      .then(function (html) {
        if (html !== null) swap(html);
        return true;
      }, function () { return false; })
      .then(function (ok) {
        live.classList.toggle("on", ok);
        live.textContent = ok ? "Live" : "Reconnecting…";
        timer = setTimeout(refresh, ok ? 3000 : 10000);
      });
  });

  document.addEventListener("DOMContentLoaded", function () {
    times();
    deadlines();
    setInterval(times, 30000);
    setInterval(deadlines, 1000);
    document.addEventListener("visibilitychange", refresh);
    refresh();
  });
})();
