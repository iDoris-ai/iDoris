ALTER TABLE virtual_keys ADD COLUMN tenant_id TEXT
    CHECK (tenant_id IS NULL OR (length(tenant_id) BETWEEN 1 AND 128 AND trim(tenant_id) = tenant_id));
