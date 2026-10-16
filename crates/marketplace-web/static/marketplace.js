/* Shared by the static marketplace and the local preview. */
function serverOrigin(value) {
  try {
    const url = new URL(value);
    return /^(https?:)$/.test(url.protocol) && !url.username && !url.password
      ? url.origin
      : null;
  } catch {
    return null;
  }
}

function installUrl(host, packageId, packageUrl) {
  const origin = serverOrigin(host);
  if (!origin)
    throw new Error("Enter an http:// or https:// Sandstorm server URL.");
  return `${origin}/install/${encodeURIComponent(packageId)}?url=${encodeURIComponent(packageUrl)}`;
}

function matchesApp(searchText, categories, query, tag) {
  return (
    (!tag || categories.includes(tag)) &&
    searchText.toLowerCase().includes(query.toLowerCase())
  );
}

function categoryFromPath(pathname) {
  const match = /^\/genre\/([^/]+)\/?$/.exec(pathname);
  if (!match) return "";
  try {
    return decodeURIComponent(match[1]);
  } catch {
    return "";
  }
}

if (typeof module !== "undefined")
  module.exports = { serverOrigin, installUrl, matchesApp, categoryFromPath };

if (typeof document !== "undefined")
  (() => {
    const params = new URLSearchParams(location.search);
    const experimental = params.get("experimental") === "true";
    const variant = document.getElementById("experimental-page");
    if (experimental && variant) {
      const content = variant.content.cloneNode(true);
      // This script is already running; inserted script tags would bind handlers twice.
      content.querySelectorAll("script").forEach((script) => script.remove());
      document.body.replaceChildren(content);
    }
    let host = serverOrigin(params.get("host"));
    const query = params.get("q") || "";
    const tag = categoryFromPath(location.pathname);
    const welcome = document.getElementById("welcome");
    if (welcome) welcome.hidden = Boolean(tag || query);
    const title = document.getElementById("catalog-title");
    if (title)
      title.textContent = query
        ? `Search results for “${query}”`
        : tag || "All Apps";
    const cards = document.querySelectorAll("article[data-search]");
    let visible = 0;
    cards.forEach((card) => {
      card.hidden = !matchesApp(
        card.dataset.search,
        JSON.parse(card.dataset.categories),
        query,
        tag,
      );
      if (!card.hidden) visible++;
    });
    const empty = document.getElementById("empty-results");
    if (empty) empty.hidden = visible !== 0;
    const search = document.querySelector('input[name="q"]');
    if (search) search.value = query;
    document.querySelectorAll("[data-tag]").forEach((link) => {
      const selected = link.dataset.tag === (tag || "All");
      link.classList.toggle("selected", selected);
      const url = new URL(link.href);
      if (query) url.searchParams.set("q", query);
      link.href = url.href;
    });
    document.querySelectorAll("form.search-form").forEach((form) => {
      for (const [name, value] of [
        ["host", host],
        ["experimental", experimental ? "true" : null],
      ]) {
        if (!value) continue;
        const field = document.createElement("input");
        field.type = "hidden";
        field.name = name;
        field.value = value;
        form.append(field);
      }
    });
    // Carry the user's server through category links, app pages, and back links.
    document.querySelectorAll("a[href]").forEach((link) => {
      const url = new URL(link.href);
      if (
        url.origin === location.origin &&
        (url.pathname === "/" ||
          url.pathname.startsWith("/app/") ||
          url.pathname.startsWith("/genre/"))
      ) {
        if (host) url.searchParams.set("host", host);
        if (experimental) url.searchParams.set("experimental", "true");
        link.href = url.href;
      }
    });
    document.querySelectorAll("[data-install]").forEach((link) => {
      const packageUrl = new URL(link.getAttribute("href"), location.href).href;
      if (host)
        link.href = installUrl(host, link.dataset.packageId, packageUrl);
      link.addEventListener("click", (event) => {
        if (host) return;
        event.preventDefault();
        const value = window.prompt(
          "Your Sandstorm server URL (for example https://sandstorm.example.com):",
        );
        if (value === null) return;
        const origin = serverOrigin(value);
        if (!origin) {
          window.alert("Enter an http:// or https:// Sandstorm server URL.");
          return;
        }
        location.href = installUrl(origin, link.dataset.packageId, packageUrl);
      });
    });

    (() => {
      const lightbox = document.getElementById("lightbox");
      const lightboxImage = document.getElementById("lightbox-image");
      const closeButton = document.getElementById("lightbox-close");
      if (!lightbox || !lightboxImage || !closeButton) return;

      const open = (src, alt) => {
        lightboxImage.src = src;
        lightboxImage.alt = alt || "";
        lightbox.classList.add("open");
        lightbox.setAttribute("aria-hidden", "false");
        document.body.style.overflow = "hidden";
      };

      const close = () => {
        lightbox.classList.remove("open");
        lightbox.setAttribute("aria-hidden", "true");
        lightboxImage.removeAttribute("src");
        document.body.style.overflow = "";
      };

      document.querySelectorAll("[data-lightbox-src]").forEach((button) => {
        button.addEventListener("click", () => {
          open(button.dataset.lightboxSrc, button.dataset.lightboxAlt);
        });
      });

      closeButton.addEventListener("click", close);
      lightbox.addEventListener("click", (event) => {
        if (event.target === lightbox) close();
      });
      document.addEventListener("keydown", (event) => {
        if (event.key === "Escape" && lightbox.classList.contains("open")) {
          close();
        }
      });
    })();
  })();
