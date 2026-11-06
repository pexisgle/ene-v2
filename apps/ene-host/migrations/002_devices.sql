-- Device authorization and operation receipts, one row per record: a change
-- rewrites only the row it changed. `body` holds the generated message as
-- JSON, so the host keeps one representation of each record.
CREATE TABLE devices(
  id TEXT PRIMARY KEY NOT NULL,
  token_hash BLOB NOT NULL,
  body TEXT NOT NULL
);
CREATE TABLE device_operations(
  operation_id TEXT PRIMARY KEY NOT NULL,
  device_id TEXT NOT NULL,
  subscription_id TEXT NOT NULL,
  status INTEGER NOT NULL,
  body TEXT NOT NULL
);
CREATE INDEX device_operations_dispatched ON device_operations(device_id, status);
