//! Integration tests against a real Postgres. Skipped (with a note) unless
//! DATABASE_URL is set; each test creates its own scratch database so the
//! suite is parallel-safe.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eris_core::{HaarSignature, Index, NUM_COEFS};
use eris_store::bootstrap::bootstrap;
use eris_store::events::{fetch_events, EventOp, ImageEvent};
use eris_store::feed::{FeedExit, FeedStatus, Follower};
use eris_store::prune::prune_events;
use eris_store::writes::{delete_image, upsert_image};
use sqlx::PgPool;

/// Deterministic valid signature for tests.
fn test_sig(seed: u32) -> HaarSignature {
  let mut sig = [[0i16; NUM_COEFS]; 3];
  let mut state = seed.wrapping_mul(2654435761).wrapping_add(7);
  for channel in sig.iter_mut() {
    let mut used = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < NUM_COEFS {
      state = state.wrapping_mul(1664525).wrapping_add(1013904223);
      let idx = 1 + ((state >> 16) % 16383) as i16;
      let value = if state & 1 == 0 { idx } else { -idx };
      if used.insert(value) {
        channel[i] = value;
        i += 1;
      }
    }
    channel.sort_unstable();
  }
  HaarSignature {
    avglf: [0.25 + seed as f64 * 1e-5, 0.04, -0.03],
    sig,
  }
}

/// Create a fresh scratch database named after the test and return a pool
/// bound to it, with migrations applied. None (skip) when DATABASE_URL unset.
async fn test_pool(name: &str) -> Option<PgPool> {
  let Ok(url) = std::env::var("DATABASE_URL") else {
    eprintln!("skipping {name}: DATABASE_URL not set");
    return None;
  };
  let admin = PgPool::connect(&url).await.expect("connect admin");
  let db = format!("eris_test_{name}");
  sqlx::query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
    .execute(&admin)
    .await
    .expect("drop scratch db");
  sqlx::query(&format!("CREATE DATABASE {db}"))
    .execute(&admin)
    .await
    .expect("create scratch db");
  admin.close().await;

  let (base, _) = url.rsplit_once('/').expect("URL with database path");
  let pool = eris_store::connect(&format!("{base}/{db}"), 5)
    .await
    .expect("connect scratch db");
  eris_store::migrate(&pool).await.expect("migrate");
  Some(pool)
}

fn apply_events(index: &mut Index, events: Vec<ImageEvent>) {
  for event in events {
    match event.op {
      EventOp::Upsert => index.insert(
        event.post_id as u32,
        event.sig.expect("upsert event carries a signature"),
      ),
      EventOp::Delete => {
        index.remove(event.post_id as u32);
      }
    }
  }
}

#[tokio::test]
async fn trigger_records_events_with_payload() {
  let Some(pool) = test_pool("trigger").await else {
    return;
  };

  let sig_a = test_sig(1);
  let sig_b = test_sig(2);
  upsert_image(&pool, 100, &sig_a).await.unwrap();
  upsert_image(&pool, 100, &sig_b).await.unwrap(); // update path
  assert!(delete_image(&pool, 100).await.unwrap());
  assert!(!delete_image(&pool, 100).await.unwrap()); // absent: no-op

  let events = fetch_events(&pool, 0, 100).await.unwrap();
  assert_eq!(events.len(), 3, "absent delete must not produce an event");
  assert_eq!(events[0].op, EventOp::Upsert);
  assert_eq!(events[0].sig.as_ref().unwrap(), &sig_a);
  assert_eq!(events[1].op, EventOp::Upsert);
  assert_eq!(events[1].sig.as_ref().unwrap(), &sig_b);
  assert_eq!(events[2].op, EventOp::Delete);
  assert_eq!(events[2].post_id, 100);
  assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
}

#[tokio::test]
async fn trigger_suppression_is_session_scoped() {
  let Some(pool) = test_pool("suppress").await else {
    return;
  };

  // Suppress on one dedicated connection; writes there produce no events.
  let mut conn = pool.acquire().await.unwrap();
  sqlx::query("SELECT set_config('eris.skip_events', 'on', false)")
    .execute(&mut *conn)
    .await
    .unwrap();
  upsert_image(&mut *conn, 1, &test_sig(1)).await.unwrap();
  upsert_image(&mut *conn, 2, &test_sig(2)).await.unwrap();
  assert_eq!(fetch_events(&pool, 0, 100).await.unwrap().len(), 0);
  drop(conn);

  // A fresh session (unset flag) fires the trigger by default.
  upsert_image(&pool, 3, &test_sig(3)).await.unwrap();
  let events = fetch_events(&pool, 0, 100).await.unwrap();
  assert_eq!(events.len(), 1);
  assert_eq!(events[0].post_id, 3);
}

#[tokio::test]
async fn bootstrap_with_concurrent_writes_loses_nothing() {
  let Some(pool) = test_pool("bootrace").await else {
    return;
  };

  for i in 0..500 {
    upsert_image(&pool, i, &test_sig(i as u32)).await.unwrap();
  }

  // Hammer writes (new posts, replacements, deletes) while bootstrapping.
  let writer_pool = pool.clone();
  let writer = tokio::spawn(async move {
    for i in 0..200 {
      upsert_image(&writer_pool, 1000 + i, &test_sig(5000 + i as u32))
        .await
        .unwrap();
      if i % 3 == 0 {
        upsert_image(&writer_pool, i, &test_sig(9000 + i as u32))
          .await
          .unwrap();
      }
      if i % 7 == 0 {
        delete_image(&writer_pool, i).await.unwrap();
      }
    }
  });

  let snapshot = bootstrap(&pool).await.unwrap();
  writer.await.unwrap();

  // Catch up from the snapshot cursor, then compare with ground truth.
  let mut index = snapshot.index;
  let events = fetch_events(&pool, snapshot.cursor, 100_000).await.unwrap();
  apply_events(&mut index, events);

  let truth: Vec<(i32, Vec<u8>)> =
    sqlx::query_as("SELECT post_id, sig FROM images ORDER BY post_id")
      .fetch_all(&pool)
      .await
      .unwrap();
  assert_eq!(index.len(), truth.len());
  for (post_id, blob) in truth {
    let sig = index.get(post_id as u32).expect("post missing from index");
    assert_eq!(sig.sig_blob().to_vec(), blob, "post {post_id} sig mismatch");
  }
}

#[tokio::test]
async fn two_followers_converge() {
  let Some(pool) = test_pool("converge").await else {
    return;
  };

  for i in 0..50 {
    upsert_image(&pool, i, &test_sig(i as u32)).await.unwrap();
  }

  let mut nodes = Vec::new();
  for _ in 0..2 {
    let snapshot = bootstrap(&pool).await.unwrap();
    let index = Arc::new(Mutex::new(snapshot.index));
    let mut follower = Follower::new(pool.clone(), snapshot.cursor, Duration::from_millis(200));
    let apply_index = Arc::clone(&index);
    let (status_tx, status_rx) = tokio::sync::watch::channel(FeedStatus {
      cursor: snapshot.cursor,
      lag_seconds: 0.0,
    });
    let handle = tokio::spawn(async move {
      follower
        .run(
          move |events| apply_events(&mut apply_index.lock().unwrap(), events),
          &status_tx,
        )
        .await
    });
    nodes.push((index, status_rx, handle));
  }

  // A burst of writes through the shared database.
  for i in 50..150 {
    upsert_image(&pool, i, &test_sig(i as u32)).await.unwrap();
  }
  for i in (0..50).step_by(5) {
    delete_image(&pool, i).await.unwrap();
  }

  let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
  let expected_len = 150 - 10;
  loop {
    let done = nodes.iter().all(|(index, _, _)| {
      let index = index.lock().unwrap();
      index.len() == expected_len && !index.contains(45) && index.contains(149)
    });
    if done {
      break;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "followers did not converge within 5s"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  // Horizon breach: force the horizon past the cursors; both followers must
  // exit asking for a re-bootstrap.
  sqlx::query("UPDATE feed_meta SET prune_horizon = 1000000")
    .execute(&pool)
    .await
    .unwrap();
  for (_, _, handle) in nodes {
    let exit = tokio::time::timeout(Duration::from_secs(5), handle)
      .await
      .expect("follower did not exit on horizon breach")
      .unwrap()
      .unwrap();
    assert_eq!(exit, FeedExit::HorizonBreached);
  }
}

#[tokio::test]
async fn prune_deletes_old_events_and_advances_horizon() {
  let Some(pool) = test_pool("prune").await else {
    return;
  };

  for i in 0..10 {
    upsert_image(&pool, i, &test_sig(i as u32)).await.unwrap();
  }
  // Backdate the first five events beyond the retention window.
  sqlx::query("UPDATE image_events SET at = now() - interval '8 days' WHERE seq <= 5")
    .execute(&pool)
    .await
    .unwrap();

  let deleted = prune_events(&pool, Duration::from_secs(7 * 24 * 3600))
    .await
    .unwrap()
    .expect("lock uncontended");
  assert_eq!(deleted, 5);

  let horizon: i64 = sqlx::query_scalar("SELECT prune_horizon FROM feed_meta")
    .fetch_one(&pool)
    .await
    .unwrap();
  assert_eq!(horizon, 5);

  let remaining = fetch_events(&pool, 0, 100).await.unwrap();
  assert_eq!(remaining.len(), 5);
  assert!(remaining.iter().all(|e| e.seq > 5));

  // Nothing else old: second run is a no-op.
  let deleted = prune_events(&pool, Duration::from_secs(7 * 24 * 3600))
    .await
    .unwrap()
    .unwrap();
  assert_eq!(deleted, 0);
}
