"use strict";
function requestErrorMessage(status, body) {
  if (status >= 500) {
    return `The marketplace is temporarily unavailable (HTTP ${status}). Please try again shortly.`;
  }
  const message = body.trim();
  // Preserve short application errors, but never display a proxy's HTML page.
  if (message && message.length <= 500 && !/<[!/?a-z]/i.test(message)) return message;
  if (status === 401 || status === 403) return "You do not have permission to perform this action.";
  return `The request could not be completed (HTTP ${status}). Please try again.`;
}
function appIdFromPath(path) {
  return /^\/([0123456789acdefghjkmnpqrstuvwxyz]{52})\/?$/.exec(path)?.[1] || null;
}
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
// Display-only names from the comments in Sandstorm's bundled replacement table.
// Submitted metadata takes precedence; these labels never affect app identity.
const bundledSigningKeyNames = {
  vjvekechd398fn1t1kn1dgdnmaekqq9jkjv3zsgzymc4z913ref0: "My Cool Example App",
  vxe8awcxvtj6yu0vgjpm1tsaeu7x8v8tfp71tyvnm6ykkephu9q0: "Paperwork",
  nfqhx83vvzm80edpgkpax8mhqp176qj2vwg67rgq5e3kjc5r4cyh: "draw.io",
  m86q05rdvj14yvn78ghaxynqz7u2svw6rnttptxx49g1785cdv1h: "Wekan",
  aax9j672p6z8n7nyupzvj2nmumeqd4upa0f7mgu8gprwmy53x04h: "WordPress",
  p04q48xd2kgkystkrda5n77kdns6u569wnactgh904tf7s3vev70: "MediaWiki",
};
const auditEventLabels = {
  submitted: "Package submitted", imported: "Package imported",
  resubmitted: "Resubmitted — no state change",
  approved: "Approved",
  rejected: "Rejected / update requested", approvalRevoked: "Approval revoked",
  authorStateChanged: "Author request changed", rolledBack: "Rollback selected",
  rollbackCleared: "Rollback cleared", deprecated: "App deprecated",
  restored: "App restored", publicationChanged: "Public visibility changed",
  signingKeyRuleChanged: "Signing-key rule changed",
  signingKeyValidationChanged: "Signing-key validation changed",
};
function auditActorForDisplay(actor, action) {
  if (actor.signingKey) return { kind: "appSigningKey", signingKey: actor.signingKey };
  if (action === "submitted") return { kind: "submissionApi", displayName: "Submission API" };
  return actor;
}
function auditActorName(actor) {
  if (actor.signingKey) return "Signing key";
  if (actor.identityId) return actor.displayName?.trim() || `Sandstorm user ${actor.identityId}`;
  return actor.displayName || "User not identified";
}
function auditEventForDisplay(event) {
  const details = { ...event.details };
  if (details.decisionActor) details.decisionActor = auditActorForDisplay(details.decisionActor);
  return { ...event, actor: auditActorForDisplay(event.actor, event.action), details };
}
function auditEventLabel(event) {
  if (event.action === "authorStateChanged") {
    const requestedState = {
      publish: "Author requested publication",
      review: "Author requested review / embargo",
      ignore: "Author withdrew release",
    }[event.details.next?.requestState];
    if (requestedState) return requestedState;
  }
  return auditEventLabels[event.action] || event.action;
}
if (typeof module !== "undefined")
  module.exports = {
    requestErrorMessage,
    appIdFromPath,
    awaitingReview,
    groupApps,
    matches,
    pendingSubmissions,
    canRollback,
    lastPublished,
    publishedApps,
    releaseCount,
    auditActorName,
    auditActorForDisplay,
    auditEventForDisplay,
    auditEventLabel,
  };
if (typeof document !== "undefined") {
  // The script lives at the session root, including when loaded at an app URL.
  const appBase = new URL(".", document.currentScript.src);
  document.querySelector("base").href = appBase.href;
  const currentPath = () => `/${location.pathname.slice(appBase.pathname.length)}`;
  const currentTab = () => {
    const tab = /^(settings|notifications|replacements)\/?$/.exec(currentPath().slice(1))?.[1];
    return tab === "notifications" ? "settings" : tab || "apps";
  };
  const currentAppId = () => appIdFromPath(currentPath());
  function syncSandstormPath() {
    if (window.parent !== window) {
      window.parent.postMessage({ setPath: currentPath() + location.search + location.hash }, "*");
    }
  }
  const $ = (id) => document.getElementById(id);
  let items = [],
    page = 0,
    canApprove = false,
    serverBusy = false,
    working = false,
    refreshing = false,
    decision = null;
  const pageSize = 25;
  let auditView = null;
  let publicBase = null;
  let keybaseConnected = false;
  const keybaseProfiles = new Map();
  let notificationSending = false;
  let notificationDirty = false;
  let signingKeys = { revision: 0, rules: [] };
  let signingKeyRevision = 0;
  let signingKeyMode = "create";

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
    let response;
    try {
      response = await fetch(
        new URL(path, appBase),
        body === undefined ? {} : { method: "POST", body },
      );
    } catch {
      throw new Error("Could not connect to the marketplace. Please try again shortly.");
    }
    if (!response.ok) {
      const body = response.status < 500 ? await response.text() : "";
      throw new Error(requestErrorMessage(response.status, body));
    }
    return response;
  }
  const json = async (path) => {
    const response = await request(path);
    try {
      return await response.json();
    } catch {
      throw new Error("The marketplace returned an unexpected response. Please try again shortly.");
    }
  };
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
    $("signing-key-form").querySelectorAll("input,textarea,button").forEach(node => {
      node.disabled = working || serverBusy || (node.id === "signing-key-allowed" && !$("signing-key-revoke").checked);
    });
    $("import-submit").disabled = working || serverBusy;
    $("notifications-retry").disabled = working || serverBusy || notificationSending;
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
        "This release will return to pending review. If the author requests publication, it can appear in experimental mode when it is newer than the app’s approved public releases.",
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
  function keybaseSummary(profile) {
    const summary = element("span");
    const fields = [];
    for (const [field, label] of [
      ["keybaseHandle", "Keybase"], ["name", "Name"], ["githubHandles", "GitHub"],
      ["websites", "Websites"], ["twitterHandles", "Twitter"],
      ["hackernewsHandles", "Hacker News"], ["redditHandles", "Reddit"],
    ]) {
      const value = profile[field];
      const text = Array.isArray(value) ? value.join(", ") : value;
      if (text) fields.push(`${label}: ${text}`);
    }
    summary.textContent = fields.join(", ");
    if (profile.picture) {
      try {
        const url = new URL(profile.picture);
        if (["https:", "http:"].includes(url.protocol)) {
          const link = element("a", "Profile Image");
          link.href = url.href;
          link.target = "_blank";
          link.rel = "noopener noreferrer";
          summary.append(fields.length ? ", " : "", link);
        }
      } catch { /* Omit malformed image URLs. */ }
    }
    if (!summary.hasChildNodes()) summary.textContent = "No profile details available.";
    return summary;
  }
  function release(item, versions) {
    const node = element("section", undefined, "release");
    const heading = element("div", undefined, "release-heading");
    heading.append(element("h3", versionName(item)));
    node.append(heading, badges(item));
    node.append(element("p", `Package ID: ${item.app.packageId}`, "meta package-id"));
    node.append(
      element(
        "p",
        `Received: ${date(item.receivedAt)} · First published: ${item.publishedAt ? date(item.publishedAt * 1000) : "Never"}`,
        "meta release-dates",
      ),
    );
    if (item.fingerprint) {
      const fingerprint = element("p", `Verified PGP fingerprint: ${item.fingerprint}`, "meta pgp-fingerprint");
      const results = element("span", undefined, "keybase-results");
      results.setAttribute("aria-live", "polite");
      const showProfile = () => {
        const profile = keybaseProfiles.get(item.fingerprint);
        if (profile) results.replaceChildren(keybaseSummary(profile));
      };
      if (canApprove && keybaseConnected && !keybaseProfiles.has(item.fingerprint)) {
        const lookup = writeButton("Look up in Keybase", () => runAction(async () => {
          const profile = await (await request(`keybase/${item.fingerprint}`, "")).json();
          keybaseProfiles.set(item.fingerprint, profile);
          showProfile();
        }), "lookup-link");
        results.append(lookup);
      }
      showProfile();
      if (results.hasChildNodes()) fingerprint.append(" ", results);
      node.append(fingerprint);
    }
    if (item.validationError)
      node.append(
        element(
          "p",
          `Invalid package metadata: ${item.validationError}. Reject this release to restore publication.`,
          "reason",
        ),
      );
    if (item.reason) node.append(element("p", item.reason, "reason"));
    if (!item.validationError) {
      const preview = element("a", "Preview listing", "button secondary");
      preview.href = `preview/${encodeURIComponent(item.app.packageId)}`;
      node.append(preview);
    }
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
      heading.append(actions);
    }
    return node;
  }
  function appLink(item, label = item.app.name) {
    const link = element("a", label);
    link.href = new URL(`${encodeURIComponent(item.app.appId)}/`, appBase).href;
    link.dataset.adminNavigation = "";
    return link;
  }
  function renderApp(groups) {
    const detail = $("app-detail");
    detail.replaceChildren();
    const appId = currentAppId();
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
    const infoBody = element("div", undefined, "app-info-body");
    const sidebar = element("div", undefined, "app-info-sidebar");
    if (item.app.imageId && !item.validationError) {
      const icon = element("img", undefined, "app-icon");
      icon.alt = "";
      icon.width = 96;
      icon.height = 96;
      icon.hidden = true;
      sidebar.append(icon);
      json(`icon/${encodeURIComponent(item.app.packageId)}`).then(url => {
        if (url) { icon.src = url; icon.hidden = false; }
      }).catch(() => {});
    }
    if (item.publicPage && publicBase) {
      const link = element("a", "Public Page");
      link.href = new URL(`app/${encodeURIComponent(appId)}/`, publicBase).href;
      link.target = "_blank";
      link.rel = "noopener";
      sidebar.append(link);
    }
    info.append(infoBody);
    if (sidebar.childElementCount) info.append(sidebar);
    infoBody.append(
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
    const deprecation = element("section", undefined, "card deprecation-controls");
    deprecation.append(element("h2", "Deprecation"), element("p",
      item.app.deprecation
        ? "Deprecated. Hidden from marketplace indexes; direct listings and downloads remain available."
        : "Not deprecated. This app appears in marketplace indexes when it has a public release."));
    if (item.app.deprecation?.replacementAppId) {
      const replacement = element("a", "View replacement app");
      replacement.href = new URL(`${encodeURIComponent(item.app.deprecation.replacementAppId)}/`, appBase).href;
      replacement.dataset.adminNavigation = "";
      deprecation.append(replacement);
    }
    if (canApprove) {
      const form = element("form");
      const label = element("label", "Replacement app (optional)");
      const select = element("select");
      select.id = "replacement-app";
      select.dataset.write = "";
      label.htmlFor = select.id;
      const none = element("option", "No replacement");
      none.value = "";
      select.append(none);
      const candidates = groups.map(group => group.find(version => version.current)).filter(Boolean);
      for (const candidate of candidates.sort((a, b) => a.app.name.localeCompare(b.app.name))) {
        if (candidate.app.appId === appId) continue;
        const option = element("option", candidate.app.name);
        option.value = candidate.app.appId;
        select.append(option);
      }
      select.value = item.app.deprecation?.replacementAppId || "";
      const save = writeButton(item.app.deprecation ? "Update deprecation" : "Deprecate app", () => {});
      save.type = "submit";
      form.append(label, select, save);
      const update = value => runAction(async () => {
        try {
          await request(`deprecation/${encodeURIComponent(appId)}`, JSON.stringify(value));
          notice("message", value ? "App deprecated. Direct listings and downloads remain available." : "App restored to marketplace indexes.");
        } finally { await refresh(); }
      });
      form.onsubmit = event => {
        event.preventDefault();
        update({ replacementAppId: select.value || null });
      };
      if (item.app.deprecation) form.append(writeButton("Restore app to indexes", () => update(null)));
      deprecation.append(form);
    }
    if (item.pinnedPackageId) {
      infoBody.append(
        element(
          "p",
          current?.app.packageId === item.pinnedPackageId
            ? "Rollback selection active. New approvals do not change the public version."
            : "The rollback selection is ineligible. The latest eligible approved release is used.",
          "notice",
        ),
      );
      if (canApprove)
        infoBody.append(
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
    detail.append(info, activityHistory(appId), history, deprecation);
  }
  function renderAuditEvent(rawEvent) {
    const event = auditEventForDisplay(rawEvent);
    const row = element("tr");
    const description = element("td");
    const { actor, details } = event;
    const dateCell = element("td", undefined, "meta");
    const dateButton = button(date(event.timestamp), () => {
      $("audit-details-title").textContent = `Event #${event.number} details`;
      $("audit-details-json").textContent = JSON.stringify(event, null, 2);
      $("audit-details").showModal();
    }, "audit-date");
    dateButton.setAttribute("aria-haspopup", "dialog");
    dateButton.title = `View event #${event.number} details`;
    dateCell.append(dateButton);
    row.append(dateCell,
      element("td", auditEventLabel(event)), element("td", auditActorName(actor), "meta"),
      element("td", event.version == null ? "—" : String(event.version)), description);
    if (event.action === "publicationChanged") {
      const previous = details.previous;
      const next = details.next;
      const changes = [];
      if (previous.download !== next.download) changes.push(next.download ? "SPK download published" : "SPK download removed");
      if (previous.approvedListing !== next.approvedListing) changes.push(next.approvedListing ? "Listed in approved catalog" : "Removed from approved catalog");
      if (previous.experimentalListing !== next.experimentalListing) changes.push(next.experimentalListing ? "Listed in experimental catalog" : "Removed from experimental catalog");
      description.append(element("p", changes.join(" · ")));
      if (details.decisionActor) description.append(element("p", `Latest decision: #${details.latestDecision} by ${auditActorName(details.decisionActor)}`, "meta"));
    }
    if (event.action === "rejected") description.append(element("p", details.next.reason));
    if (details.note && event.action !== "publicationChanged") description.append(element("p", details.note));
    return row;
  }
  function activityHistory(appId) {
    if (auditView?.appId === appId) return auditView.section;
    const section = element("section", undefined, "card audit-history");
    const toolbar = element("div", undefined, "toolbar");
    toolbar.append(element("h2", "Activity history"));
    const tableWrap = element("div", undefined, "table-wrap");
    const table = element("table");
    table.setAttribute("aria-label", "Activity history");
    const head = element("thead");
    const headings = element("tr");
    for (const label of ["Date", "Event", "Actor", "Build", "Details"]) {
      const heading = element("th", label);
      heading.scope = "col";
      headings.append(heading);
    }
    head.append(headings);
    const list = element("tbody");
    table.append(head, list);
    tableWrap.append(table);
    const status = element("p", "Loading activity…", "meta");
    status.setAttribute("role", "status");
    let before;
    const more = button("Load older events", () => load());
    more.hidden = true;
    async function load() {
      more.disabled = true;
      reload.disabled = true;
      status.hidden = false;
      status.textContent = "Loading activity…";
      try {
        const result = await json(`audit/${encodeURIComponent(appId)}${before === undefined ? "" : `/${before}`}`);
        // Render before changing the table, so a failed page cannot leave partial rows.
        list.append(...result.events.map(renderAuditEvent));
        before = result.before;
        more.hidden = before == null;
        status.textContent = list.children.length ? "" : "No activity recorded yet.";
        status.hidden = list.children.length > 0;
        more.textContent = "Load older events";
      } catch (error) {
        status.textContent = `Could not load activity: ${error.message}`;
        more.hidden = false;
        more.textContent = "Retry loading activity";
      } finally { more.disabled = false; reload.disabled = false; }
    }
    const reload = button("Refresh", () => {
      before = undefined;
      list.replaceChildren();
      load();
    });
    toolbar.append(reload);
    section.append(toolbar, status, tableWrap, more);
    auditView = { appId, section };
    load();
    return section;
  }
  function renderAppRow(versions) {
    const current = versions.find((version) => version.current);
    const item = current || versions[0];
    const tr = element("tr");
    const app = element("td");
    const name = appLink(item);
    name.className = "app-name";
    app.append(name, element("div", item.app.shortDescription || "", "meta"));
    if (item.app.deprecation) app.append(element("span", "Deprecated", "badge"));
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
    actions.append(appLink(item, String(versions.length)));
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
      const title = element("h3");
      title.append(appLink(item));
      card.append(title);
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
    const groups = publishedApps(items);
    const showingApp = !!currentAppId();
    const tab = currentTab();
    const pending = pendingSubmissions(items);
    const pendingApps = new Set(pending.map(item => item.app.appId)).size;
    $("pending-count").textContent = pendingApps;
    $("pending-count").hidden = pendingApps === 0;
    $("pending-count").setAttribute("aria-label", `${pendingApps} apps awaiting review`);
    document.querySelectorAll("[data-tab]").forEach(link => {
      link.hidden = link.dataset.tab !== "apps" && !canApprove;
      if (link.dataset.tab === tab) link.setAttribute("aria-current", "page");
      else link.removeAttribute("aria-current");
    });
    $("settings").hidden = tab !== "settings" || !canApprove;
    $("signing-keys").hidden = tab !== "replacements" || !canApprove;
    $("catalog-heading").hidden = showingApp || tab !== "apps";
    $("catalog-view").hidden = showingApp || tab !== "apps";
    $("app-detail").hidden = !showingApp;
    if (showingApp) {
      renderApp(groups);
      updateControls();
      return;
    }
    $("app-detail").replaceChildren();
    const query = $("search").value;
    const rows = groups.filter((versions) =>
      versions.some((item) => matches(item, query)),
    );
    page = Math.min(page, Math.max(0, Math.ceil(rows.length / pageSize) - 1));
    $("result-count").textContent =
      `${rows.length} app${rows.length === 1 ? "" : "s"} · Most recently published first · Refreshed ${new Date().toLocaleTimeString()}`;
    renderTable(rows);
    renderReviewQueue(pending, groups);
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
      keybaseConnected = !!snapshot.keybaseConnected;
      $("keybase-connection-status").textContent = keybaseConnected
        ? "Connected — a Keybase connection is saved for this marketplace."
        : "Not connected.";
      $("keybase").textContent = keybaseConnected ? "Reconnect Keybase" : "Connect Keybase";
      serverBusy = snapshot.busy;
      $("import").hidden = !canApprove || serverBusy || items.length !== 0;
      if (canApprove && snapshot.signingKeys) {
        signingKeys = snapshot.signingKeys;
        renderSigningKeys();
      }
      const notifications = snapshot.notifications;
      if (canApprove && notifications) {
        if (!notificationDirty) $("notification-recipient").value = notifications.recipient;
        const status = notifications.recipient
          ? `${notifications.pending} pending · ${notifications.failed} failed${notifications.sending ? " · Sending…" : ""}${notifications.lastSentAt ? ` · Last sent ${date(notifications.lastSentAt)}` : ""}`
          : "Email notifications are disabled.";
        $("notification-status").textContent = status + (notifications.lastError ? ` · ${notifications.lastError}` : "");
        $("notifications-retry").hidden = !notifications.pending;
        notificationSending = notifications.sending;
      }
      render();
    } finally {
      refreshing = false;
      updateControls();
    }
  }
  async function runAction(task) {
    if (working) return;
    working = true;
    auditView = null;
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
  $("audit-details-close").onclick = () => $("audit-details").close();
  $("audit-details").addEventListener("close", () => {
    $("audit-details-json").textContent = "";
  });
  $("audit-details").addEventListener("click", (event) => {
    const bounds = $("audit-details").getBoundingClientRect();
    if (event.target === $("audit-details") &&
        (event.clientX < bounds.left || event.clientX > bounds.right ||
         event.clientY < bounds.top || event.clientY > bounds.bottom)) {
      $("audit-details").close();
    }
  });
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
            `${err.message} If the decision was saved but publication failed, retry the decision or restart the grain.`,
          );
        }
      } finally {
        $("decision-submit").disabled = false;
        $("decision-cancel").disabled = false;
      }
    });
  };
  function navigated() {
    auditView = null;
    render();
    syncSandstormPath();
    const heading = document.querySelector(currentAppId() ? "#app-detail h1" : currentTab() === "settings" ? "#settings-heading" : currentTab() === "replacements" ? "#signing-keys-heading" : "#catalog-heading h1");
    if (heading) {
      heading.tabIndex = -1;
      heading.focus();
    }
    window.scrollTo(0, 0);
  }
  document.addEventListener("click", (event) => {
    const link = event.target.closest("a[data-admin-navigation]");
    if (!link || event.defaultPrevented || event.button !== 0 || event.metaKey ||
        event.ctrlKey || event.shiftKey || event.altKey || link.target || link.hasAttribute("download")) return;
    event.preventDefault();
    if (link.href !== location.href) history.pushState(null, "", link.href);
    navigated();
  });
  window.addEventListener("popstate", navigated);
  syncSandstormPath();
  $("search").oninput = () => {
    page = 0;
    render();
  };
  $("notification-recipient").oninput = () => { notificationDirty = true; };
  function renderSigningKeys() {
    const apps = new Map(groupApps(items).map(versions => [versions[0].app.appId, versions[0]]));
    $("signing-key-rows").replaceChildren(...signingKeys.rules.map(rule => {
      let appId = rule.original;
      for (let i = 0; i < signingKeys.rules.length; i++) {
        const parent = signingKeys.rules.find(item => item.replacement === appId);
        if (!parent) break;
        appId = parent.original;
      }
      const app = apps.get(appId);
      const name = element("td");
      const row = element("tr");
      row.append(name, element("td", rule.original), element("td", rule.replacement || "None"),
        element("td", rule.allowedPackages === null ? "Retained" : `${rule.allowedPackages.length} allowed`));
      const actions = element("td");
      const historyButton = button(app?.app.name || bundledSigningKeyNames[appId] ||
        bundledSigningKeyNames[rule.original] || "Name unavailable", () => runAction(async () => {
        const history = await json(`audit/${appId}`);
        $("audit-details-title").textContent = "App history — latest 50 events";
        $("audit-details-json").textContent = JSON.stringify(history.events.map(auditEventForDisplay), null, 2);
        $("audit-details").showModal();
      }));
      historyButton.className = "app-history-link";
      name.append(historyButton);
      actions.append(writeButton("Edit", () => openSigningKey(rule)));
      row.append(actions);
      return row;
    }));
  }
  function openSigningKey(rule = null) {
    $("signing-key-form").reset();
    $("signing-key-title").textContent = rule ? "Edit Replacement" : "New Replacement";
    $("signing-key-original").value = rule?.original || "";
    $("signing-key-original").readOnly = !!rule;
    $("signing-key-replacement").value = rule?.replacement || "";
    $("signing-key-replacement").readOnly = !!rule?.replacement;
    $("signing-key-revoke").checked = rule?.allowedPackages != null;
    $("signing-key-allowed").value = (rule?.allowedPackages || []).join("\n");
    signingKeyRevision = signingKeys.revision;
    signingKeyMode = rule ? "edit" : "create";
    notice("signing-key-error", "");
    updateControls();
    $("signing-key-dialog").showModal();
  }
  $("signing-key-new").onclick = () => openSigningKey();
  $("signing-key-cancel").onclick = () => $("signing-key-dialog").close();
  $("signing-key-dialog").addEventListener("cancel", event => {
    if (working) event.preventDefault();
  });
  $("signing-key-revoke").onchange = updateControls;
  $("signing-key-form").onsubmit = async event => {
    event.preventDefault();
    await runAction(async () => {
      const rule = {
        original: $("signing-key-original").value.trim(),
        replacement: $("signing-key-replacement").value.trim() || null,
        allowedPackages: $("signing-key-revoke").checked
          ? $("signing-key-allowed").value.split(/\s+/).filter(Boolean) : null,
      };
      notice("signing-key-error", "");
      $("signing-key-status").textContent = "Revalidating affected packages and rebuilding the public site…";
      try {
        await request("signing-keys", JSON.stringify({ mode: signingKeyMode, revision: signingKeyRevision, rule }));
        $("signing-key-dialog").close();
        notice("message", "Signing-key rule saved and public site rebuilt.");
        await refresh();
      } catch (err) {
        if (!$("signing-key-dialog").open) throw err;
        // Reload settings without replacing the draft. A failed publication may
        // still have committed the rule; retry that exact rule as an edit.
        try {
          await refresh();
          const saved = signingKeys.rules.find(item => item.original === rule.original);
          if (signingKeys.revision > signingKeyRevision && saved &&
              saved.replacement === rule.replacement &&
              JSON.stringify(saved.allowedPackages) === JSON.stringify(rule.allowedPackages)) {
            signingKeyRevision = signingKeys.revision;
            signingKeyMode = "edit";
            $("signing-key-title").textContent = "Edit Replacement";
            $("signing-key-original").readOnly = true;
            $("signing-key-replacement").readOnly = !!saved.replacement;
          }
        } catch {
          // Preserve the save error if the refresh also fails.
        }
        notice("signing-key-error", err.message);
      } finally {
        $("signing-key-status").textContent = "";
      }
    });
  };
  let keybaseRequestPending = false;
  $("keybase").onclick = () => runAction(async () => {
    const descriptor = await (await request("keybase-pb-descriptor")).text();
    keybaseRequestPending = true;
    $("keybase-status").textContent = "Choose a Keybase API connection in Sandstorm.";
    window.parent.postMessage({ powerboxRequest: {
      rpcId: "keybase-request", query: [descriptor],
      saveLabel: { defaultText: "Keybase API" },
    } }, "*");
  });
  window.addEventListener("message", (event) => {
    if (event.source !== window.parent || event.data?.rpcId !== "keybase-request" ||
        !keybaseRequestPending || !canApprove) return;
    keybaseRequestPending = false;
    $("keybase-status").textContent = "";
    if (event.data.error) {
      notice("error", String(event.data.error));
      return;
    }
    if (!event.data.token) return;
    runAction(async () => {
      await request("keybase-pb-token", event.data.token);
      await refresh();
      $("keybase-status").textContent = "Keybase connected. You can look up authors in version history.";
    });
  });
  $("notifications-form").onsubmit = async (event) => {
    event.preventDefault();
    await runAction(async () => {
      await request("notifications", $("notification-recipient").value.trim());
      notificationDirty = false;
      notice("message", "Notification settings saved.");
      await refresh();
    });
  };
  $("notifications-retry").onclick = () => runAction(async () => {
    await request("notifications/retry", "");
    notice("message", "Pending email delivery requested.");
    await refresh();
  });
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
  refresh().catch((err) => notice("error", err.message));
  json("public-id")
    .then((info) => {
      const url = new URL(info.autoUrl);
      if (!["https:", "http:"].includes(url.protocol)) return;
      if (!url.pathname.endsWith("/")) url.pathname += "/";
      publicBase = url;
      $("public-url").href = url.href;
      $("public-url").hidden = false;
      $("publishing-preview").href = url.href;
      $("publishing-preview").textContent = url.href;
      $("publishing-status").textContent = info.publicId
        ? "Web publishing is enabled."
        : "The public preview is available, but custom-domain details are unavailable.";
      $("publishing-details").hidden = false;
      $("publishing-domain").disabled = !info.publicId;
      $("publishing-host").textContent = url.hostname;
      const updatePublishingRecords = () => {
        const domain = $("publishing-domain").value.trim().toLowerCase().replace(/\.$/, "");
        const valid = domain.length <= 253 && domain.includes(".") && domain.split(".").every(
          label => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label),
        );
        $("publishing-domain-error").hidden = !domain || valid;
        $("publishing-domain").setAttribute("aria-invalid", String(!!domain && !valid));
        $("publishing-dns").hidden = !valid;
        const apex = $("publishing-apex").checked;
        $("publishing-apex-warning").hidden = !apex;
        $("publishing-records").textContent = valid
          ? `${apex ? "" : `${domain}. IN CNAME ${url.hostname}.\n`}sandstorm-www.${domain}. IN TXT "${info.publicId}"`
          : "";
      };
      $("publishing-domain").oninput = () => {
        const domain = $("publishing-domain").value.trim().replace(/\.$/, "");
        // Two-label names are usually apex domains. Allow an explicit choice
        // for multi-label public suffixes and independently delegated zones.
        $("publishing-apex").checked = domain.split(".").length === 2;
        updatePublishingRecords();
      };
      $("publishing-apex").onchange = updatePublishingRecords;
      render();
    })
    .catch((err) => {
      $("publishing-status").textContent = "Unable to load publishing details. Reload the page to try again.";
      notice("error", err.message);
    });
  // Do not replace focused controls or interrupt a review decision during automatic refresh.
  setInterval(() => {
    if (
      !document.hidden &&
      !working &&
      !$("decision").open &&
      !$("signing-key-dialog").open &&
      !$("audit-details").open &&
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
