//! B1 review #8 follow-up: the counters an alert reads are on /metrics from boot (registered at 0 when the recorder is
//! installed), not only after their first increment. Its own binary (a fresh process): nothing else has touched the
//! recorder, so the series can only come from boot registration. No database.
use agent_api::observability;

/// Every counter an alert reads is on /metrics from boot (at 0), not only after its first increment: an absent
/// series makes `increase()` read as healthy until the first incident.
#[test]
fn alerting_counters_are_registered_at_boot() {
    let text = observability::metrics_handle().render();
    for (name, labels) in observability::ZERO_COUNTERS {
        let want = if labels.is_empty() {
            format!("{name} ")
        } else {
            let l: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
            format!("{name}{{{}}} ", l.join(","))
        };
        let line = format!("{want}0");
        assert!(
            text.lines().any(|l| l == line),
            "{want:?} is not on /metrics at 0:\n{text}"
        );
    }
    assert!(
        text.lines()
            .any(|l| l == "agent_license_orphan_unrevoked_total 0"),
        "{text}"
    );
}
