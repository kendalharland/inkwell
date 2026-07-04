// Per-page right-rail "On this page" TOC. mdBook ships a left sidebar
// of chapters but no within-page navigation; this script scans the
// rendered main content for h2/h3 headings, wraps the existing article
// content in a #main-body div so <main> can act as a two-column grid,
// and appends a sticky aside in the right column with a scrollspy that
// highlights the section the reader is currently in.

(function () {
  // Force the light theme on every page load. The picker is hidden via
  // CSS, but anyone with a previously-saved dark choice in localStorage
  // would otherwise still get the dark palette.
  try {
    localStorage.setItem("mdbook-theme", "light");
  } catch (e) {
    /* private mode: nothing we can do, just continue */
  }
  function applyLight(el) {
    if (!el) return;
    ["navy", "rust", "coal", "ayu"].forEach(function (c) { el.classList.remove(c); });
    el.classList.add("light");
  }
  applyLight(document.documentElement);
  if (document.body) {
    applyLight(document.body);
  } else {
    document.addEventListener("DOMContentLoaded", function () { applyLight(document.body); });
  }

  function build() {
    var main = document.querySelector("main");
    if (!main) {
      return;
    }
    var headings = main.querySelectorAll("h2, h3");
    if (headings.length < 2) {
      // A page with only an h1 doesn't need a section TOC.
      return;
    }

    var existing = document.getElementById("page-toc");
    if (existing) {
      existing.parentNode.removeChild(existing);
    }
    var existingBody = document.getElementById("main-body");
    if (existingBody) {
      // Re-running on the same page (SPA-like nav doesn't happen in
      // mdBook, but be defensive): unwrap before re-wrapping.
      while (existingBody.firstChild) {
        main.insertBefore(existingBody.firstChild, existingBody);
      }
      existingBody.parentNode.removeChild(existingBody);
    }

    // Wrap the existing article content in a left-column container.
    var body = document.createElement("div");
    body.id = "main-body";
    while (main.firstChild) {
      body.appendChild(main.firstChild);
    }
    main.appendChild(body);

    var toc = document.createElement("aside");
    toc.id = "page-toc";
    var heading = document.createElement("h4");
    heading.textContent = "On this page";
    toc.appendChild(heading);

    var ul = document.createElement("ul");
    headings.forEach(function (h, i) {
      if (!h.id) {
        h.id = "section-" + i;
      }
      var li = document.createElement("li");
      li.className = "toc-" + h.tagName.toLowerCase();
      var a = document.createElement("a");
      a.href = "#" + h.id;
      a.textContent = h.textContent.replace(/\s*#\s*$/, "");
      li.appendChild(a);
      ul.appendChild(li);
    });
    toc.appendChild(ul);
    main.appendChild(toc);

    // Scrollspy: mark whichever heading is closest to the top of the
    // viewport as active. IntersectionObserver with a top-anchored
    // rootMargin gives the "current section" highlight modern docs
    // sites use.
    if ("IntersectionObserver" in window) {
      var links = toc.querySelectorAll("a");
      var byId = {};
      links.forEach(function (l) {
        byId[l.getAttribute("href").slice(1)] = l;
      });

      var visible = new Set();
      var observer = new IntersectionObserver(
        function (entries) {
          entries.forEach(function (e) {
            if (e.isIntersecting) {
              visible.add(e.target.id);
            } else {
              visible.delete(e.target.id);
            }
          });
          // Highlight the first visible heading in document order.
          links.forEach(function (l) {
            l.classList.remove("active");
          });
          var first = null;
          headings.forEach(function (h) {
            if (!first && visible.has(h.id)) {
              first = h.id;
            }
          });
          if (first && byId[first]) {
            byId[first].classList.add("active");
          }
        },
        { rootMargin: "-80px 0% -60% 0%", threshold: 0 }
      );
      headings.forEach(function (h) {
        observer.observe(h);
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", build);
  } else {
    build();
  }
})();

// Landing-page screenshot rotator: crossfade through every <img> child
// of .screenshot-rotator every ROTATE_MS. Assumes the first <img>
// starts with the `active` class (matching the markup in index.md).
// No-op when the container isn't on the current page or contains only
// one image.
(function () {
  var ROTATE_MS = 4000;

  function startRotator() {
    var rotator = document.querySelector(".screenshot-rotator");
    if (!rotator) return;
    var imgs = rotator.querySelectorAll("img");
    if (imgs.length <= 1) return;

    var idx = 0;
    for (var i = 0; i < imgs.length; i++) {
      if (imgs[i].classList.contains("active")) { idx = i; break; }
    }

    setInterval(function () {
      imgs[idx].classList.remove("active");
      idx = (idx + 1) % imgs.length;
      imgs[idx].classList.add("active");
    }, ROTATE_MS);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", startRotator);
  } else {
    startRotator();
  }
})();
