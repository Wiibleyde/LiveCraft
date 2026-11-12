// Fills in the download links from the newest GitHub release, pre-releases included.
// GitHub's /releases/latest skips pre-releases, so the list is read instead. Every link
// already points at the releases page, so nothing changes if this fails or never runs.
(function () {
  "use strict";

  var REPO = "Wiibleyde/LiveCraft";
  var API = "https://api.github.com/repos/" + REPO + "/releases?per_page=10";
  var DOWNLOADS = "https://github.com/" + REPO + "/releases/";

  var PATTERNS = {
    setup: /-setup\.exe$/i,
    portable: /-portable\.zip$/i,
    sums: /^SHA256SUMS(\.txt)?$/i
  };

  function newest(releases) {
    var best = null;
    releases.forEach(function (release) {
      if (release.draft) return;
      var when = Date.parse(release.published_at || release.created_at) || 0;
      if (!best || when > best.when) best = { release: release, when: when };
    });
    return best && best.release;
  }

  // Only links into this repository's releases are used.
  function safe(url) {
    return typeof url === "string" && url.indexOf(DOWNLOADS) === 0 ? url : null;
  }

  function megabytes(bytes) {
    return bytes > 0 ? Math.max(1, Math.round(bytes / 1048576)) + " MB" : "";
  }

  function apply(release) {
    var assets = {};
    Object.keys(PATTERNS).forEach(function (key) {
      assets[key] = (release.assets || []).filter(function (asset) {
        return PATTERNS[key].test(asset.name) && safe(asset.browser_download_url);
      })[0];
    });
    var page = safe(release.html_url);

    document.querySelectorAll("[data-asset]").forEach(function (link) {
      var key = link.getAttribute("data-asset");
      var url = key === "release" ? page : assets[key] && assets[key].browser_download_url;
      // An asset missing from the release still leads to that release's page.
      if (url || page) {
        link.href = url || page;
        link.hidden = false;
      }
    });

    document.querySelectorAll("[data-size]").forEach(function (span) {
      var asset = assets[span.getAttribute("data-size")];
      var size = asset && megabytes(asset.size);
      if (size) span.textContent = " · " + size;
    });

    var version = release.tag_name || release.name;
    if (!version) return;
    var date = new Date(release.published_at || release.created_at);
    var day = isNaN(date) ? "" : date.toLocaleDateString("en-GB", { day: "numeric", month: "long", year: "numeric" });
    var kind = release.prerelease ? "pre-release" : "release";

    document.querySelectorAll("[data-release-summary]").forEach(function (span) {
      span.textContent = "Latest: " + version + ", a " + kind + (day ? " from " + day : "") + ".";
    });

    document.querySelectorAll("[data-release-detail]").forEach(function (p) {
      p.textContent = "";
      var strong = document.createElement("strong");
      strong.textContent = version;
      p.appendChild(strong);
      p.appendChild(document.createTextNode(" · " + (release.prerelease ? "Pre-release" : "Release") + (day ? " · " + day : "")));
      p.hidden = false;
    });
  }

  function loadRelease() {
    if (!window.fetch) return;
    fetch(API, { headers: { Accept: "application/vnd.github+json" } })
      .then(function (response) {
        if (!response.ok) throw new Error("GitHub answered " + response.status);
        return response.json();
      })
      .then(function (releases) {
        var release = Array.isArray(releases) && newest(releases);
        if (release) apply(release);
      })
      .catch(function () {
        // Keep the links to the releases page.
      });
  }

  loadRelease();
})();
