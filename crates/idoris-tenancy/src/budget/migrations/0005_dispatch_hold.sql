-- A dispatched call keeps its reserved budget until its outcome is known,
-- even when the ordinary abandoned-reservation TTL has elapsed.
ALTER TABLE reservations ADD COLUMN dispatch_hold INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_hold IN (0, 1));
