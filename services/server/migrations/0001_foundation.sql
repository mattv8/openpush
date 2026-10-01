CREATE TABLE openpush_schema_marker (
    version SMALLINT PRIMARY KEY CHECK (version = 1)
);

INSERT INTO openpush_schema_marker (version) VALUES (1);
