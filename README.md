# Sandstorm Marketplace

A Rust Sandstorm grain that receives signed SPK submissions, provides a private
review UI, and publishes a static marketplace through Sandstorm Web Publishing.

```text
spk publish --webkey … ── ApiSession ──┐
                                     ├─ verification + submission state
Private grain UI ── review/approval ──┘
                                              │
                                      rebuild private catalog
                                              │
                                      generate static website
                                              │
                                  atomically replace /var/www
                                              │
                                     Sandstorm Web Publishing
```

Public browsing does not need a running application process. Search and category
filters run in the browser. Installation links preserve Sandstorm's `?host=`
parameter and direct users back to their server with the public SPK URL.
Without a host parameter, Install asks for a Sandstorm server URL.

## Moving parts

- `crates/marketplace-grain`: Rust Cap'n Proto RPC server, SPK verification,
  submission protocol, persistent registry, and private review UI. Vendored
  protocol definitions are documented in [UPSTREAM.md](crates/marketplace-grain/UPSTREAM.md).
- `crates/marketplace-model`: shared catalog JSON types.
- `crates/marketplace-web`: static publisher library and standalone preview server.
- `crates/marketplace-sync`: command-line catalog mirroring and import-tarball generation.
- `xtask`: Rust package builder and compatibility-test runner.

The sync tool runs on your workstation and reads a public marketplace over
HTTPS. The grain runs inside Sandstorm and handles private administration,
imports, and author submissions over Cap'n Proto RPC. It uses the web crate as a
library to generate `/var/www`; Sandstorm serves that directory through Web
Publishing. The standalone web server is for local previews.

There are two separate artifacts: `build/marketplace.spk` installs this
marketplace application, while `build/marketplace-import.tar` carries catalog
indexes and other apps' signed SPKs to seed an empty marketplace grain. The
import tarball is uploaded through the grain's administration UI.

```text
Public marketplace ── marketplace-sync ── import.tar
                                              │
                                  administration UI upload
                                              │
                                  verify and import packages
                                              │
                               /var/packages (persistent state)
                                              │
                               /var/catalog (generated catalog)
                                              │
                               /var/www (generated public site)
```

## Build and package

Requires Linux, Rust/Cargo, the Cap'n Proto schema compiler (`capnp` on PATH, or
set `CAPNP`), and `ldd` for staging system runtime libraries. Application code,
SPK signature verification, compression, packaging, and test orchestration are
Rust. No Python, C++ application backend, Sandstorm SDK, `xz`, or GnuPG is needed
for normal builds or runtime. Browser interfaces use HTML and JavaScript.

```bash
cargo xtask package --contact-email you@example.com
# Equivalent optional Make wrapper:
make package CONTACT_EMAIL=you@example.com
```

The result is `build/marketplace.spk`. The Rust packager builds the grain, stages
its executable and shared system libraries, constructs the manifest, and signs
and compresses the SPK. A signing identity is created in
`.local/marketplace.keyring`, with its app ID in `.local/app-id`. Keep both files
for future updates; they are ignored by Git and excluded from the package.
Existing SDK-format signing keys are preserved. An incomplete or mismatched
identity causes packaging to fail instead of silently changing the app ID.

The default contact address is a placeholder for local development. Set a real
address before distributing the package. Increment `APP_VERSION` and update
`MARKETING_VERSION` in `xtask/src/package.rs` for releases.

## Use in Sandstorm

1. Install `build/marketplace.spk` and create a marketplace grain.
2. Open its administration UI. The public marketplace URL is displayed there;
   this also allocates the grain's Web Publishing address.
3. Use Sandstorm's Share access menu to create an API token with the **submitter**
   role. Authors publish using the resulting webkey:

   ```bash
   spk publish --webkey 'https://YOUR-API-HOST/#TOKEN' your-app.spk
   ```

4. Review and approve the submission in the grain. Approved packages whose
   authors requested publication appear in the main catalog and public website.

The **reviewer** role can inspect submissions; **approver** can approve, reject,
and rebuild the catalog. Submission access alone does not grant review access.
`spk publish --status`, `--embargo`, and `--remove` retain their existing meanings.
Pending packages requested for publication appear in `experimental/index.json`
and have public downloads; embargoed packages do not. Unapproval moves a release
back to experimental; rejection or author withdrawal removes it from public
output. Older still-published versions remain downloadable.

To use this marketplace for a Sandstorm installation, set its App Market URL
and App Index URL to the public site URL in the server's configuration.

```text
/                         marketplace listing
/app/<appId>/             static app detail page
/apps/index.json          approved app summaries
/apps/<appId>.json         app details
/experimental/index.json  pending publication requests
/images/<imageId>          icons and screenshots
/packages/<packageId>      signed SPK downloads
```

## Sync to a tarball, then upload

Run these commands from the repository root. First install the marketplace SPK
and create a grain as described above. Import requires the owner or an
**approver**, an empty grain with no submitted packages, and no other write
operation in progress. It is a one-time bootstrap, not an incremental sync into
an existing grain.

### 1. Generate the import tarball

```bash
cargo run --release -p marketplace-sync -- sync-remote \
  --base-url https://app-index.sandstorm.io \
  --output-dir data/app-index \
  --tarball build/marketplace-import.tar
```

The base URL and output directory shown are the defaults. Use `--base-url` to
copy another compatible public marketplace. This command fetches catalog JSON
and images into `data/app-index`, then downloads the SPKs referenced by the
approved and experimental indexes into an **uncompressed `.tar`** archive.
The archive contains the indexes and packages; app details and images are
regenerated from signed package manifests during import.

For a smaller trial, use a separate output directory and a disposable empty grain:

```bash
cargo run --release -p marketplace-sync -- sync-remote \
  --limit 10 \
  --output-dir data/app-index-test \
  --tarball build/marketplace-test.tar
```

`--limit` selects at most that many releases across both indexes: approved
releases first, then experimental releases with any remaining allowance, ordered
by app name within each index. Both saved indexes and the tarball contain only
the selected releases. Omit the limit for the full current catalog. A trial
import fills its grain too; create another empty grain for the full import.

Wait for the command to exit successfully and log **Import tarball ready to
upload**. The destination tarball is replaced only after generation succeeds;
a failed run can leave an older tarball at that path. Rerunning downloads the
selected packages again; it does not resume partial downloads. Reusing an output
directory can leave old detail/image files, but only releases in the newly saved
indexes are included in the archive.

### 2. Upload through the grain

1. Open the empty grain's administration page as owner or approver.
2. Under **Import an existing marketplace**, select
   `build/marketplace-import.tar` (or the test tarball).
3. Click **Import marketplace** and keep the page open while it displays
   **Uploading and verifying packages…**. This covers upload, verification, and
   publication; the UI does not show a percentage. Detailed progress is in
   Sandstorm's grain debug log.
4. On success, the submission list refreshes and the import form disappears.
   Open the **Public marketplace** link and check the catalog and an app detail
   page. Approved releases appear in the main catalog; experimental releases
   remain pending and public in `experimental/index.json`.

The import endpoint is part of the private grain UI. `spk publish --webkey` is
for individual author SPK submissions. In `spk dev`, rebuild with
`cargo xtask prepare-dev` and restart the grain if it is running an older build
without the import UI.

If the form is missing, check your role, whether the grain already has packages,
and whether another operation is running. Invalid archives fail before packages
are committed, leaving an empty grain available for retry. If packages were
committed but final publication failed, use **Rebuild public site** or restart
the grain to regenerate the site from persisted state.

### Import contents and limits

The archive contains `apps/index.json`, optional `experimental/index.json`, and
`packages/<packageId>` files. Upload limits are 16 GiB per tarball and 1 GiB per
SPK. The importer rejects links, unexpected paths, duplicates, missing packages,
invalid signatures, and mismatched package/app IDs or versions. It verifies all
packages and renders the site in temporary storage before committing them.
Use the sync-generated archive rather than tarring the mirror directory, which
contains extra paths the importer does not accept.

Approved listings become approved and public; experimental listings remain
pending and public. Metadata and images are regenerated from signed manifests;
public catalog author profiles and editorial overrides are not imported.
Publication dates start at import time. Historical releases, embargoed packages,
private review state, and signing keys are not part of this public snapshot.

Allow local disk space for the mirror, tarball, and temporary SPK download, plus
grain storage for the uploaded tarball, extracted packages, verification, and
publication staging. See [Logs and temporary storage](#logs-and-temporary-storage)
for progress logging and grain storage details.

For a private migration of the old index grain, restore its `/var/packages`,
`/var/apps`, and `/var/keybase` data (and optional `/var/descriptions` and
`/var/keybase-token`) into the new grain's storage; startup reconstructs public
output. This is a separate migration path from the public snapshot import.

## Development

To develop against a local Sandstorm server, run from the repository root:

```bash
cargo xtask prepare-dev
spk dev -I crates/marketplace-grain/schema
# For a server at a non-default location:
# spk dev -I crates/marketplace-grain/schema --server /path/to/sandstorm
```

The root `sandstorm-pkgdef.capnp` maps the staged runtime files and imports the
manifest generated by the Rust packager. `prepare-dev` builds a debug executable
and stages its system libraries without signing or accessing the private key.
It requires a local Sandstorm installation accessible to your user.

After editing Rust or embedded HTML/JavaScript, rerun `cargo xtask prepare-dev`,
then restart the development grain to load the rebuilt executable. Restart
`spk dev` too if changing the manifest or using its `--cache` option.
The root pkgdef records the public app ID; developers do not need its private
key. If creating a separately signed fork, update that ID to match your key.

The grain stores packages and submission state under `/var/packages`, builds a
private catalog under `/var/catalog`, and calls the publisher library after state
changes. The publisher stages a complete replacement next to `/var/www`, then
uses Linux `renameat2(RENAME_EXCHANGE)` to switch generations. Failed rendering
leaves the previous site intact. Retrying the operation, clicking **Rebuild public site**, or
restarting the grain rebuilds from persisted state. Registry mutations are
serialized; this implementation rebuilds the full site per change. Upload
verification and catalog imports run on a worker thread so capability RPC remains
responsive. During processing, the grain holds a Sandstorm `stayAwake` handle,
shown as an ongoing notification, and releases it when processing finishes.
Canceling that notification stops processing at cancellation checkpoints; imports
check cancellation before committing their staged packages. Conflicting write
requests report that an operation is already in progress.
Static output is a snapshot: a browser spanning two publications can still see
a removed resource disappear between requests.

Preview the fixtures (demo SPK files are placeholders, not installable apps):

```bash
MARKETPLACE_FIXTURE_ROOT=fixtures/app-index cargo run -p marketplace-web
# http://127.0.0.1:9938
```

Generate a static preview:

```bash
cargo run -p marketplace-web -- publish --source fixtures/app-index --output build/public
```

Mirror the existing public catalog for preview:

```bash
cargo run -p marketplace-sync -- sync-remote --output-dir data/app-index
MARKETPLACE_PACKAGE_BASE_URL=https://app-index.sandstorm.io/packages \
  cargo run -p marketplace-web
```

Without `--tarball`, the mirror downloads JSON and images only. Even with
`--tarball`, downloaded SPKs go directly into the archive, not into
`data/app-index/packages`. A self-contained static export needs the referenced
SPKs in its input catalog's `packages/` directory; the preview command above
instead links downloads to the upstream marketplace.

The older `sync-fixtures` SQLite import remains available for development;
`marketplace.db` is not used by the grain or static publisher.

## Validation

```bash
cargo test --workspace
node --test tests/frontend.cjs
# After packaging; requires Bubblewrap:
cargo xtask test-package
# Optional independent compatibility check; requires the old SDK and GnuPG:
cargo xtask test-interop --spk ~/src/sandstorm/build/dev/bin/spk
# Verify and publish a real import into disposable storage (no live grain changes):
MARKETPLACE_IMPORT_TARBALL="$PWD/build/marketplace-test.tar" \
  cargo test --release -p marketplace-grain --test workflow real_catalog_tarball_import -- --ignored --nocapture
```

Rust tests exercise signed submissions, API permissions, streaming uploads,
embargo, approval, publication, replay rejection, withdrawal, restart, and a real
grain process connected over RPC fd 3. Publication tests check rollback and
stale-file removal. Node tests exercise the browser JavaScript.

The package smoke test boots the staged runtime in Bubblewrap with disposable
storage and no host runtime libraries. The optional compatibility runner compares
Rust verification against SDK-generated packages with GnuPG author proofs, runs
the actual `spk publish` client, and asks the SDK to verify the Rust-built package
when available. The SDK and GnuPG serve only as independent test references.

The legacy `spk publish` wire protocol retains its existing unverified webkey-hash
field. This app does not send notification emails to the old Sandstorm administrators.

## Logs and temporary storage

The grain and sync tool write timestamped logs to stderr at info level by default.
Sync reports catalog requests, per-package downloads, byte progress every five
seconds while data arrives, and tarball completion. The grain logs upload receipt,
SPK verification, import stages, review decisions, publication, and failures.
Grain stderr is available in Sandstorm's grain debug log; sync logs appear in the
terminal. `RUST_LOG` overrides the filter, for example:

```bash
RUST_LOG=warn,marketplace_sync=debug cargo run -p marketplace-sync -- sync-remote --limit=10 --tarball build/marketplace-test.tar
```

Grain uploads and SPK decompression use `/var/tmp`. Sandstorm's separate `/tmp`
is limited to 16 MiB and is unsuitable for package verification. Imports also
need temporary disk space for the uploaded tarball, extracted SPKs, and staging;
free space on the host's `/tmp` does not describe grain storage availability.
