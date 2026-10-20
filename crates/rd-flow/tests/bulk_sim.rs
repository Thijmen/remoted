//! BulkWindow on a simulated bottleneck: a FIFO link with a fixed rate and propagation delay,
//! optionally shared with a constant-rate "screen stream". Checks that a transfer fills an idle
//! link and keeps the queue it causes near its target.

use std::collections::VecDeque;

use rd_flow::{BulkConfig, BulkWindow, TARGET_FULL_US, TARGET_GENTLE_US};

const MS: u64 = 1000;
const STEP_US: u64 = 100;

struct Link {
    /// Bytes per second.
    rate: f64,
    /// One-way propagation delay.
    delay_us: u64,
    /// (bytes left, seq or 0 for cross traffic)
    queue: VecDeque<(f64, u32)>,
    queued: f64,
}

struct Outcome {
    /// Transfer bytes delivered per second, over the measured part.
    throughput: f64,
    /// Mean and 95th percentile queueing delay of the link, ms.
    queue_mean_ms: f64,
    queue_p95_ms: f64,
}

fn run(rate_mbps: f64, delay_ms: u64, target_us: u64, cross_mbps: f64, seconds: u64) -> Outcome {
    let mut link = Link {
        rate: rate_mbps * 1e6 / 8.0,
        delay_us: delay_ms * MS,
        queue: VecDeque::new(),
        queued: 0.0,
    };
    let mut w = BulkWindow::new(BulkConfig::default(), target_us);
    // (ack arrival time, seq)
    let mut acks: VecDeque<(u64, u32)> = VecDeque::new();
    let cross_frame = cross_mbps * 1e6 / 8.0 / 60.0;
    let mut next_frame_us = 0;
    let warmup_us = 10_000 * MS;
    let end_us = seconds * 1_000_000;
    let mut delivered = 0.0;
    let mut queue_samples = Vec::new();
    let mut t = 0;
    while t < end_us {
        while let Some(&(at, seq)) = acks.front() {
            if at > t {
                break;
            }
            acks.pop_front();
            w.on_ack(seq, t);
        }
        loop {
            let chunk = w.chunk_size();
            if !w.can_send(chunk) {
                break;
            }
            let seq = w.on_sent(chunk, t);
            link.queue.push_back((chunk as f64, seq));
            link.queued += chunk as f64;
        }
        if cross_frame > 0.0 && t >= next_frame_us {
            link.queue.push_back((cross_frame, 0));
            link.queued += cross_frame;
            next_frame_us += 16_667;
        }
        // Serve the link for one step.
        let mut budget = link.rate * STEP_US as f64 / 1e6;
        while budget > 0.0 {
            let Some(front) = link.queue.front_mut() else {
                break;
            };
            let served = front.0.min(budget);
            front.0 -= served;
            budget -= served;
            link.queued -= served;
            if t >= warmup_us && front.1 != 0 {
                delivered += served;
            }
            if front.0 <= 0.0 {
                let (_, seq) = link.queue.pop_front().unwrap();
                if seq != 0 {
                    acks.push_back((t + 2 * link.delay_us, seq));
                }
            }
        }
        if t >= warmup_us && t % MS == 0 {
            queue_samples.push(link.queued / link.rate * 1e3);
        }
        t += STEP_US;
    }
    queue_samples.sort_by(f64::total_cmp);
    Outcome {
        throughput: delivered / ((end_us - warmup_us) as f64 / 1e6),
        queue_mean_ms: queue_samples.iter().sum::<f64>() / queue_samples.len() as f64,
        queue_p95_ms: queue_samples[queue_samples.len() * 95 / 100],
    }
}

#[test]
fn fills_an_idle_link_with_a_bounded_queue() {
    for (mbps, delay) in [(10.0, 20), (50.0, 30), (200.0, 5)] {
        let o = run(mbps, delay, TARGET_FULL_US, 0.0, 40);
        eprintln!(
            "{mbps} Mbps/{delay} ms: {:.1} Mbps, queue mean {:.1} p95 {:.1} ms",
            o.throughput * 8.0 / 1e6,
            o.queue_mean_ms,
            o.queue_p95_ms
        );
        let share = o.throughput / (mbps * 1e6 / 8.0);
        assert!(
            share > 0.9,
            "{mbps} Mbps/{delay} ms: only {share:.2} of the link"
        );
        assert!(
            o.queue_p95_ms < 2.0 * TARGET_FULL_US as f64 / 1e3,
            "{mbps} Mbps/{delay} ms: queue p95 {:.1} ms",
            o.queue_p95_ms
        );
    }
}

#[test]
fn leaves_the_screen_stream_a_short_queue() {
    // 20 Mbps uplink, 25 ms one way, a 6 Mbps screen stream next to the transfer.
    let o = run(20.0, 25, TARGET_GENTLE_US, 6.0, 40);
    eprintln!(
        "gentle: {:.1} Mbps, queue mean {:.1} p95 {:.1} ms",
        o.throughput * 8.0 / 1e6,
        o.queue_mean_ms,
        o.queue_p95_ms
    );
    assert!(
        o.queue_mean_ms < 25.0 && o.queue_p95_ms < 40.0,
        "queue mean {:.1} ms, p95 {:.1} ms",
        o.queue_mean_ms,
        o.queue_p95_ms
    );
    let rest = (20.0 - 6.0) * 1e6 / 8.0;
    assert!(
        o.throughput > 0.75 * rest,
        "the transfer got {:.0}% of what the stream leaves",
        o.throughput / rest * 100.0
    );
}
