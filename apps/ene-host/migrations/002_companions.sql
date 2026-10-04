CREATE TABLE companions (id TEXT PRIMARY KEY, body TEXT NOT NULL);
INSERT INTO companions SELECT json_extract(value, '$.id'), value FROM metadata WHERE key='companion';
DELETE FROM metadata WHERE key='companion';
