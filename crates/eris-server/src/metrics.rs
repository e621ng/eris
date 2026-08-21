use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install (once per process) and return the Prometheus recorder handle.
pub fn handle() -> PrometheusHandle {
  HANDLE
    .get_or_init(|| {
      PrometheusBuilder::new()
        .install_recorder()
        .expect("install prometheus recorder")
    })
    .clone()
}

pub async fn render() -> String {
  handle().render()
}
