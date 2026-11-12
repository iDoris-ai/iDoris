#!/usr/bin/env bash
# 离线演练：本机已有的 litestream/litestream:0.5.18，复制到本地 file 副本再恢复。
# 不 docker pull，不访问外网，不用真实密钥。MinIO 镜像本次拉不下来，所以这里不用它。
# 需要：sqlite3、docker，以及已经在本机的镜像 litestream/litestream:0.5.18。
set -euo pipefail

IMAGE="litestream/litestream:0.5.18"
ROOT="$(cd "$(dirname "$0")" && pwd)/.drill-work"
rm -rf "$ROOT"
mkdir -p "$ROOT/data" "$ROOT/replica" "$ROOT/out"
cleanup() { rm -rf "$ROOT"; }
trap cleanup EXIT

if ! command -v sqlite3 >/dev/null; then
  echo "缺少 sqlite3" >&2
  exit 1
fi
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "本机没有镜像 $IMAGE。脚本不会拉取。请在有网时自行 docker pull，再离线重跑。" >&2
  exit 1
fi

sqlite3 "$ROOT/data/ledger.sqlite" <<'SQL'
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA busy_timeout=5000;
CREATE TABLE attempts (
  attempt_id TEXT PRIMARY KEY,
  state TEXT NOT NULL
);
INSERT INTO attempts(attempt_id, state) VALUES ('A', 'settled');
SQL

echo "== replicate A =="
docker run --rm --pull never \
  -v "$ROOT:/work" \
  "$IMAGE" \
  replicate -once /work/data/ledger.sqlite file:///work/replica/ledger

sqlite3 "$ROOT/data/ledger.sqlite" \
  "INSERT INTO attempts(attempt_id, state) VALUES ('B', 'maybe_executed');"

echo "== replicate B =="
docker run --rm --pull never \
  -v "$ROOT:/work" \
  "$IMAGE" \
  replicate -once /work/data/ledger.sqlite file:///work/replica/ledger

# 最后一次复制之后的写入必须不在恢复结果里。这就是 RPO 窗口。
sqlite3 "$ROOT/data/ledger.sqlite" \
  "INSERT INTO attempts(attempt_id, state) VALUES ('C', 'executed');"

rm -f "$ROOT/data/ledger.sqlite" "$ROOT/data/ledger.sqlite-wal" "$ROOT/data/ledger.sqlite-shm"

echo "== restore =="
docker run --rm --pull never \
  -v "$ROOT:/work" \
  "$IMAGE" \
  restore -o /work/out/restored.sqlite file:///work/replica/ledger

echo "== restored rows =="
sqlite3 "$ROOT/out/restored.sqlite" <<'SQL'
PRAGMA integrity_check;
SELECT attempt_id || '|' || state FROM attempts ORDER BY attempt_id;
SQL

got="$(sqlite3 "$ROOT/out/restored.sqlite" "SELECT group_concat(attempt_id, ',') FROM (SELECT attempt_id FROM attempts ORDER BY attempt_id);")"
if [[ "$got" != "A,B" ]]; then
  echo "恢复结果应为 A,B，实际是 ${got}" >&2
  exit 1
fi

echo "== reconcile fixture =="
sqlite3 "$ROOT/out/reconcile.sqlite" <<'SQL'
PRAGMA foreign_keys=ON;
CREATE TABLE attempts (
  attempt_id TEXT PRIMARY KEY,
  request_id TEXT NOT NULL,
  state TEXT NOT NULL,
  pays_user INTEGER NOT NULL,
  reserved_minor INTEGER NOT NULL
);
CREATE TABLE postings (
  posting_id INTEGER PRIMARY KEY,
  attempt_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  amount_minor INTEGER NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE
);
CREATE TABLE upstream (
  attempt_id TEXT PRIMARY KEY,
  evidence TEXT NOT NULL CHECK (evidence IN ('line', 'not_found')),
  billed_minor INTEGER
);

INSERT INTO attempts VALUES
  ('win', 'req-1', 'settled', 1, 100),
  ('extra', 'req-1', 'executed', 0, 80),
  ('held', 'req-2', 'maybe_executed', 1, 50),
  ('miss', 'req-3', 'maybe_executed', 1, 70),
  ('plat', 'req-4', 'executed', 0, 40);

INSERT INTO postings(attempt_id, kind, amount_minor, idempotency_key) VALUES
  ('win', 'user_price', 100, 'win|user_price|settle'),
  ('extra', 'user_price', 80, 'extra|user_price|settle');

INSERT INTO upstream VALUES
  ('win', 'line', 100),
  ('extra', 'line', 80),
  ('miss', 'line', 70),
  ('plat', 'line', 40);
-- held 没有账单行。not_found 也不在这里：没有证据就不能释放。

-- 第二次结算必须被幂等键拒绝。
SELECT CASE
  WHEN EXISTS (
    SELECT 1 FROM postings WHERE idempotency_key = 'win|user_price|settle'
  ) THEN 'duplicate_rejected_by_key'
END;
SQL

set +e
dup_err="$(sqlite3 "$ROOT/out/reconcile.sqlite" \
  "INSERT INTO postings(attempt_id, kind, amount_minor, idempotency_key) VALUES ('win','user_price',100,'win|user_price|settle');" 2>&1)"
dup_rc=$?
set -e
if [[ $dup_rc -eq 0 ]]; then
  echo "重复幂等键应当失败" >&2
  exit 1
fi
echo "duplicate insert: ${dup_err}"

echo "== classification =="
sqlite3 -cmd ".mode list" "$ROOT/out/reconcile.sqlite" <<'SQL'
WITH flags AS (
  SELECT
    a.attempt_id,
    a.request_id,
    a.state,
    a.pays_user,
    CASE
      WHEN a.pays_user = 1 AND (
        SELECT count(*) FROM postings p
        WHERE p.attempt_id = a.attempt_id AND p.kind = 'user_price'
      ) > 1 THEN '重复结算'
      WHEN (
        SELECT count(*) FROM attempts a2
        JOIN postings p ON p.attempt_id = a2.attempt_id AND p.kind = 'user_price'
        WHERE a2.request_id = a.request_id AND a2.pays_user = 0
      ) > 0 AND a.pays_user = 0 THEN '重复结算'
      WHEN u.evidence = 'line'
        AND a.state IN ('maybe_executed', 'executed')
        AND NOT EXISTS (
          SELECT 1 FROM postings p
          WHERE p.attempt_id = a.attempt_id
            AND p.kind = CASE WHEN a.pays_user = 1 THEN 'user_price' ELSE 'platform_cost' END
        ) THEN '漏记'
      WHEN a.state = 'maybe_executed' AND u.attempt_id IS NULL THEN '保持预留'
      WHEN a.state = 'settled'
        AND EXISTS (
          SELECT 1 FROM postings p
          WHERE p.attempt_id = a.attempt_id AND p.kind = 'user_price'
        ) THEN '已结算'
      ELSE '未分类'
    END AS klass
  FROM attempts a
  LEFT JOIN upstream u ON u.attempt_id = a.attempt_id
)
SELECT attempt_id || '=' || klass FROM flags ORDER BY attempt_id;
SQL

got_class="$(sqlite3 "$ROOT/out/reconcile.sqlite" <<'SQL'
WITH flags AS (
  SELECT
    a.attempt_id,
    CASE
      WHEN a.pays_user = 1 AND (
        SELECT count(*) FROM postings p
        WHERE p.attempt_id = a.attempt_id AND p.kind = 'user_price'
      ) > 1 THEN '重复结算'
      WHEN (
        SELECT count(*) FROM attempts a2
        JOIN postings p ON p.attempt_id = a2.attempt_id AND p.kind = 'user_price'
        WHERE a2.request_id = a.request_id AND a2.pays_user = 0
      ) > 0 AND a.pays_user = 0 THEN '重复结算'
      WHEN u.evidence = 'line'
        AND a.state IN ('maybe_executed', 'executed')
        AND NOT EXISTS (
          SELECT 1 FROM postings p
          WHERE p.attempt_id = a.attempt_id
            AND p.kind = CASE WHEN a.pays_user = 1 THEN 'user_price' ELSE 'platform_cost' END
        ) THEN '漏记'
      WHEN a.state = 'maybe_executed' AND u.attempt_id IS NULL THEN '保持预留'
      WHEN a.state = 'settled'
        AND EXISTS (
          SELECT 1 FROM postings p
          WHERE p.attempt_id = a.attempt_id AND p.kind = 'user_price'
        ) THEN '已结算'
      ELSE '未分类'
    END AS klass
  FROM attempts a
  LEFT JOIN upstream u ON u.attempt_id = a.attempt_id
)
SELECT group_concat(attempt_id || '=' || klass, ',') FROM (
  SELECT * FROM flags ORDER BY attempt_id
);
SQL
)"
expect="extra=重复结算,held=保持预留,miss=漏记,plat=漏记,win=已结算"
if [[ "$got_class" != "$expect" ]]; then
  echo "对账分类应为 ${expect}" >&2
  echo "实际 ${got_class}" >&2
  exit 1
fi

echo "PASS restore=A,B gap=C class=${got_class}"
