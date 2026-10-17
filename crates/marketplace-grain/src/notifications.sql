CREATE TABLE notification_settings (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    recipient TEXT NOT NULL DEFAULT '',
    last_sent_at TEXT
);
INSERT INTO notification_settings(singleton) VALUES(1);
CREATE TABLE notification_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id TEXT NOT NULL,
    event_number INTEGER NOT NULL,
    recipient TEXT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    UNIQUE(app_id,event_number),
    FOREIGN KEY(app_id,event_number) REFERENCES audit_events(app_id,event_number)
);
CREATE INDEX notification_due ON notification_outbox(next_attempt,id);
