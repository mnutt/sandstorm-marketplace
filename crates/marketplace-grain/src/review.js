"use strict";
function awaitingReview(item) {
  return item.review === "pending" && item.requestState !== "ignore";
}
function groupApps(items) {
  const groups = new Map();
  for (const item of items) {
    if (!groups.has(item.app.appId)) groups.set(item.app.appId, []);
    groups.get(item.app.appId).push(item);
  }
  return [...groups.values()]
    .map((versions) =>
      versions.sort((a, b) => b.app.versionNumber - a.app.versionNumber),
    )
    .sort((a, b) => a[0].app.name.localeCompare(b[0].app.name));
}
function lastPublished(versions) {
  return Math.max(0, ...versions.map((item) => item.publishedAt || 0));
}
function publishedApps(items) {
  return groupApps(items).sort(
    (a, b) =>
      lastPublished(b) - lastPublished(a) ||
      a[0].app.name.localeCompare(b[0].app.name),
  );
}
function matches(item, query) {
  return [item.app.name, item.app.appId, item.app.packageId, item.contactEmail]
    .join(" ")
    .toLowerCase()
    .includes(query.trim().toLowerCase());
}
function pendingSubmissions(items) {
  return items
    .filter(awaitingReview)
    .sort(
      (a, b) =>
        (b.receivedAt || "").localeCompare(a.receivedAt || "") ||
        b.app.versionNumber - a.app.versionNumber ||
        a.app.name.localeCompare(b.app.name),
    );
}
function testInstallUrl(server, publicBase, packageId) {
  const host = new URL(server);
  const base = new URL(publicBase);
  if (
    ![host, base].every(
      (url) =>
        ["http:", "https:"].includes(url.protocol) &&
        !url.username &&
        !url.password,
    )
  ) {
    throw new Error(
      "Enter an http:// or https:// Sandstorm server URL without credentials.",
    );
  }
  const packageUrl = new URL(
    `packages/${encodeURIComponent(packageId)}`,
    `${base.href.replace(/\/$/, "")}/`,
  );
  return `${host.origin}/install/${encodeURIComponent(packageId)}?url=${encodeURIComponent(packageUrl.href)}`;
}
function releaseCount(count) {
  return `${count} release${count === 1 ? "" : "s"}`;
}
function canRollback(item, versions) {
  const current = versions.find((version) => version.current);
  return (
    item.review === "approved" &&
    item.requestState === "publish" &&
    !!current &&
    item.app.versionNumber < current.app.versionNumber
  );
}
if (typeof module !== "undefined")
  module.exports = {
    awaitingReview,
    groupApps,
    matches,
    pendingSubmissions,
    canRollback,
    lastPublished,
    publishedApps,
    releaseCount,
    testInstallUrl,
  };
if (typeof document !== "undefined") {
  const $ = (id) => document.getElementById(id);
  let items = [],
    page = 0,
    canApprove = false,
    serverBusy = false,
    working = false,
    refreshing = false,
    decision = null;
  const pageSize = 25;
  let publicBase = null,
    installing = null;

  function element(tag, text, className) {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  }
  function notice(id, text) {
    $(id).textContent = text;
    $(id).hidden = !text;
  }
  async function request(path, body) {
    const response = await fetch(
      path,
      body === undefined ? {} : { method: "POST", body },
    );
    if (!response.ok) throw new Error(await response.text());
    return response;
  }
  const json = async (path) => (await request(path)).json();
  const versionName = (item) =>
    `${item.app.version || "Unlabeled"} (build ${item.app.versionNumber})`;
  const date = (value) =>
    value ? new Date(value).toLocaleString() : "Not recorded";
  function button(label, callback, className) {
    const node = element("button", label, className);
    node.type = "button";
    node.onclick = callback;
    return node;
  }
  function writeButton(label, callback, className) {
    const node = button(label, callback, className);
    node.dataset.write = "";
    node.disabled = working || serverBusy;
    return node;
  }
  function updateControls() {
    document.querySelectorAll("[data-write]").forEach((node) => {
      node.hidden = !canApprove;
      node.disabled = working || serverBusy;
    });
    $("import-submit").disabled = working || serverBusy;
    notice(
      "access-note",
      !canApprove
        ? "Read-only reviewer access. An approver can change review decisions and public versions."
        : serverBusy
          ? "A marketplace operation is in progress. Review actions will be available when it finishes."
          : "",
    );
  }
  function badges(item) {
    const node = element("div");
    if (item.current)
      node.append(element("span", "Current public version", "badge current"));
    if (item.pinnedPackageId === item.app.packageId)
      node.append(element("span", "Rollback selection", "badge"));
    node.append(
      element(
        "span",
        {
          pending: "Pending review",
          approved: "Approved",
          needsUpdate: "Rejected / needs update",
        }[item.review],
        `badge ${item.review}`,
      ),
    );
    node.append(
      element(
        "span",
        {
          publish: "Author requests publication",
          review: "Embargoed · review only",
          ignore: "Not requested / withdrawn",
        }[item.requestState],
        "badge",
      ),
    );
    return node;
  }
  function openDecision(item, action) {
    decision = { item, action };
    const labels = {
      approve: "Approve release",
      reject: "Reject / request update",
      unapprove: "Return to review",
      rollback: "Roll back public listing",
      latest: "Use latest approved release",
    };
    const explanations = {
      approve:
        item.requestState === "publish"
          ? "This release will be eligible for the public catalog. An existing rollback selection stays in effect until you choose “Use latest approved”."
          : "This approves the package. It will stay private until the author requests publication.",
      reject:
        "This release will be removed from public listings and downloads. The author can retrieve your reason through submission status; no email is sent.",
      unapprove:
        "This release will return to pending review. If the author requests publication, it will be available in the experimental catalog.",
      rollback:
        "List this older approved release instead of the current public version. Other approved packages stay downloadable. Installed grains are not downgraded. The selection persists until “Use latest approved”, unless the selected release becomes ineligible.",
      latest:
        "Clear the rollback selection. The highest approved version that the author requests to publish will be listed again.",
    };
    $("decision-title").textContent = `${labels[action]}: ${item.app.name}`;
    $("decision-description").textContent =
      `${versionName(item)}. ${explanations[action]}`;
    $("reason-label").hidden = action !== "reject";
    $("decision-reason").required = action === "reject";
    $("decision-reason").value = "";
    $("decision-submit").textContent = labels[action];
    notice("decision-error", "");
    $("decision").showModal();
    (action === "reject" ? $("decision-reason") : $("decision-cancel")).focus();
  }
  function release(item, versions) {
    const node = element("section", undefined, "release");
    node.append(element("h3", versionName(item)), badges(item));
    node.append(
      element(
        "p",
        `Received: ${date(item.receivedAt)} · First published: ${item.publishedAt ? date(item.publishedAt * 1000) : "Never"}`,
        "meta",
      ),
    );
    if (item.validationError)
      node.append(
        element(
          "p",
          `Invalid package metadata: ${item.validationError}. Reject this release to restore publication.`,
          "reason",
        ),
      );
    if (item.reason) node.append(element("p", item.reason, "reason"));
    const details = element("details");
    details.dataset.key = `package-${item.app.packageId}`;
    details.append(
      element("summary", "Package details & description"),
      element("p", `Package ID: ${item.app.packageId}`, "meta"),
      element(
        "p",
        `Contact: ${item.contactEmail || "Not provided"} · License: ${item.details.license || "Not provided"}`,
        "meta",
      ),
      element(
        "pre",
        item.details.description || "No description.",
        "description",
      ),
    );
    if (item.fingerprint) {
      details.append(
        element("p", `Verified PGP fingerprint: ${item.fingerprint}`, "meta"),
      );
      if (canApprove)
        details.append(
          writeButton("Look up author on Keybase", () =>
            runAction(async () => {
              const profile = await (
                await request(`keybase/${item.fingerprint}`, "")
              ).json();
              details.append(
                element("pre", JSON.stringify(profile, null, 2), "description"),
              );
            }),
          ),
        );
    }
    node.append(details);
    const testing = element("div", undefined, "actions");
    const download = element("a", "Download SPK", "button secondary");
    download.href = `download/${encodeURIComponent(item.app.packageId)}`;
    download.download = `${item.app.packageId}.spk`;
    testing.append(download);
    if (item.publicDownload && publicBase)
      testing.append(
        button("Install for testing", () => {
          installing = item;
          $("install-description").textContent =
            `Install ${item.app.name} · ${versionName(item)} on your Sandstorm server, then create a test grain. This does not approve the release.`;
          notice("install-error", "");
          $("install-dialog").showModal();
          $("install-server").focus();
        }),
      );
    node.append(testing);
    if (!item.publicDownload)
      node.append(
        element(
          "p",
          "To test this private release, download the SPK and upload it to your Sandstorm server.",
          "meta",
        ),
      );
    if (canApprove) {
      const actions = element("div", undefined, "actions");
      if (item.review !== "approved" && !item.validationError)
        actions.append(
          writeButton(
            "Approve",
            () => openDecision(item, "approve"),
            "primary",
          ),
        );
      if (item.review !== "needsUpdate")
        actions.append(
          writeButton(
            "Reject / request update",
            () => openDecision(item, "reject"),
            "danger",
          ),
        );
      if (item.review !== "pending")
        actions.append(
          writeButton("Return to review", () =>
            openDecision(item, "unapprove"),
          ),
        );
      if (canRollback(item, versions))
        actions.append(
          writeButton("Roll back to this version", () =>
            openDecision(item, "rollback"),
          ),
        );
      node.append(actions);
    }
    return node;
  }
  function appLink(item, label = item.app.name) {
    const link = element("a", label);
    link.href = `#app/${encodeURIComponent(item.app.appId)}`;
    return link;
  }
  function renderApp(groups) {
    const detail = $("app-detail");
    detail.replaceChildren();
    const back = element("a", "← All apps", "back");
    back.href = "#";
    detail.append(back);
    let appId;
    try {
      appId = decodeURIComponent(location.hash.slice(5));
    } catch {
      appId = "";
    }
    const versions = groups.find((group) => group[0].app.appId === appId);
    if (!versions) {
      detail.append(
        element("h1", "App not found", "catalog-title"),
        element("p", "This app is not in the marketplace."),
      );
      return;
    }
    const current = versions.find((version) => version.current);
    const item = current || versions[0];
    detail.append(element("h1", item.app.name, "catalog-title"));
    const info = element("section", undefined, "card app-info");
    info.append(
      element("h2", "App information"),
      element("p", item.app.shortDescription || ""),
      element(
        "p",
        current
          ? `Public version: ${versionName(current)}`
          : "No approved public version.",
      ),
      element("p", `App ID: ${appId}`, "meta"),
      element(
        "p",
        `Contact: ${item.contactEmail || "Not provided"} · License: ${item.details.license || "Not provided"}`,
        "meta",
      ),
      element(
        "pre",
        item.details.description || "No description.",
        "description",
      ),
    );
    if (item.pinnedPackageId) {
      info.append(
        element(
          "p",
          current?.app.packageId === item.pinnedPackageId
            ? "Rollback selection active. New approvals do not change the public version."
            : "The rollback selection is ineligible. The latest eligible approved release is used.",
          "notice",
        ),
      );
      if (canApprove)
        info.append(
          writeButton("Use latest approved", () =>
            openDecision(item, "latest"),
          ),
        );
    }
    const history = element("section", undefined, "app-history");
    history.append(
      element("h2", "Version history"),
      element(
        "p",
        `${releaseCount(versions.length)} · Newest build first`,
        "meta",
      ),
    );
    for (const entry of versions) history.append(release(entry, versions));
    detail.append(info, history);
  }
  function renderAppRow(versions) {
    const current = versions.find((version) => version.current);
    const item = current || versions[0];
    const tr = element("tr");
    const app = element("td");
    const name = appLink(item);
    name.className = "app-name";
    app.append(name, element("div", item.app.shortDescription || "", "meta"));
    const awaiting = versions.filter(awaitingReview).length;
    if (awaiting)
      app.append(
        element("span", `${awaiting} awaiting approval`, "badge pending"),
      );
    const version = element(
      "td",
      current ? versionName(current) : "Not published",
    );
    if (item.pinnedPackageId)
      version.append(
        element(
          "div",
          current?.app.packageId === item.pinnedPackageId
            ? "Rollback active"
            : "Rollback inactive",
          "meta",
        ),
      );
    const published = lastPublished(versions);
    const when = element(
      "td",
      published ? date(published * 1000) : "Never",
      "meta",
    );
    const actions = element("td");
    actions.append(appLink(item, releaseCount(versions.length)));
    tr.append(app, version, when, actions);
    return tr;
  }
  function renderTable(rows) {
    const queue = $("queue");
    queue.replaceChildren();
    queue.setAttribute("aria-busy", "false");
    if (!rows.length)
      queue.append(
        element(
          "div",
          items.length
            ? "No apps match your search."
            : "No apps yet. Import a marketplace below or share a submitter API token with an author.",
          "card empty",
        ),
      );
    else {
      const table = element("table");
      table.setAttribute(
        "aria-label",
        "Existing apps, ordered by last publication",
      );
      const thead = element("thead");
      const headings = element("tr");
      for (const title of [
        "App",
        "Public version",
        "Last published",
        "Versions",
      ]) {
        const th = element("th", title);
        th.scope = "col";
        if (title === "Last published")
          th.setAttribute("aria-sort", "descending");
        headings.append(th);
      }
      thead.append(headings);
      table.append(thead);
      const tbody = element("tbody");
      for (const versions of rows.slice(
        page * pageSize,
        (page + 1) * pageSize,
      )) {
        tbody.append(renderAppRow(versions));
      }
      table.append(tbody);
      queue.append(table);
    }
  }
  function renderReviewQueue(pending, groups) {
    const rail = $("review-queue");
    rail.replaceChildren();
    const appCount = new Set(pending.map((item) => item.app.appId)).size;
    const groupsById = new Map(
      groups.map((versions) => [versions[0].app.appId, versions]),
    );
    $("review-count").textContent =
      `${pending.length} pending release${pending.length === 1 ? "" : "s"} across ${appCount} app${appCount === 1 ? "" : "s"}`;
    if (!pending.length)
      rail.append(
        element(
          "p",
          "All caught up. New submissions awaiting review will appear here.",
          "empty",
        ),
      );
    for (const item of pending) {
      const versions = groupsById.get(item.app.appId);
      const card = element("article", undefined, "card");
      card.append(element("h3", item.app.name));
      const current = versions.find((version) => version.current);
      card.append(
        element(
          "p",
          current
            ? `Update to public version ${current.app.version}`
            : "No current public version",
          "meta",
        ),
      );
      card.append(release(item, versions));
      card.append(appLink(item, "View app history"));
      rail.append(card);
    }
  }
  function renderPagination(total) {
    const pagination = $("pagination");
    pagination.replaceChildren();
    if (total > pageSize) {
      const previous = button("Previous", () => {
        page--;
        render();
      });
      previous.disabled = page === 0;
      const next = button("Next", () => {
        page++;
        render();
      });
      next.disabled = (page + 1) * pageSize >= total;
      pagination.append(
        previous,
        element("span", `Page ${page + 1} of ${Math.ceil(total / pageSize)}`),
        next,
      );
    }
  }
  function render() {
    const open = new Set(
      [...document.querySelectorAll("details[open][data-key]")].map(
        (node) => node.dataset.key,
      ),
    );
    const groups = publishedApps(items);
    const showingApp = location.hash.startsWith("#app/");
    $("catalog-heading").hidden = showingApp;
    $("catalog-view").hidden = showingApp;
    $("app-detail").hidden = !showingApp;
    if (showingApp) {
      renderApp(groups);
      document
        .querySelectorAll("#app-detail details[data-key]")
        .forEach((node) => {
          node.open = open.has(node.dataset.key);
        });
      updateControls();
      return;
    }
    $("app-detail").replaceChildren();
    const pending = pendingSubmissions(items);
    const query = $("search").value;
    const rows = groups.filter((versions) =>
      versions.some((item) => matches(item, query)),
    );
    page = Math.min(page, Math.max(0, Math.ceil(rows.length / pageSize) - 1));
    $("result-count").textContent =
      `${rows.length} app${rows.length === 1 ? "" : "s"} · Most recently published first · Refreshed ${new Date().toLocaleTimeString()}`;
    renderTable(rows);
    renderReviewQueue(pending, groups);
    document.querySelectorAll("details[data-key]").forEach((node) => {
      node.open = open.has(node.dataset.key);
    });
    renderPagination(rows.length);
    updateControls();
  }
  async function refresh() {
    if (refreshing) return;
    refreshing = true;
    updateControls();
    try {
      const snapshot = await json("admin");
      items = snapshot.releases;
      canApprove = snapshot.canApprove;
      serverBusy = snapshot.busy;
      $("import").hidden = !canApprove || serverBusy || items.length !== 0;
      render();
    } finally {
      refreshing = false;
      updateControls();
    }
  }
  async function runAction(task) {
    if (working) return;
    working = true;
    updateControls();
    notice("error", "");
    notice("message", "");
    try {
      await task();
    } catch (err) {
      notice("error", err.message);
    } finally {
      working = false;
      updateControls();
    }
  }
  $("install-cancel").onclick = () => $("install-dialog").close();
  $("install-form").onsubmit = (event) => {
    event.preventDefault();
    try {
      const url = testInstallUrl(
        $("install-server").value.trim(),
        publicBase,
        installing.app.packageId,
      );
      window.open(url, "_blank", "noopener,noreferrer");
      $("install-dialog").close();
    } catch (err) {
      notice("install-error", err.message);
    }
  };
  $("decision-cancel").onclick = () => $("decision").close();
  $("decision").addEventListener("cancel", (event) => {
    if (working) event.preventDefault();
  });
  $("decision-form").onsubmit = async (event) => {
    event.preventDefault();
    if (working || !decision) return;
    const { item, action } = decision;
    const reason = $("decision-reason").value.trim();
    if (action === "reject" && !reason) {
      notice("decision-error", "Enter a reason for the author.");
      return;
    }
    await runAction(async () => {
      $("decision-submit").disabled = true;
      $("decision-cancel").disabled = true;
      notice("decision-error", "");
      let saved = false;
      try {
        await request(
          `${action}/${item.app.packageId}`,
          action === "reject" ? reason : "",
        );
        saved = true;
        $("decision").close();
        notice("message", `Updated ${item.app.name} · ${versionName(item)}.`);
        await refresh();
      } catch (err) {
        if (saved) {
          notice("message", "");
          notice(
            "error",
            `The decision was saved, but the view could not refresh: ${err.message}. It will retry automatically.`,
          );
        } else {
          notice(
            "decision-error",
            `${err.message} If the decision was saved but publication failed, rebuild the public site.`,
          );
        }
      } finally {
        $("decision-submit").disabled = false;
        $("decision-cancel").disabled = false;
      }
    });
  };
  window.addEventListener("hashchange", () => {
    render();
    const heading = document.querySelector(
      location.hash.startsWith("#app/")
        ? "#app-detail h1"
        : "#catalog-heading h1",
    );
    if (heading) {
      heading.tabIndex = -1;
      heading.focus();
    }
    window.scrollTo(0, 0);
  });
  $("search").oninput = () => {
    page = 0;
    render();
  };
  $("import-form").onsubmit = async (event) => {
    event.preventDefault();
    const file = $("import-file").files[0];
    if (!file) return;
    await runAction(async () => {
      $("import-progress").textContent = "Uploading and verifying packages…";
      try {
        await request("import", file);
        notice("message", "Marketplace imported and published.");
      } finally {
        $("import-progress").textContent = "";
        await refresh();
      }
    });
  };
  $("reindex").onclick = () =>
    runAction(async () => {
      await request("reindex", "");
      await refresh();
      notice("message", "Public site rebuilt.");
    });
  $("keybase").onclick = () =>
    runAction(async () => {
      const descriptor = await (await request("keybase-pb-descriptor")).text();
      window.parent.postMessage(
        {
          powerboxRequest: {
            rpcId: "keybase-request",
            query: [descriptor],
            saveLabel: { defaultText: "Keybase API" },
          },
        },
        "*",
      );
    });
  window.addEventListener("message", (event) => {
    if (
      event.source !== window.parent ||
      event.data?.rpcId !== "keybase-request" ||
      !event.data.token
    )
      return;
    runAction(async () => {
      await request("keybase-pb-token", event.data.token);
      notice("message", "Keybase connected.");
    });
  });
  refresh().catch((err) => notice("error", err.message));
  json("public-id")
    .then((info) => {
      const url = new URL(info.autoUrl);
      if (!["https:", "http:"].includes(url.protocol)) return;
      publicBase = url.href;
      $("public-url").href = url.href;
      $("public-url").hidden = false;
      render();
    })
    .catch((err) => notice("error", err.message));
  // Do not replace focused controls or interrupt a review decision during automatic refresh.
  setInterval(() => {
    if (
      !document.hidden &&
      !working &&
      !$("decision").open &&
      !$("install-dialog").open &&
      !document.activeElement?.matches(
        'input, textarea, select, [contenteditable="true"]',
      )
    ) {
      refresh().catch((err) =>
        notice("error", `Could not refresh: ${err.message}`),
      );
    }
  }, 30000);
}
