CREATE TABLE event_log_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE, tenant_id TEXT NOT NULL, record_id TEXT NOT NULL,
    event_type TEXT NOT NULL, ts_utc_ms INTEGER NOT NULL, request_id TEXT, session_id TEXT,
    trace_id TEXT, parent_id TEXT, origin_record_id TEXT, metadata TEXT NOT NULL
);
CREATE INDEX event_log_tenant_record_sequence
    ON event_log_events(tenant_id, record_id, sequence);
CREATE TRIGGER event_log_guard_insert BEFORE INSERT ON event_log_events WHEN NEW.sequence != -1 OR EXISTS (SELECT 1 FROM event_log_events WHERE event_id = NEW.event_id) BEGIN SELECT RAISE(ABORT, 'event insert violates append-only invariants'); END;
CREATE TRIGGER event_log_sequence_positive AFTER INSERT ON event_log_events WHEN NEW.sequence <= 0 BEGIN SELECT RAISE(ABORT, 'event sequence must be database-assigned'); END;
CREATE TRIGGER event_log_no_update BEFORE UPDATE ON event_log_events BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;
CREATE TRIGGER event_log_no_delete BEFORE DELETE ON event_log_events BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;
