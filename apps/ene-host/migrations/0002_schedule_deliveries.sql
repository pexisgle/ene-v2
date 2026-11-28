CREATE TABLE schedule_deliveries (
    schedule_id TEXT NOT NULL,
    due INTEGER NOT NULL,
    turn_id TEXT NOT NULL UNIQUE,
    ready_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (schedule_id, due),
    FOREIGN KEY (schedule_id) REFERENCES schedules(id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE INDEX schedule_deliveries_ready ON schedule_deliveries(ready_at);
