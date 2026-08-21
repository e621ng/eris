//! Full-stack API tests: real Postgres (scratch database per test), a real
//! server bound to an ephemeral port, request bodies shaped byte-for-byte
//! like the ones `IqdbProxy` produces. Skipped unless DATABASE_URL is set.

use std::time::Duration;

use eris_server::config::Config;
use eris_server::lifecycle;
use serde_json::{json, Value};

fn test_config(database_url: String) -> Config {
  Config {
    database_url,
    listen: "127.0.0.1:0".into(),
    token: None,
    feed_interval_ms: 100,
    ready_max_lag_s: 30.0,
    body_limit: 1024 * 1024,
    db_max_conns: 5,
    migrate: true,
    prune_interval_s: 3600,
    event_retention_s: 7 * 24 * 3600,
  }
}

/// Create a scratch database and return its URL; None (skip) without
/// DATABASE_URL.
async fn scratch_db(name: &str) -> Option<String> {
  let Ok(url) = std::env::var("DATABASE_URL") else {
    eprintln!("skipping {name}: DATABASE_URL not set");
    return None;
  };
  let admin = sqlx::PgPool::connect(&url).await.expect("connect admin");
  let db = format!("eris_api_test_{name}");
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
  Some(format!("{base}/{db}"))
}

/// Boot a full server against the given database; returns its base URL.
async fn boot(config: Config) -> String {
  let started = lifecycle::start(&config).await.expect("server start");
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  tokio::spawn(async move {
    axum::serve(listener, started.router).await.unwrap();
  });
  format!("http://{addr}")
}

/// Deterministic channel arrays, shaped exactly like
/// `IqdbProxy.get_channels_data` output: plain integer arrays of length 16384.
fn channels_json(seed: u32) -> Value {
  let mut state = seed.wrapping_mul(2654435761).wrapping_add(3);
  let mut chan = || {
    let mut v = Vec::with_capacity(16384);
    for _ in 0..16384 {
      state = state.wrapping_mul(1664525).wrapping_add(1013904223);
      v.push(Value::from((state >> 24) as u8));
    }
    Value::Array(v)
  };
  json!({ "r": chan(), "g": chan(), "b": chan() })
}

async fn poll_until<F: Fn() -> reqwest::RequestBuilder>(
  request: F,
  accept: impl Fn(&reqwest::StatusCode) -> bool,
  what: &str,
) -> reqwest::Response {
  let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
  loop {
    let response = request().send().await.expect("request");
    if accept(&response.status()) {
      return response;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "timed out waiting for {what}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

#[tokio::test]
async fn iqdb_proxy_request_flow() {
  let Some(db_url) = scratch_db("flow").await else {
    return;
  };
  let base = boot(test_config(db_url)).await;
  let client = reqwest::Client::new();

  // POST /images/123 with an IqdbProxy-shaped body.
  let body = json!({ "channels": channels_json(1) });
  let response = client
    .post(format!("{base}/images/123"))
    .header("content-type", "application/json")
    .body(body.to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 200);
  let added: Value = response.json().await.unwrap();
  assert_eq!(added["post_id"], 123);
  let hash = added["hash"].as_str().expect("hash in add response");
  assert_eq!(hash.len(), 528);
  assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));

  // The write becomes visible through the event feed.
  let response = poll_until(
    || client.get(format!("{base}/images/123")),
    |s| s.as_u16() == 200,
    "feed to apply the add",
  )
  .await;
  let got: Value = response.json().await.unwrap();
  assert_eq!(got["hash"].as_str().unwrap(), hash);

  // Query by hash: the post matches itself at score 100.
  let response = client
    .post(format!("{base}/query"))
    .header("content-type", "application/json")
    .body(json!({ "hash": hash }).to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 200);
  let matches: Value = response.json().await.unwrap();
  let arr = matches.as_array().unwrap();
  assert_eq!(arr.len(), 1);
  assert_eq!(arr[0]["post_id"], 123);
  assert!((arr[0]["score"].as_f64().unwrap() - 100.0).abs() < 0.01);
  assert_eq!(arr[0]["hash"].as_str().unwrap(), hash);

  // Query by channels (the exact same pixel data) finds the same result.
  let response = client
    .post(format!("{base}/query"))
    .header("content-type", "application/json")
    .body(json!({ "channels": channels_json(1) }).to_string())
    .send()
    .await
    .unwrap();
  let matches: Value = response.json().await.unwrap();
  assert_eq!(matches.as_array().unwrap()[0]["post_id"], 123);

  // Query by post id (the new endpoint).
  let response = client
    .post(format!("{base}/query"))
    .header("content-type", "application/json")
    .body(json!({ "post_id": 123 }).to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 200);
  let matches: Value = response.json().await.unwrap();
  assert_eq!(matches.as_array().unwrap()[0]["post_id"], 123);

  // Unindexed post id: 404 not_indexed.
  let response = client
    .post(format!("{base}/query"))
    .header("content-type", "application/json")
    .body(json!({ "post_id": 999 }).to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 404);
  let body: Value = response.json().await.unwrap();
  assert_eq!(body["error"], "not_indexed");

  // min_score filters server-side.
  let response = client
    .post(format!("{base}/query"))
    .header("content-type", "application/json")
    .body(json!({ "hash": hash, "min_score": 101 }).to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(
    response
      .json::<Value>()
      .await
      .unwrap()
      .as_array()
      .unwrap()
      .len(),
    0
  );

  // DELETE returns 200 whether or not the post exists (C++ parity;
  // IqdbRemoveJob raises on non-200).
  let response = client
    .delete(format!("{base}/images/123"))
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 200);
  assert_eq!(response.json::<Value>().await.unwrap()["post_id"], 123);
  let response = client
    .delete(format!("{base}/images/123"))
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 200, "absent delete must still be 200");

  // The delete propagates: GET goes 404.
  poll_until(
    || client.get(format!("{base}/images/123")),
    |s| s.as_u16() == 404,
    "feed to apply the delete",
  )
  .await;

  // /status keeps the C++ "images" key.
  let status: Value = client
    .get(format!("{base}/status"))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
  assert_eq!(status["images"], 0);
  assert_eq!(status["tombstones"], 1);
  assert_eq!(status["ready"], true);

  // /healthz, /ready, /metrics respond.
  assert_eq!(
    client
      .get(format!("{base}/healthz"))
      .send()
      .await
      .unwrap()
      .status(),
    200
  );
  assert_eq!(
    client
      .get(format!("{base}/ready"))
      .send()
      .await
      .unwrap()
      .status(),
    200
  );
  let metrics_body = client
    .get(format!("{base}/metrics"))
    .send()
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
  assert!(metrics_body.contains("eris_query_duration_seconds"));
}

#[tokio::test]
async fn validation_rejects_bad_input() {
  let Some(db_url) = scratch_db("validation").await else {
    return;
  };
  let mut config = test_config(db_url);
  config.body_limit = 2000; // small cap to exercise 413 cheaply
  let base = boot(config).await;
  let client = reqwest::Client::new();
  let post = |path: &str, body: String| {
    client
      .post(format!("{base}{path}"))
      .header("content-type", "application/json")
      .body(body)
  };

  // Wrong channel length.
  let short = json!({ "channels": { "r": [1, 2, 3], "g": [1, 2, 3], "b": [1, 2, 3] } });
  let response = post("/query", short.to_string()).send().await.unwrap();
  assert_eq!(response.status(), 400);

  // Bad hash: wrong length, then non-hex at the right length.
  let response = post("/query", json!({ "hash": "abc" }).to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 400);
  let response = post("/query", json!({ "hash": "g".repeat(528) }).to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 400);

  // Non-positive limit.
  let response = post(
    "/query",
    json!({ "hash": "0".repeat(528), "limit": 0 }).to_string(),
  )
  .send()
  .await
  .unwrap();
  assert_eq!(response.status(), 400);

  // Neither hash nor channels nor post_id.
  let response = post("/query", json!({}).to_string()).send().await.unwrap();
  assert_eq!(response.status(), 400);

  // Body over the configured limit -> 413.
  let response = post("/query", format!("{{\"hash\": \"{}\"}}", "0".repeat(3000)))
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 413);

  // Post id beyond i32 (id is checked before the channel payload).
  let tiny = json!({ "channels": { "r": [], "g": [], "b": [] } });
  let response = post("/images/3000000000", tiny.to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn bearer_auth_when_token_configured() {
  let Some(db_url) = scratch_db("auth").await else {
    return;
  };
  let mut config = test_config(db_url);
  config.token = Some("sekrit".into());
  let base = boot(config).await;
  let client = reqwest::Client::new();

  let response = client.get(format!("{base}/status")).send().await.unwrap();
  assert_eq!(response.status(), 401);
  let response = client
    .get(format!("{base}/status"))
    .bearer_auth("wrong")
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 401);
  let response = client
    .get(format!("{base}/status"))
    .bearer_auth("sekrit")
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 200);

  // Probes stay open for infrastructure.
  for path in ["/healthz", "/ready", "/metrics"] {
    let response = client.get(format!("{base}{path}")).send().await.unwrap();
    assert_eq!(response.status(), 200, "{path} must be exempt from auth");
  }
}

#[tokio::test]
async fn two_servers_converge_through_one_database() {
  let Some(db_url) = scratch_db("twonode").await else {
    return;
  };
  let node_a = boot(test_config(db_url.clone())).await;
  let node_b = boot(test_config(db_url)).await;
  let client = reqwest::Client::new();

  // Write through node A...
  let response = client
    .post(format!("{node_a}/images/555"))
    .header("content-type", "application/json")
    .body(json!({ "channels": channels_json(42) }).to_string())
    .send()
    .await
    .unwrap();
  assert_eq!(response.status(), 200);
  let hash = response.json::<Value>().await.unwrap()["hash"]
    .as_str()
    .unwrap()
    .to_owned();

  // ...and observe it on node B (and A) within the convergence budget.
  for node in [&node_a, &node_b] {
    let response = poll_until(
      || client.get(format!("{node}/images/555")),
      |s| s.as_u16() == 200,
      "cross-node convergence",
    )
    .await;
    assert_eq!(
      response.json::<Value>().await.unwrap()["hash"]
        .as_str()
        .unwrap(),
      hash
    );
  }

  // Delete through node B; node A converges.
  assert_eq!(
    client
      .delete(format!("{node_b}/images/555"))
      .send()
      .await
      .unwrap()
      .status(),
    200
  );
  poll_until(
    || client.get(format!("{node_a}/images/555")),
    |s| s.as_u16() == 404,
    "cross-node delete convergence",
  )
  .await;
}
