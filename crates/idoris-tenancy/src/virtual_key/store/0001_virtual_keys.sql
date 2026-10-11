CREATE TABLE virtual_keys (
    key_id          TEXT PRIMARY KEY CHECK (trim(key_id) <> ''),
    key_hash        BLOB NOT NULL UNIQUE CHECK (length(key_hash) = 32),
    owner           TEXT NOT NULL CHECK (trim(owner) <> ''),
    allowed_privacy TEXT NOT NULL CHECK (
        json_valid(allowed_privacy) AND json_type(allowed_privacy) = 'array'
    ),
    allowed_roles   TEXT NOT NULL CHECK (
        json_valid(allowed_roles) AND json_type(allowed_roles) = 'array'
    ),
    budget_ref      TEXT CHECK (budget_ref IS NULL OR trim(budget_ref) <> ''),
    expires_at_ms   INTEGER CHECK (expires_at_ms IS NULL OR expires_at_ms > 0),
    admin_scopes    TEXT NOT NULL CHECK (
        json_valid(admin_scopes) AND json_type(admin_scopes) = 'array'
    ),
    status          TEXT NOT NULL CHECK (status IN ('active', 'revoked')),
    revoked_at_ms   INTEGER,
    CHECK (
        (status = 'active' AND revoked_at_ms IS NULL)
        OR (status = 'revoked' AND revoked_at_ms IS NOT NULL)
    )
);
