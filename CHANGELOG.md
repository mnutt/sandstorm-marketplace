# Changelog

## 0.3.0 — 2026-10-17

- Added private, paginated activity history for each app, covering submissions,
  review decisions, publication, withdrawals, rollbacks, and deprecation. Events
  include actor attribution and detailed records.
- Centralized registry metadata and audit history in SQLite, with transactional
  state changes and recovery of interrupted publication updates. SPK bundles
  remain separate files.
- Added persistent admin app URLs, supporting direct links, reloads, and browser
  back/forward navigation within Sandstorm.
- Refined the administration interface, including app information, review actions,
  approval queue navigation, and links to published app pages.
- Added release build cache warming on pushes to `main` to speed up tagged builds.

## 0.2.0 — 2026-10-16

- Added app deprecation with optional replacement links. Deprecated apps are hidden
  from discovery while their direct pages and package downloads remain available.
  Administrators can restore them to the indexes at any time.

## 0.1.0

- Initial app release
