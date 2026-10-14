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

  var pending = false;
  function refresh() {
    if (pending) return;
    pending = true;
    fetch(location.pathname + location.search, { headers: { "X-Amx-Partial": "1" }, cache: "no-store" })
      .then(function (r) { return r.ok ? r.text() : null; })
      .then(function (html) {
        if (html !== null) {
          document.getElementById("main").innerHTML = html;
          times();
          deadlines();
        }
      })
      .catch(function () {})
      .then(function () { pending = false; });
  }

  function live() {
    var dot = document.getElementById("live");
    if (!window.EventSource || !dot) return;
    var es = new EventSource("/api/events");
    es.addEventListener("open", function () { dot.classList.add("on"); });
    es.addEventListener("error", function () { dot.classList.remove("on"); });
    es.addEventListener("change", refresh);
  }

  document.addEventListener("DOMContentLoaded", function () {
    times();
    deadlines();
    setInterval(times, 30000);
    setInterval(deadlines, 1000);
    live();
  });
})();
