use crate::config::{ConnectionStrategy, MAX_CONNECTIONS, default_connections};

const MIB: u64 = 1024 * 1024;
const MIN_SEGMENT_SIZE: u64 = 2 * MIB;
const TARGET_SEGMENTS_PER_WORKER: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TransferPlan {
    pub(super) strategy: ConnectionStrategy,
    pub(super) workers: usize,
    pub(super) segments: usize,
    pub(super) segment_size: u64,
}

impl TransferPlan {
    pub(super) fn description(&self) -> String {
        format!(
            "{} strategy: {} workers, {} segments of about {:.1} MiB",
            match self.strategy {
                ConnectionStrategy::Auto => "Auto",
                ConnectionStrategy::Fixed(_) => "Fixed",
            },
            self.workers,
            self.segments,
            (self.segment_size as f64 / MIB as f64).max(0.1)
        )
    }
}

pub(super) fn plan_transfer(total_size: u64, strategy: ConnectionStrategy) -> TransferPlan {
    let workers = match strategy {
        ConnectionStrategy::Auto => auto_workers(total_size),
        ConnectionStrategy::Fixed(connections) => connections.clamp(1, MAX_CONNECTIONS),
    }
    .min(total_size.max(1) as usize);

    let target_segments = workers
        .saturating_mul(TARGET_SEGMENTS_PER_WORKER)
        .max(workers);
    let max_useful_segments = total_size
        .div_ceil(MIN_SEGMENT_SIZE)
        .max(workers as u64)
        .min(usize::MAX as u64) as usize;
    let segments = target_segments.min(max_useful_segments).max(1);
    let segment_size = total_size.div_ceil(segments as u64);

    TransferPlan {
        strategy,
        workers,
        segments,
        segment_size,
    }
}

fn auto_workers(total_size: u64) -> usize {
    let baseline = default_connections();
    let by_size = match total_size {
        0..=1_048_576 => 1,
        1_048_577..=8_388_608 => 4,
        8_388_609..=67_108_864 => 8,
        67_108_865..=536_870_912 => 16,
        _ => 32,
    };

    by_size.max(baseline).min(MAX_CONNECTIONS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_scales_workers_with_size() {
        let small = plan_transfer(512 * 1024, ConnectionStrategy::Auto);
        let large = plan_transfer(700 * MIB, ConnectionStrategy::Auto);

        assert!(small.workers >= 1);
        assert!(large.workers >= small.workers);
        assert!(large.workers <= MAX_CONNECTIONS);
    }

    #[test]
    fn fixed_strategy_limits_workers_but_keeps_extra_segments() {
        let plan = plan_transfer(128 * MIB, ConnectionStrategy::Fixed(4));

        assert_eq!(plan.workers, 4);
        assert!(plan.segments > plan.workers);
    }
}
