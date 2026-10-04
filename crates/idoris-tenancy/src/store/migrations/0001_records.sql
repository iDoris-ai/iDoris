-- Initial tenant record schema; request_id is intentionally non-unique.
CREATE TABLE tenant_records (
    tenant_id        TEXT NOT NULL CHECK (trim(tenant_id) <> ''),
    kind             TEXT NOT NULL CHECK (kind IN ('usage', 'audit')),
    record_id        TEXT NOT NULL CHECK (trim(record_id) <> ''),
    request_id       TEXT NOT NULL CHECK (trim(request_id) <> ''),
    origin_record_id TEXT,
    payload          TEXT NOT NULL CHECK (
        json_valid(payload) AND json_type(payload) = 'object'
    ),
    PRIMARY KEY (tenant_id, kind, record_id)
);
