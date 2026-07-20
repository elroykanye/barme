/* Reads the latest barme release from the GitHub API and updates the page.
   Static HTML values are the fallback if the API is unreachable or rate-limited. */
(function () {
  var REPO = "elroykanye/barme";
  var CACHE_KEY = "barme-latest-release";
  var TTL = 30 * 60 * 1000; // 30 minutes

  function apply(tag, url) {
    if (tag) {
      var v = String(tag).replace(/^v/, "");
      var els = document.querySelectorAll(".barme-version");
      for (var i = 0; i < els.length; i++) els[i].textContent = v;
    }
    if (url) {
      var links = document.querySelectorAll("[data-release-link]");
      for (var j = 0; j < links.length; j++) links[j].setAttribute("href", url);
    }
  }

  function cached() {
    try {
      var c = JSON.parse(localStorage.getItem(CACHE_KEY));
      if (c && c.tag) return c;
    } catch (e) {}
    return null;
  }

  var c = cached();
  if (c) apply(c.tag, c.url); // paint cached value immediately, no flash

  // Only hit the API when the cache is missing or stale (keeps us well under the rate limit).
  if (c && Date.now() - c.t < TTL) return;

  fetch("https://api.github.com/repos/" + REPO + "/releases/latest", {
    headers: { Accept: "application/vnd.github+json" }
  })
    .then(function (r) { if (!r.ok) throw new Error("http " + r.status); return r.json(); })
    .then(function (d) {
      if (!d || !d.tag_name) return;
      apply(d.tag_name, d.html_url);
      try {
        localStorage.setItem(CACHE_KEY, JSON.stringify({ tag: d.tag_name, url: d.html_url, t: Date.now() }));
      } catch (e) {}
    })
    .catch(function () { /* keep static fallback */ });
})();
