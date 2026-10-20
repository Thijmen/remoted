//! Prometheus text metrics served on the metrics port.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub sessions_started: Counter,
    pub sessions_active: Gauge,
    pub bytes_down: Counter,
    pub bytes_up: Counter,
    pub agent_auth_failures: Counter,
    pub viewer_auth_failures: Counter,
    pub agent_registrations: Counter,
    pub agent_updates_requested: Counter,
    pub probe_runs: Counter,
    pub files_started: Counter,
    pub files_active: Gauge,
    pub files_bytes_down: Counter,
    pub files_bytes_up: Counter,
}

impl Metrics {
    pub fn render(&self, agents_online: usize) -> String {
        let mut out = String::new();
        let mut put = |name: &str, kind: &str, help: &str, value: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
            );
        };
        put(
            "rd_agents_online",
            "gauge",
            "Agents with a registered control socket.",
            agents_online as u64,
        );
        put(
            "rd_sessions_active",
            "gauge",
            "Joined viewer sessions.",
            self.sessions_active.get(),
        );
        put(
            "rd_sessions_started_total",
            "counter",
            "Viewer sessions requested.",
            self.sessions_started.get(),
        );
        put(
            "rd_relay_bytes_down_total",
            "counter",
            "Payload bytes relayed agent to viewer.",
            self.bytes_down.get(),
        );
        put(
            "rd_relay_bytes_up_total",
            "counter",
            "Payload bytes relayed viewer to agent.",
            self.bytes_up.get(),
        );
        put(
            "rd_files_channels_active",
            "gauge",
            "Joined file channels.",
            self.files_active.get(),
        );
        put(
            "rd_files_channels_started_total",
            "counter",
            "File channels requested.",
            self.files_started.get(),
        );
        put(
            "rd_files_bytes_down_total",
            "counter",
            "Payload bytes relayed agent to viewer on file channels.",
            self.files_bytes_down.get(),
        );
        put(
            "rd_files_bytes_up_total",
            "counter",
            "Payload bytes relayed viewer to agent on file channels.",
            self.files_bytes_up.get(),
        );
        put(
            "rd_agent_auth_failures_total",
            "counter",
            "Rejected agent authentications.",
            self.agent_auth_failures.get(),
        );
        put(
            "rd_viewer_auth_failures_total",
            "counter",
            "Rejected viewer requests (403).",
            self.viewer_auth_failures.get(),
        );
        put(
            "rd_agent_registrations_total",
            "counter",
            "Agent control socket registrations.",
            self.agent_registrations.get(),
        );
        put(
            "rd_agent_updates_requested_total",
            "counter",
            "Agent updates requested from the portal (sent to the agent).",
            self.agent_updates_requested.get(),
        );
        put(
            "rd_probe_runs_total",
            "counter",
            "Probe test runs started.",
            self.probe_runs.get(),
        );
        out
    }
}
