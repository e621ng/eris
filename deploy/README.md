# Deploying ERIS

## Pieces

- `compose.dev.yml` – Postgres 15 for local development and tests
  (`docker compose -f deploy/compose.dev.yml up -d`; connection URL
  `postgres://postgres:postgres@127.0.0.1:15432/eris`).
- `compose.oracle.yml` – builds and runs the legacy C++ IQDB from
  `external/iqdb` for the differential harness. Point `ORACLE_DATA` at a
  directory containing `oracle.db` (always a *copy* – the C++ opens it rw).
- The service image is built by the repo `Dockerfile` and published as
  `ghcr.io/e621ng/eris` (CI pushes on `v*` tags, gated on all tests).

## Production shape

- N ≥ 2 identical containers on different hosts, all pointing at the same
  Postgres database (a dedicated `eris` database on the e621ng cluster).
- Load balancer health check = `GET /ready` (it goes unready during
  bootstrap and when the event feed stalls/lags). `GET /healthz` is plain
  process liveness.
- Rolling restart: one node at a time, waiting for `/ready`. A node
  bootstraps the full index from Postgres at startup (seconds to low tens of
  seconds at 6.6M images) and holds no state on disk.
- Memory: ~4.1 GiB steady state at 6.6M images; set container limits with
  bootstrap headroom (6 GiB is comfortable).
- Writes are Postgres transactions; the writing node does NOT special-case
  itself, so a write becomes queryable everywhere (including the writer)
  within the feed interval – ~25ms typical thanks to NOTIFY, bounded by
  `ERIS_FEED_INTERVAL_MS` (2s) when notifications are lost.

## Cutover runbook (from the C++ IQDB)

Full-scale rehearsal of every step below was performed against a copy of the
production database; measured numbers in the design doc.

1. **Prepare**: create the `eris` database; run one node with
   `ERIS_MIGRATE=true` once (or `sqlx` migrations via `eris-migrate import`,
   which migrates automatically).
2. **Import**: with a copy of the production SQLite file:
   `eris-migrate import --sqlite e621.db --database-url $URL`
   (binary COPY, trigger suppressed; ~20s for 6.6M rows), then
   `eris-migrate verify --sqlite e621.db --database-url $URL --sample 10000`
   (row-count equality + hash spot check).
3. **Start nodes**, wait for `/ready`, put them behind the LB. e621ng keeps
   talking to the old service; ERIS serves no traffic yet.
4. **Differential + load check** (optional but recommended):
   `eris-migrate corpus` from the SQLite copy, `eris-migrate diff` against a
   C++ oracle container and one ERIS node (expect zero non-accepted
   divergences), `eris-migrate bench` for the throughput sanity check.
5. **Cutover**:
   - pause the `iqdb` Sidekiq queue (ops action, no code change);
   - take a fresh copy of the C++ service's SQLite file;
   - `eris-migrate import --incremental --sqlite fresh.db --database-url $URL`
     – merge-join delta: only changed/new/removed rows are written, with the
     event trigger active, so running nodes converge on the delta live;
   - flip e621ng's `iqdb_server` to the ERIS LB (per app server – 5 natural
     canary units);
   - resume the queue.
   Queries never stop; index freshness gaps by minutes, indistinguishable
   from the async pipeline's normal lag.
6. **Rollback** is the config flip in reverse (the old service stays warm).
   Posts indexed only in ERIS during the window are repaired by re-enqueueing
   recent update jobs (the existing `db/fixes` pattern).

## Full-scale verification checklist

The commands, in order, as executed for the acceptance run:

```bash
docker compose -f deploy/compose.dev.yml up -d
cargo build --release --workspace

# Import + verify
eris-migrate import --sqlite external/e621.db --database-url $URL
eris-migrate verify --sqlite external/e621.db --database-url $URL --sample 10000

# Memory/latency measurement without a server
eris-migrate stats --sqlite external/e621.db --sample-queries 200

# Two nodes; readiness gates their LB entry
ERIS_DATABASE_URL=$URL ERIS_LISTEN=127.0.0.1:15590 eris-server &
ERIS_DATABASE_URL=$URL ERIS_LISTEN=127.0.0.1:15591 eris-server &

# Cross-node write visibility
eris-migrate convergence --write http://127.0.0.1:15590 --read http://127.0.0.1:15591

# Differential vs the C++ oracle (full corpus)
eris-migrate corpus --sqlite external/e621.db --out corpus-full.jsonl
ORACLE_DATA=... docker compose -f deploy/compose.oracle.yml up -d  # full DB copy
eris-migrate diff --corpus corpus-full.jsonl \
  --oracle http://127.0.0.1:15588 --subject http://127.0.0.1:15590 --limit 20

# Load
eris-migrate bench --subject http://127.0.0.1:15590 --corpus corpus-full.jsonl \
  --concurrency 50 --duration-s 120

# Incremental (cutover) rehearsal: mutate a DB copy, delta-sync, watch nodes converge
eris-migrate import --incremental --sqlite mutated-copy.db --database-url $URL
```
