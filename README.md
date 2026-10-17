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
                                  replace /var/www
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
                               /var/packages (immutable SPKs)
                               /var/marketplace.sqlite (metadata and history)
                                              │
                               /var/catalog (generated catalog)
                                              │
                               /var/www (generated public site)
```

## Build and package

Requires Linux, Rust/Cargo, the Cap'n Proto schema compiler (`capnp` on PATH, or
set `CAPNP`) with its standard schemas, and `ldd` for staging system runtime
libraries. On Debian/Ubuntu, install both `capnproto` and `libcapnp-dev`; the latter
provides schemas such as `/capnp/stream.capnp`. Application code,
SPK signature verification, compression, packaging, and test orchestration are
Rust. No Python, C++ application backend, Sandstorm SDK, `xz`, or GnuPG is needed
for normal builds or runtime. Browser interfaces use HTML and JavaScript.

```bash
cargo xtask package --contact-email you@example.com
# Equivalent optional Make wrapper:
make package CONTACT_EMAIL=you@example.com
```

For a disposable build with no private signing key:

```bash
cargo xtask package --test-key
```

This uses a fixed **public** test signing key, labels the app **Sandstorm
Marketplace (Test)**, and neither reads nor writes `.local` signing files. It
has a separate app identity from normal builds. Anyone can sign updates for the
test identity; do not use it for production or sensitive data.

The result is `build/marketplace.spk`. The Rust packager builds the grain, stages
its executable and shared system libraries, constructs the manifest, and signs
and compresses the SPK. A signing identity is created in
`.local/marketplace.keyring`, with its app ID in `.local/app-id`. Keep both files
for future updates; they are ignored by Git and excluded from the package.
Existing SDK-format signing keys are preserved. An incomplete or mismatched
identity causes packaging to fail instead of silently changing the app ID.

The default contact address is a placeholder for local development. Set a real
address before distributing the package. Increment `APP_VERSION` and update
`MARKETING_VERSION` in `xtask/src/package.rs` for releases, or supply explicit
`--app-version` and `--marketing-version` values to `cargo xtask package`.

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
Pending updates requested for publication appear in `experimental/index.json`
and have public downloads only after the app has had an approved release.
First-time submissions stay private, including their metadata, images, and SPKs,
until approval. Embargoed, withdrawn, and rejected releases stay out of public
output. Older approved, still-published versions remain downloadable.

Add `?experimental=true` to the marketplace URL to opt into pending updates.
The listing contains only apps with a newer eligible pending release; apps
without a pending update are omitted. App detail pages show that pending
release’s description, images, and SPK download/install URLs. Search, category links, app pages, and installation links
preserve this mode and the chosen Sandstorm host. Normal URLs (or
`?experimental=false`) use the approved catalog. Pending versions older than an
approved public release are excluded, including while a rollback is selected.
Approval history survives revocation and restarts in SQLite's
`releases.ever_approved` field.

To use this marketplace for a Sandstorm installation, set its App Market URL
and App Index URL to the public site URL in the server's configuration.

```text
/                         marketplace listing
/app/<appId>/             static app detail page
/apps/index.json          approved app summaries
/apps/<appId>.json         app details
/experimental/index.json  eligible pending updates
/images/<imageId>          icons and screenshots
/packages/<packageId>      signed SPK downloads
```

## Reviewing submissions and managing versions

The administration page opens on a table of **Existing apps**, ordered by the
most recent first-publication date among each app's stored releases, newest
first. Apps that have never been published appear last. The table shows the
current public version, last publication date, and links to app information and version history.
Search by app name, app/package ID, or contact email.

The **Awaiting approval** rail on the right shows pending releases whose authors
requested review or publication, newest received first. It remains visible
independently of the table search; on narrow screens it moves below the table.
Existing packages without receipt timestamps display “Not recorded”. The page
refreshes automatically every 30 seconds when idle.

Open **Package details & description** to inspect a release, then **Approve**,
**Reject / request update**, or **Return to review**. Each action explains its
publication effect before confirmation. Rejection requires a reason, which the
author can retrieve through submission status; no email is sent. Approving an
embargoed release keeps it private until the author requests publication.
Reviewers have read-only access; owners and approvers can make changes.

Click an app name or release count to open its detail page with app information
and all stored releases, newest build first, including rejected and withdrawn
packages and the current public version. Use **All apps** or browser Back to
return to the table. **Roll back to this version** selects an older approved release whose
author still requests publication. This changes the public catalog listing;
it does not downgrade installed grains or remove other approved downloads.
Reject a faulty release separately if its download must be removed.

Use **Preview listing** on a release to see its marketplace page before approving
or publishing it. The preview uses the public listing template, including the
icon, screenshots, description, and version, but is only available through an
authenticated reviewer or approver session. Previewing does not change approval
or publication state; private images are embedded rather than published.

The preview includes **Download SPK** and **Install for testing**. Downloads
stream through the authenticated administration session; submitter API tokens
cannot preview or download private packages. For an embargoed or rejected
release, Install explains how to download the SPK and upload it using **Upload
app** on your Sandstorm server's Apps page, with a button to open that page.

For releases with a public package download, **Install for testing** asks for
your Sandstorm server URL and opens that server's installer in a new tab. It
uses the public package URL, so your server must be able to reach the marketplace.
Neither downloading nor installing changes the release's approval state.

Rollback selections persist in SQLite, including across restarts
and new approvals. **Use latest approved** clears the selection and returns to
the highest eligible version. If the selected release is withdrawn, embargoed,
or loses approval, the latest eligible approved release is used instead. The
selection takes effect again if that release becomes eligible, until cleared.
If final publication fails after a decision is saved, **Rebuild public site**
under **Publishing setup & maintenance** retries publication from stored state.

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
   remain pending; updates to approved apps appear in `experimental/index.json`.
   Experimental-only apps with no approval history remain private.

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

Approved listings become approved and public; eligible experimental listings remain
pending and public. Metadata and images are regenerated from signed manifests;
author display profiles are preserved from the imported catalog for each exact
release. These profiles are catalog-supplied metadata, not verified identity proofs;
a Keybase lookup takes precedence. Editorial overrides are not imported.
Publication dates start at import time. Historical releases, embargoed packages,
private review state, and signing keys are not part of this public snapshot.

Allow local disk space for the mirror, tarball, and temporary SPK download, plus
grain storage for the uploaded tarball, extracted packages, verification, and
publication staging. See [Logs and temporary storage](#logs-and-temporary-storage)
for progress logging and grain storage details.

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

The grain stores SPKs under `/var/packages` and metadata in `/var/marketplace.sqlite`, builds a
private catalog under `/var/catalog`, and calls the publisher library after state
changes. The publisher stages a complete replacement next to `/var/www`, then
uses Linux `renameat2(RENAME_EXCHANGE)` to switch generations when available.
When Sandstorm or the filesystem does not support directory exchange, publication
uses ordinary renames with a saved previous generation. This fallback has a brief
interval where public requests may return a missing-page response, but never
exposes a partially built site. Failed installation restores the previous site;
an interrupted swap is recovered on the next publication attempt before rendering.
Failed rendering leaves the previous site intact. Retrying the operation, clicking **Rebuild public site**, or
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
generates the same static site in temporary storage and links downloads to the
upstream marketplace. The preview is regenerated when the server starts.

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

## Browser acceptance tests and GitHub Actions

```bash
npm ci --ignore-scripts
npx playwright install --with-deps chromium
npm run test:acceptance
# Equivalent Make target: make test-acceptance
```

Playwright starts a loopback-only HTTP adapter on port 9940. The adapter uses the
production grain WebSession RPC handlers, signed test SPKs, and a disposable
registry; review decisions, publication, and private downloads execute real Rust
code. Only Sandstorm platform services (public URL and the installer destination)
are stubbed. Tests reset storage before each scenario and cover approval/rejection,
version history, rollback, permissions, embargoed downloads, malformed-package
recovery, automatic refresh, refresh failures, and mobile layout. Separate Rust
tests boot the actual grain process to verify recovery after startup publication
fails. The adapter is an ignored test binary, never shipped in the SPK.

To use an existing Chromium instead of the Playwright-managed browser:

```bash
PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH=/usr/bin/chromium npm run test:acceptance
```

`.github/workflows/ci.yml` runs formatting, Rust tests, frontend tests, browser
acceptance tests, and a packaged-runtime smoke test on main pushes and pull
requests. Failed browser runs upload screenshots, traces, and an HTML report.

Pushing a `v*` tag invokes `.github/workflows/spk.yml`. It first runs the same CI
checks, then builds an optimized SPK using the public dummy key, smoke-tests it,
and attaches `marketplace-test.spk` to a GitHub **prerelease** for the tag. No
signing secrets or spktool installation are required. The workflow run number
provides the test app's numeric version; the tag labels its marketing version.
This publishes a downloadable test artifact, not a production app-market
submission. Production signing remains a local operation with the private key.

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

### App deprecation

Open an app in the admin UI to deprecate it, optionally selecting a replacement
app with an approved public release. Deprecated apps disappear from the home,
category, search, and experimental discovery indexes (including their JSON
indexes). Direct app pages remain available with a deprecation banner and a link
to the replacement, if selected. Public SPKs remain available. Deprecation does not approve private submissions or change release
approval, author withdrawal, or rollback behavior.

Use **Restore app to indexes** to undo deprecation. This policy is app-level and
survives new releases and restarts. App policy is stored in SQLite and
included in Sandstorm grain backups. A public catalog sync/import only includes
listed apps; it is not a full grain backup and does not preserve deprecated apps.

Admin app pages use `/<app_id>/` URLs. Navigation updates Sandstorm’s address bar,
and reloading or reopening a link restores the selected app. Browser Back and
Forward work between the app list and app pages.

### Private audit history

Each app's administration page includes **Activity history**, available to reviewers
and approvers. It records package submissions and imports, author requests for
review/publication or withdrawal, approvals, rejections, approval revocations,
rollback selections, and deprecation/restoration. Separate visibility events show
when a release's public download or approved/experimental listing changes.
Approval remains recorded when publication fails; a successful retry records the
resulting visibility change without duplicating the decision. Unchanged rebuilds,
status queries, and identical retries do not add events. A fresh signed author
request with a newer sequence number records **Resubmitted — no state change**
when it repeats the existing submission state. This records the author and request
without changing review status or adding another publication event.

Admin events retain the Sandstorm identity ID and display name supplied at session
creation. Signed author requests always show **Signing key** in the Actor column,
with the verified key in Details. Submission events omit any webkey-associated
user from both the Actor column and displayed details; that identity does not
establish who used the key. Raw upload events show **Submission API**, since the
package signature alone does not identify its uploader. Stored session metadata
is retained in the private audit records. Admin actions continue to show the
acting user's identity.
Publication events identify the marketplace system and reference the most recent
app decision and its actor. This reference provides context, not proof that one
decision was the sole cause when several changes preceded publication. Recovery
observations use their observation time rather than inventing a publication time.

History is private in SQLite's `audit_events` table, indexed by app ID and event
number. The UI fetches **50 events** at a time through
`audit/<appId>[/<before-event-number>]`. An indexed range query reads each page
without scanning older history; stable cursors exclude newer arrivals. Loaded
pages survive automatic admin refreshes; **Refresh** reloads the newest page.
Events are limited to 32 KiB, and new rejection reasons to 8 KiB of JSON-encoded
text.
There is no automatic history deletion.

State changes and their audit entries commit in one SQLite transaction. Publication
records its intended package visibility and decision context before building the
site. A public `generation.txt` contains only the generation number and is installed
with the site. After installation, visibility checkpoints and events commit together.
If installation succeeds but the database update is interrupted, the next publication
or decision reconciles that generation first. Imports record
the importing session and catalog state without claiming to know upstream reviewers.

Private state is included in Sandstorm grain backups, never in the generated public
site or public catalog export. This is an application history, not a tamper-proof
ledger against someone who can modify grain storage or restore an older backup.

### SQLite storage

`/var/marketplace.sqlite` is authoritative for all registry metadata. The schema is
in [`schema.sql`](crates/marketplace-grain/src/schema.sql):

| Tables | Contents |
| --- | --- |
| `apps` | Rollback selection, deprecation/replacement, description override, audit counter |
| `releases` | App/build identity, verified Cap'n Proto metadata, review and author state, timestamps, approval history, visibility checkpoint |
| `audit_events` | Per-app numbered events, actor snapshots and JSON details |
| `author_profiles`, `release_author_profiles` | Cached Keybase profiles and imported per-release author information |
| `capabilities` | Saved capability tokens |
| `registry_state` | State revision and installed generation |
| `publication_generations`, `publication_items`, `publication_causes` | Publication attempts, visibility snapshots and decision context |

SPKs remain immutable files at `/var/packages/<packageId>/spk`. `/var/catalog` and
`/var/www` are generated output. Verified manifest data and cached Keybase profiles
use Cap'n Proto encoding in BLOB columns; submission state uses
ordinary SQL columns. The unsigned 64-bit submission sequence uses an eight-byte
big-endian BLOB to avoid narrowing it to SQLite's signed integer range.

SQLite is bundled into the executable. Foreign keys and full synchronous durability
are enabled; the database uses rollback journaling with short serialized write
transactions. Package verification, network calls, and site rendering happen outside
those transactions. SPKs become durable before their database rows commit; a failed
upload or import may leave unreferenced files, which verified retries can replace.
Generation summaries remain available; full visibility/cause snapshots are retained
only for the current and most recent prepared generation.

On first startup, the grain creates the schema and records its version in one
transaction. Subsequent startups open the existing database; unsupported schema
versions are rejected. Full grain backups must include the database and SPKs
together.
