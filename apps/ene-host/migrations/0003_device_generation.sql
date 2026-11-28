CREATE TABLE device_generation (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    generation INTEGER NOT NULL CHECK (generation >= 0)
) STRICT;

INSERT INTO device_generation (id,generation)
SELECT 1, COALESCE(MAX(CAST(json_extract(body, '$.generation') AS INTEGER)), 0)
FROM records WHERE kind = 'activation';
