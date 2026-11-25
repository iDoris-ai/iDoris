//! Incremental projection over a settled prefix of an account's history.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::{
    Bucket, EntrySums, HOUR_MS, LEGACY_OVERAGE_SQL, MINUTE_MS, WindowIndex, WindowSpec,
    attributed_entries_sql, attributed_sum, entry_sums, load_buckets, load_spends, replay_spends,
    uncovered_without_allowances,
};
use crate::accounts::ledger::error::{LedgerError, add, sub};

// A closed hold's release/settlement row can commit after another hold starts.
// Include those rows with their reservation, and index them as spill entries
// until the watermark overtakes them. Only an actual later spend (late or
// legacy overage) constrains the spend-order boundary.
//
// Parameters: ?1 account, ?2 first entry (inclusive lower bound), ?3 watermark. The fold variant reads
// the live entries up to the watermark plus the later entries of straddling
// holds which this very fold spilled (`folded_at = ?3`), found through the spill
// table, so its cost does not depend on how far away those entries are. The tail
// variant (watermark = i64::MAX) is the plain index scan.
const REPLAY_FOLD_SOURCE: &str = "FROM (
    SELECT * FROM ledger_entries INDEXED BY ledger_entries_by_account_entry
     WHERE account_id = ?1 AND entry_id >= ?2 AND entry_id <= ?3
       AND NOT EXISTS (SELECT 1 FROM ledger_checkpoint_spill s WHERE s.entry_id = ledger_entries.entry_id)
    UNION ALL
    SELECT e2.* FROM ledger_checkpoint_spill s JOIN ledger_entries e2 ON e2.entry_id = s.entry_id
     WHERE s.account_id = ?1 AND s.folded_at = ?3 AND s.entry_id > ?3) e
    WHERE 1";
const REPLAY_TAIL_SOURCE: &str = "FROM ledger_entries e INDEXED BY ledger_entries_by_account_entry
    WHERE e.account_id = ?1 AND e.entry_id >= ?2 AND e.entry_id <= ?3
      AND NOT EXISTS (SELECT 1 FROM ledger_checkpoint_spill s WHERE s.entry_id = e.entry_id)";

#[derive(Clone, Copy)]
pub(super) struct ReplayRange {
    pub(super) through: i64,
}

impl ReplayRange {
    pub(super) const TAIL: Self = Self { through: i64::MAX };

    /// The `FROM ... WHERE ...` source of this replay (see the parameters above).
    pub(super) fn source(self) -> &'static str {
        if self.through == i64::MAX {
            REPLAY_TAIL_SOURCE
        } else {
            REPLAY_FOLD_SOURCE
        }
    }
}

// Entry ids are caller-writable (the released schema allows 0, negative ids and
// even i64::MIN), so no id value can stand for "no checkpoint": the watermark is
// an `Option`, and the lower bound of every replay is the inclusive `lower()`,
// which for an absent watermark is i64::MIN itself.

/// The most entries one request folds into the checkpoint. A cold account
/// catches up over successive requests instead of in one long write
/// transaction holding the account-store mutex.
pub(super) const COMPACTION_BATCH: i64 = 1024;

/// A spend-order prefix containing whole, final holds. The raw entries remain
/// authoritative for historical windows which cut through the prefix.
pub(super) struct Checkpoint {
    pub(super) through: Option<i64>,
    held: i64,
    committed: i64,
    funds: i64,
    uncovered: i64,
    first_at: i64,
    last_at: i64,
    bucket_remaining: HashMap<String, i64>,
}

impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            through: None,
            held: 0,
            committed: 0,
            funds: 0,
            uncovered: 0,
            first_at: 0,
            last_at: 0,
            bucket_remaining: HashMap::new(),
        }
    }
}

impl Checkpoint {
    /// First entry id a replay on top of this checkpoint must read (inclusive).
    pub(super) fn lower(&self) -> i64 {
        self.through.map_or(i64::MIN, |through| through + 1)
    }

    pub(super) fn load(conn: &Connection, account: &str) -> Result<Self, LedgerError> {
        let Some((mut checkpoint, remaining)) = conn
            .query_row(
                "SELECT through_entry, held_micros, committed_micros, funds_micros,
                        uncovered_micros, first_at_ms, last_at_ms, bucket_remaining
                   FROM ledger_checkpoints WHERE account_id = ?1",
                [account],
                |row| {
                    Ok((
                        Self {
                            through: Some(row.get(0)?),
                            held: row.get(1)?,
                            committed: row.get(2)?,
                            funds: row.get(3)?,
                            uncovered: row.get(4)?,
                            first_at: row.get(5)?,
                            last_at: row.get(6)?,
                            ..Self::default()
                        },
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(Self::default());
        };
        checkpoint.bucket_remaining = serde_json::from_str(&remaining).map_err(|error| {
            LedgerError::Corrupt(format!("invalid checkpoint buckets for {account}: {error}"))
        })?;
        Ok(checkpoint)
    }

    pub(super) fn advance(
        &mut self,
        conn: &Connection,
        account: &str,
        now: i64,
        ttl: i64,
    ) -> Result<(), LedgerError> {
        // Amortize checkpoint writes. The account/entry index bounds this probe
        // even when other accounts have a much longer history.
        let enough: bool = conn.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM ledger_entries INDEXED BY ledger_entries_by_account_entry
                 WHERE account_id = ?1 AND entry_id >= ?2 LIMIT 1 OFFSET 255)",
            params![account, self.lower()],
            |row| row.get(0),
        )?;
        if !enough {
            return Ok(());
        }

        // The longest sliding rate window is one hour. Also leave the
        // configured TTL and each hold's stored expiry on the live side.
        let horizon = now.saturating_sub(HOUR_MS.max(ttl));
        // Fold at most one batch per call. Holds that straddle the cutoff are
        // read whole (their later entries come through the by-hold index) and
        // the straddle machinery below spills them.
        let cutoff: i64 = conn.query_row(
            "SELECT COALESCE(
                 (SELECT entry_id FROM ledger_entries INDEXED BY ledger_entries_by_account_entry
                   WHERE account_id = ?1 AND entry_id >= ?2
                   ORDER BY entry_id LIMIT 1 OFFSET ?3),
                 (SELECT MAX(entry_id) FROM ledger_entries INDEXED BY ledger_entries_by_account_entry
                   WHERE account_id = ?1))",
            params![account, self.lower(), COMPACTION_BATCH - 1],
            |row| row.get(0),
        )?;
        // A hold is final once settled/released past the horizon, or expired
        // past the horizon with a verified zero net (an orphan nobody will
        // report on). A late charge on a folded orphan appends to a folded hold,
        // which invalidates the checkpoint, so the rebuild replays it in order.
        let mut groups = conn.prepare(&format!(
            "SELECT MIN(e.entry_id), MAX(e.entry_id), e.hold_id,
                    MAX(e.at_ms) >= ?3 OR MAX(e.window_at_ms) >= ?3
                      OR (e.hold_id IS NOT NULL AND
                          (h.state IS NULL
                           OR (h.state = 'expired' AND (h.expires_at_ms >= ?3
                               OR SUM(CASE WHEN e.kind <> 'late_settle'
                                           THEN e.held_micros + e.spent_micros ELSE 0 END) <> 0))
                           OR (h.state IN ('settled', 'released')
                               AND (h.expires_at_ms >= ?3 OR h.closed_at_ms >= ?3))
                           OR h.state NOT IN ('settled', 'released', 'expired'))),
                    MAX(CASE WHEN e.kind = 'late_settle' AND e.spent_micros <> 0
                               OR e.kind = 'settle' AND {LEGACY_OVERAGE_SQL} > 0
                             THEN e.entry_id END)
               FROM (SELECT * FROM ledger_entries INDEXED BY ledger_entries_by_account_entry
                      WHERE account_id = ?1 AND entry_id >= ?2 AND entry_id <= ?4
                     UNION ALL
                     SELECT * FROM ledger_entries INDEXED BY ledger_entries_by_hold
                      WHERE account_id = ?1 AND entry_id > ?4
                        AND hold_id IN (SELECT hold_id FROM ledger_entries INDEXED BY ledger_entries_by_account_entry
                                         WHERE account_id = ?1 AND entry_id >= ?2 AND entry_id <= ?4
                                           AND hold_id IS NOT NULL)) e
               LEFT JOIN ledger_holds h ON h.hold_id = e.hold_id
              WHERE NOT EXISTS (SELECT 1 FROM ledger_checkpoint_spill s WHERE s.entry_id = e.entry_id)
              GROUP BY e.hold_id, CASE WHEN e.hold_id IS NULL THEN e.entry_id END
              ORDER BY 5 DESC",
        ))?;
        let mut rows = groups.query(params![account, self.lower(), horizon, cutoff])?;
        // The watermark may land anywhere up to the cutoff, including inside a
        // hold (straddling holds are spilled below) or on entries which were
        // already spilled by an earlier step.
        // Never fold an entry at i64::MAX: `lower()` is `through + 1`.
        let mut through = cutoff.min(i64::MAX - 1);
        let mut first_retained = i64::MAX;
        let mut ranges = Vec::new();
        while let Some(row) = rows.next()? {
            let first: i64 = row.get(0)?;
            let last: i64 = row.get(1)?;
            let retained: bool = row.get(3)?;
            if retained {
                first_retained = first_retained.min(first);
            }
            ranges.push((
                first,
                last,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(4)?,
            ));
        }
        // Retaining a hold also retains its original position in the replay.
        // A late or legacy spend after that position pins its entire hold.
        // Descending spend ID handles chains of overlapping split settlements.
        if first_retained != i64::MAX {
            through = through.min(first_retained.saturating_sub(1));
        }
        for (first, _, _, last_spend) in &ranges {
            if last_spend.is_some_and(|spend| spend > through) {
                through = through.min(first.saturating_sub(1));
            }
        }
        // Advance only over at least one entry (live, or already spilled by an earlier step).
        if self.through.is_some_and(|previous| through <= previous)
            || !conn.query_row::<bool, _, _>(
                "SELECT EXISTS (
                     SELECT 1 FROM ledger_entries INDEXED BY ledger_entries_by_account_entry
                      WHERE account_id = ?1 AND entry_id >= ?2 AND entry_id <= ?3)",
                params![account, self.lower(), through],
                |row| row.get(0),
            )?
        {
            return Ok(());
        }

        for (first, last, hold, _) in ranges {
            if first <= through && last > through {
                conn.execute(
                    "INSERT INTO ledger_checkpoint_spill (entry_id, account_id, folded_at)
                     SELECT entry_id, ?1, ?2 FROM ledger_entries INDEXED BY ledger_entries_by_hold
                     WHERE hold_id = ?3 AND account_id = ?1 AND entry_id > ?2",
                    params![account, through, hold],
                )?;
            }
        }
        let range = ReplayRange { through };
        let (held, committed, funds, first_at, last_at): (i64, i64, i64, i64, i64) = conn.query_row(
            &format!("SELECT COALESCE(SUM(held_micros), 0), COALESCE(SUM(held_micros + spent_micros), 0),
                    COALESCE(SUM(funds_micros), 0),
                    COALESCE(MIN(CASE WHEN held_micros <> 0 OR spent_micros <> 0
                                     THEN MIN(window_at_ms, at_ms) END), ?4),
                    COALESCE(MAX(CASE WHEN held_micros <> 0 OR spent_micros <> 0
                                     THEN MAX(window_at_ms, at_ms) END), ?5)
               {}", range.source()),
            params![account, self.lower(), through, i64::MAX, i64::MIN],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        let (ids, buckets) = load_buckets(conn, account)?;
        let (uncovered, left) = replay_range(conn, account, self, range, &ids, &buckets)?;
        conn.execute(
            "DELETE FROM ledger_checkpoint_spill WHERE account_id = ?1 AND entry_id <= ?2",
            params![account, through],
        )?;
        let remaining = ids.into_iter().zip(left).collect();
        *self = Self {
            through: Some(through),
            held: add(self.held, held)?,
            committed: add(self.committed, committed)?,
            funds: add(self.funds, funds)?,
            uncovered,
            first_at: if self.through.is_none() {
                first_at
            } else {
                self.first_at.min(first_at)
            },
            last_at: if self.through.is_none() {
                last_at
            } else {
                self.last_at.max(last_at)
            },
            bucket_remaining: remaining,
        };
        let remaining = serde_json::to_string(&self.bucket_remaining)
            .map_err(|error| LedgerError::Corrupt(error.to_string()))?;
        conn.execute(
            "INSERT INTO ledger_checkpoints VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(account_id) DO UPDATE SET
                through_entry = excluded.through_entry, held_micros = excluded.held_micros,
                committed_micros = excluded.committed_micros, funds_micros = excluded.funds_micros,
                uncovered_micros = excluded.uncovered_micros, first_at_ms = excluded.first_at_ms,
                last_at_ms = excluded.last_at_ms, bucket_remaining = excluded.bucket_remaining",
            params![
                account,
                through,
                self.held,
                self.committed,
                self.funds,
                uncovered,
                self.first_at,
                self.last_at,
                remaining
            ],
        )?;
        Ok(())
    }

    pub(super) fn sums(
        &self,
        conn: &Connection,
        account: &str,
        allowance_from: Option<i64>,
        now: i64,
    ) -> Result<EntrySums, LedgerError> {
        let window_from = allowance_from.unwrap_or(now);
        let Some(through) = self.through else {
            return entry_sums(conn, account, window_from, now, None);
        };
        let mut sums = entry_sums(conn, account, window_from, now, Some(self.lower()))?;
        sums.held = add(sums.held, self.held)?;
        sums.committed = add(sums.committed, self.committed)?;
        sums.funds = add(sums.funds, self.funds)?;
        sums.has_funds |= self.funds != 0;
        for (first, sum) in [
            (allowance_from, &mut sums.window),
            (Some(sub(now, MINUTE_MS)? + 1), &mut sums.minute),
            (Some(sub(now, HOUR_MS)? + 1), &mut sums.hour),
        ] {
            let Some(first) = first.filter(|first| *first <= self.last_at) else {
                continue;
            };
            if first <= self.first_at {
                *sum = add(*sum, self.committed)?;
            } else {
                // Backdated reads and future backdated grants can select only
                // part of the prefix. Seek by attribution time in that case.
                let window = WindowSpec {
                    condition: "{t} >= ?2",
                };
                let folded = "(e.entry_id <= ?3 OR EXISTS (
                    SELECT 1 FROM ledger_checkpoint_spill s WHERE s.entry_id = e.entry_id))";
                let by_reservation = attributed_entries_sql(
                    &[&window],
                    &format!(
                        "FROM ledger_entries e INDEXED BY ledger_entries_by_account_window
                            WHERE e.account_id = ?1 AND e.window_at_ms >= ?2 AND {folded}"
                    ),
                );
                let by_settlement = attributed_entries_sql(
                    &[&window],
                    &format!(
                        "FROM ledger_entries e INDEXED BY ledger_entries_settled_by_account_at
                            WHERE e.account_id = ?1 AND e.kind = 'settle' AND e.at_ms >= ?2
                              AND e.window_at_ms < ?2 AND {folded}"
                    ),
                );
                *sum = add(
                    *sum,
                    conn.query_row(
                        &format!(
                            "SELECT {} FROM ({by_reservation} UNION ALL {by_settlement})",
                            attributed_sum(&window)
                        ),
                        params![account, first, through],
                        |row| row.get(0),
                    )?,
                )?;
            }
        }
        Ok(sums)
    }
}

pub(super) fn replay_range(
    conn: &Connection,
    account: &str,
    checkpoint: &Checkpoint,
    range: ReplayRange,
    ids: &[String],
    buckets: &[Bucket],
) -> Result<(i64, Vec<i64>), LedgerError> {
    if buckets.is_empty() {
        let uncovered = uncovered_without_allowances(conn, account, checkpoint.lower(), range)?;
        return Ok((add(checkpoint.uncovered, uncovered)?, Vec::new()));
    }
    let left = ids
        .iter()
        .zip(buckets)
        .map(|(id, bucket)| {
            let remaining = checkpoint
                .bucket_remaining
                .get(id)
                .copied()
                .unwrap_or(bucket.capacity);
            if !(0..=bucket.capacity).contains(&remaining) {
                return Err(LedgerError::Corrupt(format!(
                    "invalid checkpoint balance for allocation {id}"
                )));
            }
            Ok(remaining)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let spends = load_spends(conn, account, checkpoint.lower(), range)?;
    let (uncovered, left) = replay_spends(buckets, &spends, WindowIndex::MAX_ENTRIES, left)?;
    Ok((add(checkpoint.uncovered, uncovered)?, left))
}
