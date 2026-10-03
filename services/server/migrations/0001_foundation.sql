CREATE TABLE peppy_schema_marker (
    version SMALLINT PRIMARY KEY CHECK (version = 1)
);

INSERT INTO peppy_schema_marker (version) VALUES (1);
