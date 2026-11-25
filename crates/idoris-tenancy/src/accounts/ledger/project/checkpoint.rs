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
pub(super) const REPLAY_RANGE_SQL: &str =
    "e.account_id = ?1 AND e.entry_id > ?2 AND e.entry_id <= ?3
    AND COALESCE((SELECT s.folded_at = ?4 FROM ledger_checkpoint_spill s
                   WHERE s.entry_id = e.entry_id), e.entry_id <= ?4)";

#[derive(Clone, Copy)]
pub(super) struct ReplayRange {
    pub(super) through: i64,
    pub(super) scan_end: i64,
}

impl ReplayRange {
    pub(super) const TAIL: Self = Self {
        through: i64::MAX,
        scan_end: i64::MAX,
    };
}

/// A spend-order prefix containing whole, final holds. The raw entries remain
/// authoritative for historical windows which cut through the prefix.
#[derive(Default)]
pub(super) struct Checkpoint {
    pub(super) through: i64,
    held: i64,
    committed: i64,
    funds: i64,
    uncovered: i64,
    first_at: i64,
    last_at: i64,
    bucket_remaining: HashMap<String, i64>,
}

impl Checkpoint {
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
                            through: row.get(0)?,
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
                 WHERE account_id = ?1 AND entry_id > ?2 LIMIT 1 OFFSET 255)",
            params![account, self.through],
            |row| row.get(0),
        )?;
        if !enough {
            return Ok(());
        }

        // The longest sliding rate window is one hour. Also leave the
        // configured TTL and each hold's stored expiry on the live side.
        let horizon = now.saturating_sub(HOUR_MS.max(ttl));
        let mut groups = conn.prepare(&format!(
            "SELECT MIN(e.entry_id), MAX(e.entry_id), e.hold_id,
                    MAX(e.at_ms) >= ?3 OR MAX(e.window_at_ms) >= ?3
                      OR (e.hold_id IS NOT NULL AND
                          (h.state IS NULL OR h.state NOT IN ('settled', 'released')
                           OR h.expires_at_ms >= ?3 OR h.closed_at_ms >= ?3)),
                    MAX(CASE WHEN e.kind = 'late_settle' AND e.spent_micros <> 0
                               OR e.kind = 'settle' AND {LEGACY_OVERAGE_SQL} > 0
                             THEN e.entry_id ELSE 0 END)
               FROM ledger_entries e INDEXED BY ledger_entries_by_account_entry
               LEFT JOIN ledger_holds h ON h.hold_id = e.hold_id
              WHERE e.account_id = ?1 AND e.entry_id > ?2
                AND NOT EXISTS (SELECT 1 FROM ledger_checkpoint_spill s WHERE s.entry_id = e.entry_id)
              GROUP BY e.hold_id, CASE WHEN e.hold_id IS NULL THEN e.entry_id END
              ORDER BY 5 DESC",
        ))?;
        let mut rows = groups.query(params![account, self.through, horizon])?;
        let mut through = self.through;
        let mut first_retained = i64::MAX;
        let mut ranges = Vec::new();
        while let Some(row) = rows.next()? {
            let first: i64 = row.get(0)?;
            let last: i64 = row.get(1)?;
            let retained: bool = row.get(3)?;
            through = through.max(last);
            if retained {
                first_retained = first_retained.min(first);
            }
            ranges.push((
                first,
                last,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(4)?,
            ));
        }
        // Retaining a hold also retains its original position in the replay.
        // A late or legacy spend after that position pins its entire hold.
        // Descending spend ID handles chains of overlapping split settlements.
        if first_retained != i64::MAX {
            through = through.min(first_retained - 1);
        }
        for (first, _, _, last_spend) in &ranges {
            if *last_spend > through {
                through = through.min(first - 1);
            }
        }
        if through <= self.through || !ranges.iter().any(|(first, ..)| *first <= through) {
            return Ok(());
        }

        let mut scan_end = through;
        for (first, last, hold, _) in ranges {
            if first <= through && last > through {
                conn.execute(
                    "INSERT INTO ledger_checkpoint_spill (entry_id, account_id, folded_at)
                     SELECT entry_id, ?1, ?2 FROM ledger_entries INDEXED BY ledger_entries_by_hold
                     WHERE hold_id = ?3 AND account_id = ?1 AND entry_id > ?2",
                    params![account, through, hold],
                )?;
                scan_end = scan_end.max(last);
            }
        }
        let range = ReplayRange { through, scan_end };
        let (held, committed, funds, first_at, last_at): (i64, i64, i64, i64, i64) = conn.query_row(
            &format!("SELECT COALESCE(SUM(held_micros), 0), COALESCE(SUM(held_micros + spent_micros), 0),
                    COALESCE(SUM(funds_micros), 0),
                    COALESCE(MIN(CASE WHEN held_micros <> 0 OR spent_micros <> 0
                                     THEN MIN(window_at_ms, at_ms) END), ?5),
                    COALESCE(MAX(CASE WHEN held_micros <> 0 OR spent_micros <> 0
                                     THEN MAX(window_at_ms, at_ms) END), ?6)
               FROM ledger_entries e INDEXED BY ledger_entries_by_account_entry
              WHERE {REPLAY_RANGE_SQL}"),
            params![account, self.through, scan_end, through, i64::MAX, i64::MIN],
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
            through,
            held: add(self.held, held)?,
            committed: add(self.committed, committed)?,
            funds: add(self.funds, funds)?,
            uncovered,
            first_at: if self.through == 0 {
                first_at
            } else {
                self.first_at.min(first_at)
            },
            last_at: if self.through == 0 {
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
        if self.through == 0 {
            return entry_sums(conn, account, window_from, now, None);
        }
        let mut sums = entry_sums(conn, account, window_from, now, Some(self.through))?;
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
                        params![account, first, self.through],
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
        let uncovered = uncovered_without_allowances(conn, account, checkpoint.through, range)?;
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
    let spends = load_spends(conn, account, checkpoint.through, range)?;
    let (uncovered, left) = replay_spends(buckets, &spends, WindowIndex::MAX_ENTRIES, left)?;
    Ok((add(checkpoint.uncovered, uncovered)?, left))
}
