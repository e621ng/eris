# ERIS: E621 Reverse Image Search

Rust + Postgres drop-in replacement for the IQDB service.  
N identical, disk-stateless nodes serve similarity queries from an in-RAM
index, bootstrap from Postgres, and stay converged by tailing a trigger-fed event log.

## How it works

```
 e621ng app servers            ERIS nodes (N >= 2, identical)     PostgreSQL
┌──────────────────┐   HTTP   ┌─────────────────────────────┐   ┌──────────────┐
│ IqdbProxy        │─────────▶│ axum HTTP API               │──▶│ images       │
│ (libvips decode) │    LB    │ in-RAM chunked index        │◀──│ image_events │
└──────────────────┘          │ feed follower (poll+NOTIFY) │   └──────────────┘
                              └─────────────────────────────┘
```

- **Clients decode images.** ERIS never parses image files: callers resize to
  128x128 and POST raw RGB channel arrays (or a precomputed hash). One
  resampler (libvips, in the app) produces every signature.
- **Any node accepts writes.** A write is a Postgres transaction; a trigger
  appends to `image_events` under an advisory xact lock (so event order ==
  commit order) and NOTIFYs. Every node – including the writer – applies the
  change via the feed. Visibility is eventual, bounded by the poll interval
  (default 2s).
- **Postgres down** – queries keep serving from RAM, writes 503, `/ready`
  flips once the feed staleness exceeds the threshold.
- The Haar signature math is frozen and bit-identical to the C++ IQDB
  (verified by golden fixtures generated from the original binary and a
  differential harness; see `crates/eris-migrate`).

## API

All bodies are JSON. With `ERIS_TOKEN` set, every endpoint except
`/healthz`, `/ready`, and `/metrics` requires `Authorization: Bearer <token>`;
with no token configured, auth is disabled.

| Endpoint | Body | Response |
|---|---|---|
| `POST /images/{id}` | `{"channels": {"r": [...], "g": [...], "b": [...]}}` – 16,384 ints (0-255) per channel | `{"post_id": N, "hash": "..."}` |
| `DELETE /images/{id}` | – | `{"post_id": N}` – **200 even if absent** |
| `GET /images/{id}` | – | `{"post_id": N, "hash": "..."}` or 404 |
| `POST /query` | one of `{"hash": "..."}` \| `{"channels": {...}}` \| `{"post_id": N}`; optional `"limit"` (default 10, max 200), `"min_score"` | `[{"post_id": N, "score": 87.3, "hash": "..."}]`, best first |
| `GET /status` | – | image count, tombstones, feed cursor/lag, version |
| `GET /metrics` | – | Prometheus text format |
| `GET /healthz` / `GET /ready` | – | liveness / load-balancer readiness gate |

The `hash` is 528 hex chars: 3x16 hex of the raw f64 bits of the YIQ DC
components, then 120x4 hex of the i16 coefficient indices – identical to the
C++ format.

### Deliberate deviations from the C++ service

- **Pure-black images are findable.** The C++ conflated "deleted" with
  "avgl[0] == 0"; ERIS uses explicit tombstones.
- **Validation errors are 400s** with a message, instead of silent value
  wrapping or blanket 500s. Channel arrays must be exactly 16,384 integers in
  0..=255; `limit` is clamped to 200 and rejected when <= 0.
- `POST /query {"post_id": N}` is new; an unindexed id returns
  404 `{"error": "not_indexed"}`.
- Responses are compact JSON (not pretty-printed).

## Configuration

Flags or `ERIS_*` environment variables (see `eris-server --help`):

| Variable | Default | Purpose |
|---|---|---|
| `ERIS_DATABASE_URL` | – (required) | Postgres URL |
| `ERIS_LISTEN` | `0.0.0.0:5588` | bind address |
| `ERIS_TOKEN` | unset (auth off) | bearer token |
| `ERIS_FEED_INTERVAL_MS` | `2000` | event poll interval |
| `ERIS_READY_MAX_LAG_S` | `30` | `/ready` unready above this feed lag/staleness |
| `ERIS_BODY_LIMIT` | `1048576` | request body cap (bytes) |
| `ERIS_DB_MAX_CONNS` | `10` | connection pool size |
| `ERIS_MIGRATE` | `true` | run schema migrations at startup |
| `ERIS_EVENT_RETENTION_S` | 7 days | event log retention |

Sizing: at 6.6M images a node uses ~4.1 GiB RSS and bootstraps in well under
a minute; give containers headroom above steady-state for the bootstrap spike
(a 6 GiB limit is comfortable). Nodes hold no state on disk.

## Operating

- **Deploy**: container image (`ghcr.io/e621ng/eris`), one process per node,
  any number of nodes behind a load balancer that gates on `/ready`.
- **Rolling restarts**: recycle one node at a time, waiting for `/ready`
  (bootstrap re-reads the full table from Postgres).
- **Tombstones**: replaced/deleted images leave index tombstones until the
  next (re-)bootstrap; `/status` reports the ratio. A rolling restart is the
  compaction mechanism; alert if the ratio approaches 10%.
- **Event log**: pruned automatically after the retention window. A node
  offline longer than retention re-bootstraps automatically on its next poll.
- **Never expose the port** to untrusted networks; the bearer token is
  defense in depth, not a substitute for firewalling.

## Migration from IQDB

`eris-migrate` (in the image, or `cargo build -p eris-migrate`) covers the
whole cutover; see [deploy/README.md](deploy/README.md) for the full runbook:

```bash
# One-time full import (suppresses the event trigger; ~20s for 6.6M rows).
eris-migrate import --sqlite e621.db --database-url $ERIS_DATABASE_URL
eris-migrate verify --sqlite e621.db --database-url $ERIS_DATABASE_URL

# At cutover: pause the iqdb Sidekiq queue, then sync the delta accumulated
# since the snapshot (trigger active: running nodes converge on it).
eris-migrate import --incremental --sqlite e621-fresh-copy.db --database-url $ERIS_DATABASE_URL
# ...flip e621ng's iqdb_server config to the ERIS LB and resume the queue.
```

Verification tooling: `stats` (index memory/latency from a SQLite snapshot),
`corpus`/`subsample` (test-data generation), `fixtures` (golden vectors from
the C++ oracle), `diff` (differential run: ERIS vs C++, score parity to
0.01), `bench`/`convergence` (load and write-visibility measurement).

## Development

```bash
docker compose -f deploy/compose.dev.yml up -d          # Postgres 15
cargo test --workspace                                  # unit + property tests
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:15432/eris \
  cargo test --workspace                                # + integration tests
```

The differential harness needs the C++ oracle:
`docker compose -f deploy/compose.oracle.yml build` (uses `external/iqdb`).

## License

GPL-2.0. The Haar transform in `eris-core` is adapted from
[iqdb-rs](https://github.com/TheBobBobs/iqdb-rs) (GPL-2.0), itself derived
from the imgSeek/IQDB lineage by Ricardo Niederberger Cabral and piespy.
