CREATE TABLE apps (
    app_id TEXT PRIMARY KEY,
    pinned_package_id TEXT,
    deprecated INTEGER NOT NULL DEFAULT 0 CHECK(deprecated IN (0,1)),
    replacement_app_id TEXT REFERENCES apps(app_id),
    description_override TEXT,
    next_event_number INTEGER NOT NULL DEFAULT 1 CHECK(next_event_number > 0),
    FOREIGN KEY(app_id, pinned_package_id) REFERENCES releases(app_id, package_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE releases (
    package_id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL REFERENCES apps(app_id),
    build_number INTEGER NOT NULL,
    verified_metadata BLOB NOT NULL,
    received_at TEXT,
    request_state INTEGER NOT NULL CHECK(request_state BETWEEN 0 AND 2),
    review_state TEXT NOT NULL CHECK(review_state IN ('pending','approved','needsUpdate','notUploaded')),
    review_reason TEXT NOT NULL,
    first_published_at INTEGER NOT NULL,
    ever_approved INTEGER NOT NULL DEFAULT 0,
    next_sequence_number BLOB NOT NULL CHECK(length(next_sequence_number) = 8),
    public_visibility TEXT NOT NULL DEFAULT '{"approvedListing":false,"experimentalListing":false,"download":false}',
    UNIQUE(app_id, package_id)
);
CREATE INDEX releases_app_build ON releases(app_id, build_number);
CREATE TABLE audit_events (
    app_id TEXT NOT NULL REFERENCES apps(app_id),
    event_number INTEGER NOT NULL CHECK(event_number > 0),
    timestamp TEXT NOT NULL,
    package_id TEXT,
    build_number INTEGER,
    action TEXT NOT NULL,
    actor_json TEXT NOT NULL,
    details_json TEXT NOT NULL,
    PRIMARY KEY(app_id, event_number)
) WITHOUT ROWID;
CREATE INDEX audit_decisions ON audit_events(app_id, event_number)
 WHERE action NOT IN ('publicationChanged','submitted','resubmitted');
CREATE TABLE author_profiles (fingerprint TEXT PRIMARY KEY, profile BLOB NOT NULL);
CREATE TABLE release_author_profiles (
    package_id TEXT PRIMARY KEY REFERENCES releases(package_id), profile_json TEXT NOT NULL
);
CREATE TABLE capabilities (name TEXT PRIMARY KEY, token BLOB NOT NULL);
CREATE TABLE publication_generations (
    generation_id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_revision INTEGER NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('prepared','installed','superseded','failed')),
    created_at TEXT NOT NULL,
    installed_at TEXT,
    last_error TEXT
);
CREATE INDEX publication_status ON publication_generations(status);
CREATE TABLE registry_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    state_revision INTEGER NOT NULL DEFAULT 0,
    installed_generation_id INTEGER REFERENCES publication_generations(generation_id)
);
INSERT INTO registry_state(singleton) VALUES(1);
CREATE TABLE publication_items (
    generation_id INTEGER NOT NULL REFERENCES publication_generations(generation_id),
    package_id TEXT NOT NULL REFERENCES releases(package_id),
    visibility_json TEXT NOT NULL,
    PRIMARY KEY(generation_id,package_id)
) WITHOUT ROWID;
CREATE TABLE publication_causes (
    generation_id INTEGER NOT NULL REFERENCES publication_generations(generation_id),
    app_id TEXT NOT NULL,
    event_number INTEGER NOT NULL,
    PRIMARY KEY(generation_id,app_id),
    FOREIGN KEY(app_id,event_number) REFERENCES audit_events(app_id,event_number)
) WITHOUT ROWID;
