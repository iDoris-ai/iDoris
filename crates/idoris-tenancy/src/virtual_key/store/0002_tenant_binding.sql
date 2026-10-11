ALTER TABLE virtual_keys ADD COLUMN tenant_id TEXT
    CHECK (tenant_id IS NULL OR (trim(tenant_id) <> '' AND tenant_id = trim(tenant_id)));
