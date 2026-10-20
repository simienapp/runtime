use super::*;

#[derive(Clone, serde::Serialize)]
pub(super) struct BridgeProtoMetric {
    calls: u64,
    total_req_bytes: u64,
    total_resp_bytes: u64,
    total_us: u64,
    avg_us: u64,
}

static BRIDGE_PROTO_METRICS: OnceLock<Mutex<HashMap<String, BridgeProtoMetric>>> = OnceLock::new();

fn bridge_proto_metrics() -> &'static Mutex<HashMap<String, BridgeProtoMetric>> {
    BRIDGE_PROTO_METRICS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn record_bridge_proto_metric(
    kind: &str,
    req_len: usize,
    resp_len: usize,
    elapsed_us: u64,
) {
    if let Ok(mut metrics) = bridge_proto_metrics().lock() {
        let metric = metrics
            .entry(kind.to_string())
            .or_insert(BridgeProtoMetric {
                calls: 0,
                total_req_bytes: 0,
                total_resp_bytes: 0,
                total_us: 0,
                avg_us: 0,
            });
        metric.calls += 1;
        metric.total_req_bytes = metric.total_req_bytes.saturating_add(req_len as u64);
        metric.total_resp_bytes = metric.total_resp_bytes.saturating_add(resp_len as u64);
        metric.total_us = metric.total_us.saturating_add(elapsed_us);
        metric.avg_us = if metric.calls == 0 {
            0
        } else {
            metric.total_us / metric.calls
        };
    }
}

#[op2]
#[serde]
pub(super) fn op_php_bridge_proto_stats() -> Result<serde_json::Value, simien_core::error::CoreError>
{
    let metrics = bridge_proto_metrics()
        .lock()
        .map_err(|_| core_err("bridge proto metrics lock poisoned"))?;
    let mut out = serde_json::Map::new();
    for (key, metric) in metrics.iter() {
        out.insert(
            key.clone(),
            serde_json::json!({
                "calls": metric.calls,
                "total_req_bytes": metric.total_req_bytes,
                "total_resp_bytes": metric.total_resp_bytes,
                "total_us": metric.total_us,
                "avg_us": metric.avg_us,
            }),
        );
    }
    Ok(serde_json::Value::Object(out))
}
