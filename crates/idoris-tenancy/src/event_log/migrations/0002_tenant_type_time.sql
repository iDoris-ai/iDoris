CREATE INDEX main.event_log_tenant_type_time
    ON event_log_events(tenant_id, event_type, ts_utc_ms);
