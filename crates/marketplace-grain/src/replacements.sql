CREATE TABLE signing_key_rules (
    original TEXT PRIMARY KEY,
    replacement TEXT UNIQUE,
    allowed_packages_json TEXT
);
ALTER TABLE registry_state ADD COLUMN key_rules_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE releases ADD COLUMN key_error TEXT;
