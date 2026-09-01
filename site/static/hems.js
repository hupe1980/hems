/* hems — theme toggle, copy buttons, mermaid.
   Everything here is progressive: the page is complete without it. */

(function () {
  "use strict";

  /* ── Theme ────────────────────────────────────────────────────────────── */

  var root = document.documentElement;
  var LABEL = { light: "☀", dark: "☾", system: "◐" };
  var ORDER = ["system", "light", "dark"];

  function apply(choice) {
    if (choice === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", choice);

    // The syntax stylesheets are picked by media query, so an explicit choice
    // has to move the query rather than a class.
    var light = document.getElementById("syntax-light");
    var dark = document.getElementById("syntax-dark");
    if (light && dark) {
      if (choice === "system") {
        light.media = "(prefers-color-scheme: light)";
        dark.media = "(prefers-color-scheme: dark)";
      } else {
        light.media = choice === "light" ? "all" : "not all";
        dark.media = choice === "dark" ? "all" : "not all";
      }
    }
    var btn = document.getElementById("theme-toggle");
    if (btn) {
      btn.textContent = LABEL[choice];
      btn.setAttribute("aria-label", "Colour theme: " + choice);
      btn.title = "Colour theme: " + choice;
    }
    document.dispatchEvent(new CustomEvent("hems:theme", { detail: choice }));
  }

  function stored() {
    try { return localStorage.getItem("hems-theme") || "system"; } catch (e) { return "system"; }
  }

  var current = stored();
  apply(current);

  var toggle = document.getElementById("theme-toggle");
  if (toggle) {
    toggle.hidden = false;
    toggle.addEventListener("click", function () {
      current = ORDER[(ORDER.indexOf(current) + 1) % ORDER.length];
      try { localStorage.setItem("hems-theme", current); } catch (e) { /* private mode */ }
      apply(current);
    });
  }

  /* ── Wide tables scroll inside their own box ──────────────────────────── */

  document.querySelectorAll("article table").forEach(function (table) {
    if (table.parentElement.classList.contains("table-wrap")) return;
    var wrap = document.createElement("div");
    wrap.className = "table-wrap";
    table.parentNode.insertBefore(wrap, table);
    wrap.appendChild(table);
  });

  /* ── Copy buttons ─────────────────────────────────────────────────────── */

  if (navigator.clipboard) {
    document.querySelectorAll("pre:not(.mermaid)").forEach(function (pre) {
      var btn = document.createElement("button");
      btn.className = "copy-btn";
      btn.type = "button";
      btn.textContent = "Copy";
      btn.addEventListener("click", function () {
        // A console block is written with its prompt; what a reader wants on the
        // clipboard is the command, not the `$`.
        var text = pre.innerText.replace(/^\$ ?/gm, "");
        navigator.clipboard.writeText(text).then(function () {
          btn.textContent = "Copied";
          setTimeout(function () { btn.textContent = "Copy"; }, 1400);
        });
      });
      pre.appendChild(btn);
    });
  }

  /* ── Search ───────────────────────────────────────────────────────────── */

  // Progressive to the last: the box stays hidden until an index has actually
  // loaded and parsed, so a failure here is invisible rather than broken.
  var box = document.getElementById("search");
  if (box) {
    (function () {
      var input = document.getElementById("search-input");
      var list = document.getElementById("search-results");
      var index = null;

      function load(then) {
        if (index) return then();
        var lunr = document.createElement("script");
        lunr.src = base("elasticlunr.min.js");
        lunr.onload = function () {
          var data = document.createElement("script");
          data.src = base("search_index.en.js");
          data.onload = function () {
            try {
              index = window.elasticlunr.Index.load(window.searchIndex);
              then();
            } catch (e) { /* no search, and no broken box either */ }
          };
          document.head.appendChild(data);
        };
        document.head.appendChild(lunr);
      }

      function base(file) {
        var css = document.querySelector('link[href$="hems.css"]');
        return css ? css.getAttribute("href").replace(/hems\.css$/, file) : file;
      }

      function render(term) {
        list.innerHTML = "";
        if (!index || term.length < 2) return;
        var hits = index.search(term, {
          bool: "AND",
          expand: true,
          fields: { title: { boost: 3 }, description: { boost: 2 }, body: { boost: 1 } }
        }).slice(0, 8);
        if (!hits.length) {
          var none = document.createElement("li");
          none.className = "search-empty";
          none.textContent = "Nothing for “" + term + "”";
          list.appendChild(none);
          return;
        }
        hits.forEach(function (hit) {
          var doc = index.documentStore.getDoc(hit.ref);
          var li = document.createElement("li");
          var a = document.createElement("a");
          a.href = hit.ref;
          var t = document.createElement("strong");
          t.textContent = doc.title || hit.ref;
          var d = document.createElement("span");
          d.textContent = (doc.description || doc.body || "").slice(0, 110);
          a.appendChild(t);
          a.appendChild(d);
          li.appendChild(a);
          list.appendChild(li);
        });
      }

      input.addEventListener("input", function () {
        var term = input.value.trim();
        if (!term) { list.innerHTML = ""; return; }
        load(function () { render(term); });
      });
      input.addEventListener("keydown", function (e) {
        if (e.key === "Escape") { input.value = ""; list.innerHTML = ""; input.blur(); }
      });
      document.addEventListener("click", function (e) {
        if (!box.contains(e.target)) list.innerHTML = "";
      });

      // Only now is there something that works.
      box.hidden = false;
    })();
  }

  /* ── Mermaid ──────────────────────────────────────────────────────────── */

  var diagrams = document.querySelectorAll("pre.mermaid");
  if (!diagrams.length) return;

  function dark() {
    var choice = root.getAttribute("data-theme");
    if (choice === "dark") return true;
    if (choice === "light") return false;
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  }

  var sources = [];
  diagrams.forEach(function (el) { sources.push(el.textContent); });

  var script = document.createElement("script");
  script.type = "module";
  script.textContent = [
    "import mermaid from 'https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs';",
    "window.__hemsMermaid = mermaid;",
    "document.dispatchEvent(new Event('hems:mermaid-ready'));"
  ].join("\n");

  function render() {
    var mermaid = window.__hemsMermaid;
    if (!mermaid) return;
    var css = getComputedStyle(document.body);
    diagrams.forEach(function (el, i) {
      el.removeAttribute("data-processed");
      el.textContent = sources[i];
    });
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      theme: dark() ? "dark" : "neutral",
      fontFamily: css.getPropertyValue("--font-sans") || "sans-serif",
      themeVariables: {
        primaryColor: css.getPropertyValue("--bg-sunk").trim(),
        primaryTextColor: css.getPropertyValue("--fg").trim(),
        primaryBorderColor: css.getPropertyValue("--line").trim(),
        lineColor: css.getPropertyValue("--fg-subtle").trim(),
        secondaryColor: css.getPropertyValue("--accent-weak").trim(),
        tertiaryColor: css.getPropertyValue("--bg-soft").trim(),
        background: css.getPropertyValue("--bg-soft").trim()
      }
    });
    mermaid.run({ nodes: diagrams });
  }

  // If the renderer never arrives, show the graph source rather than nothing.
  var revealed = setTimeout(function () {
    diagrams.forEach(function (el) {
      if (!el.hasAttribute("data-processed")) el.classList.add("source-fallback");
    });
  }, 4000);

  document.addEventListener("hems:mermaid-ready", function () {
    clearTimeout(revealed);
    render();
  });
  document.addEventListener("hems:theme", function () { if (window.__hemsMermaid) render(); });
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function () {
    if (window.__hemsMermaid) render();
  });

  document.head.appendChild(script);
})();
