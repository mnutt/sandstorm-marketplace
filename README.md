# sandstorm-marketplace

Rust workspace for a replacement Sandstorm app marketplace.

Current layout:

- `crates/marketplace-model`: shared JSON/domain models for Sandstorm app-index exports.
- `crates/marketplace-web`: Axum + Askama server-rendered marketplace.
- `crates/marketplace-sync`: import/sync utilities for fixture or remote app-index data.
- `fixtures/app-index`: local fixture data matching the existing Sandstorm export shape.

## Run the web app

```bash
cargo run -p marketplace-web
```

The server defaults to `http://127.0.0.1:9938` and reads fixture data from `fixtures/app-index`.

## Run the sync stub

```bash
cargo run -p marketplace-sync -- --database-url sqlite://marketplace.db sync-fixtures
```

## Mirror the live Sandstorm app index

```bash
cargo run -p marketplace-sync -- sync-remote --output-dir data/app-index
MARKETPLACE_PACKAGE_BASE_URL=https://app-index.sandstorm.io/packages \
cargo run -p marketplace-web
```

`sync-remote` downloads the published app list, per-app detail JSON, and referenced image assets from `https://app-index.sandstorm.io`.
If `data/app-index` exists, the web app will use it automatically; otherwise it falls back to `fixtures/app-index`.
