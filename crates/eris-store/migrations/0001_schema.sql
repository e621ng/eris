-- Current state: one row per indexed post.
CREATE TABLE images (
  post_id    integer PRIMARY KEY,
  avglf1     double precision NOT NULL,
  avglf2     double precision NOT NULL,
  avglf3     double precision NOT NULL,
  -- int16[3][40], little-endian: the C++-compatible signature blob.
  sig        bytea NOT NULL CHECK (octet_length(sig) = 240),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Append-only change feed. Upsert events carry the full payload so followers
-- never need to join back to `images`; delete events carry only the post_id.
CREATE TABLE image_events (
  seq     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  op      smallint NOT NULL CHECK (op IN (1, 2)),  -- 1 = upsert, 2 = delete
  post_id integer NOT NULL,
  avglf1  double precision,
  avglf2  double precision,
  avglf3  double precision,
  sig     bytea CHECK (sig IS NULL OR octet_length(sig) = 240),
  at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX image_events_at_idx ON image_events (at);

-- Single row: events with seq <= prune_horizon have been deleted; a follower
-- whose cursor is below the horizon must re-bootstrap.
CREATE TABLE feed_meta (
  prune_horizon bigint NOT NULL
);
INSERT INTO feed_meta (prune_horizon) VALUES (0);
