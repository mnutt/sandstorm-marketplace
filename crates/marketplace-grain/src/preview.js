"use strict";
const install = document.getElementById("preview-install");
const dialog = document.getElementById("install-dialog");
const error = document.getElementById("install-error");
install.onclick = () => {
  error.hidden = true;
  dialog.showModal();
  document.getElementById("install-server").focus();
};
document.getElementById("install-cancel").onclick = () => dialog.close();
document.getElementById("install-form").onsubmit = async (event) => {
  event.preventDefault();
  const server = serverOrigin(
    document.getElementById("install-server").value.trim(),
  );
  if (!server) {
    error.textContent =
      "Enter an http:// or https:// Sandstorm server URL without credentials.";
    error.hidden = false;
    return;
  }
  // Open synchronously so the browser does not block the installer after fetch.
  const destination = window.open("about:blank", "_blank");
  if (destination) destination.opener = null;
  try {
    let url = `${server}/apps`;
    if (install.dataset.public === "true") {
      const response = await fetch("../public-id");
      if (!response.ok) throw new Error(await response.text());
      const info = await response.json();
      const base = new URL(info.autoUrl);
      if (!["http:", "https:"].includes(base.protocol))
        throw new Error("Public marketplace URL is unavailable.");
      const packageUrl = new URL(
        `packages/${encodeURIComponent(install.dataset.packageId)}`,
        `${base.href.replace(/\/$/, "")}/`,
      );
      url = installUrl(server, install.dataset.packageId, packageUrl.href);
    }
    if (!destination)
      throw new Error("Allow popups to open your Sandstorm server.");
    destination.location = url;
    dialog.close();
  } catch (err) {
    if (destination) destination.close();
    error.textContent = err.message;
    error.hidden = false;
  }
};
