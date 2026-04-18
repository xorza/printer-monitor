use crate::obico::Detection;

const EWM_ALPHA: f64 = 2.0 / (12.0 + 1.0); // span=12, α=0.1538
const BASELINE_WINDOW: u64 = 7200; // ~20 hours at 10s
const GRACE_FRAMES: u64 = 10; // ~100s at POLL_TARGET=10s
const THRESHOLD_WARNING: f64 = 0.35;
const THRESHOLD_FAILING: f64 = 0.55;

#[derive(Debug, PartialEq)]
pub enum DetectionResult {
    Safe,
    Warning { score: f64 },
    Failing { score: f64 },
}

#[derive(Debug)]
pub struct DetectionState {
    ewm_mean: f64,
    baseline_mean: f64,
    frame_count: u64,
    lifetime_frame_count: u64,
    current_job_id: Option<u64>,
    sensitivity: f64,
}

impl DetectionState {
    pub fn new(sensitivity: f64) -> Self {
        Self {
            ewm_mean: 0.0,
            baseline_mean: 0.0,
            frame_count: 0,
            lifetime_frame_count: 0,
            current_job_id: None,
            sensitivity,
        }
    }

    pub fn current_score(&self) -> f64 {
        ((self.ewm_mean - self.baseline_mean) * self.sensitivity).max(0.0)
    }

    /// Reset per-print state (EWM, frame count) so the next print starts clean.
    /// `baseline_mean` and `lifetime_frame_count` are preserved — they track
    /// the baseline noise floor across all prints.
    pub fn reset_per_print(&mut self) {
        self.ewm_mean = 0.0;
        self.frame_count = 0;
    }

    /// Update detection state with new detections.
    /// `job_id` comes from PrusaLink when available; triggers reset on job change.
    pub fn update(&mut self, detections: &[Detection], job_id: Option<u64>) -> DetectionResult {
        // Handle job ID changes (PrusaLink mode)
        if let Some(id) = job_id
            && self.current_job_id != Some(id)
        {
            self.reset_per_print();
            self.current_job_id = Some(id);
        }

        // Compute raw p = sum of detection confidences
        let p: f64 = detections.iter().map(|d| d.confidence).sum();

        // Update smoothed signals
        self.ewm_mean = p * EWM_ALPHA + self.ewm_mean * (1.0 - EWM_ALPHA);
        self.baseline_mean = streaming_sma(
            self.baseline_mean,
            p,
            self.lifetime_frame_count,
            BASELINE_WINDOW,
        );

        self.frame_count += 1;
        self.lifetime_frame_count += 1;

        let score = self.current_score();

        // Grace period
        if self.frame_count <= GRACE_FRAMES {
            DetectionResult::Safe
        } else if score >= THRESHOLD_FAILING {
            DetectionResult::Failing { score }
        } else if score >= THRESHOLD_WARNING {
            DetectionResult::Warning { score }
        } else {
            DetectionResult::Safe
        }
    }
}

/// Streaming simple moving average. `count` is the number of samples already
/// integrated into `mean`; `window` caps the effective averaging length.
fn streaming_sma(mean: f64, sample: f64, count: u64, window: u64) -> f64 {
    let divisor = window.min(count + 1) as f64;
    mean + (sample - mean) / divisor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detection(confidence: f64) -> Detection {
        Detection {
            label: "failure".to_string(),
            confidence,
        }
    }

    fn detections_with_confidence(p: f64) -> Vec<Detection> {
        if p == 0.0 { vec![] } else { vec![detection(p)] }
    }

    #[test]
    fn grace_period_returns_safe() {
        let mut state = DetectionState::new(1.0);
        // Build low baseline (200 frames of 0)
        for _ in 0..200 {
            state.update(&detections_with_confidence(0.0), None);
        }
        state.reset_per_print();

        // Multiple detections per frame (p=2.5) — high enough to trigger after grace
        let high_dets = vec![detection(0.85), detection(0.85), detection(0.80)];
        // During grace period (10 frames) → always Safe
        for _ in 0..10 {
            let result = state.update(&high_dets, None);
            assert_eq!(result, DetectionResult::Safe);
        }
        // Frame 11 exits grace period and should trigger
        let result = state.update(&high_dets, None);
        assert_ne!(result, DetectionResult::Safe);
    }

    #[test]
    fn ewm_update_math() {
        let mut state = DetectionState::new(1.0);
        // After first update: ewm = 0.9 * 0.1538 + 0.0 * 0.8462 = 0.13846
        state.update(&detections_with_confidence(0.9), None);
        let expected = 0.9 * EWM_ALPHA;
        assert!(
            (state.ewm_mean - expected).abs() < 1e-10,
            "ewm after 1 frame: {} != {}",
            state.ewm_mean,
            expected
        );

        // After second update: ewm = 0.9 * 0.1538 + 0.13846 * 0.8462 = 0.25508
        state.update(&detections_with_confidence(0.9), None);
        let expected = 0.9 * EWM_ALPHA + expected * (1.0 - EWM_ALPHA);
        assert!(
            (state.ewm_mean - expected).abs() < 1e-10,
            "ewm after 2 frames: {} != {}",
            state.ewm_mean,
            expected
        );
    }

    #[test]
    fn baseline_mean_math() {
        let mut state = DetectionState::new(1.0);
        // Frame 0: mean = 0 + (0.6 - 0) / min(7200, 1) = 0.6
        state.update(&detections_with_confidence(0.6), None);
        assert!((state.baseline_mean - 0.6).abs() < 1e-10);

        // Frame 1: mean = 0.6 + (0.4 - 0.6) / min(7200, 2) = 0.6 + (-0.2/2) = 0.5
        state.update(&detections_with_confidence(0.4), None);
        assert!((state.baseline_mean - 0.5).abs() < 1e-10);

        // Frame 2: mean = 0.5 + (0.8 - 0.5) / min(7200, 3) = 0.5 + 0.1 = 0.6
        state.update(&detections_with_confidence(0.8), None);
        assert!((state.baseline_mean - 0.6).abs() < 1e-10);
    }

    #[test]
    fn sustained_high_confidence_triggers_warning_then_failing() {
        let mut state = DetectionState::new(1.0);
        // Build low baseline (200 frames of 0)
        for _ in 0..200 {
            state.update(&detections_with_confidence(0.0), None);
        }
        state.reset_per_print();

        // Phase 1: p=0.6 triggers Warning (score ≥ 0.35) but not Failing (< 0.55)
        let moderate_dets = vec![detection(0.3), detection(0.3)];
        let mut saw_warning = false;
        for _ in 0..60 {
            if matches!(
                state.update(&moderate_dets, None),
                DetectionResult::Warning { .. }
            ) {
                saw_warning = true;
                break;
            }
        }
        assert!(saw_warning, "should have seen Warning with p=0.6");

        // Phase 2: ramp to p=2.5, score ≥ 0.55 triggers Failing
        let high_dets = vec![detection(0.85), detection(0.85), detection(0.80)];
        let mut saw_failing = false;
        for _ in 0..60 {
            if matches!(
                state.update(&high_dets, None),
                DetectionResult::Failing { .. }
            ) {
                saw_failing = true;
                break;
            }
        }
        assert!(saw_failing, "should have seen Failing with p=2.5");
    }

    #[test]
    fn single_spike_then_low_returns_to_safe() {
        let mut state = DetectionState::new(1.0);
        // Build up past grace period with low values
        for _ in 0..15 {
            state.update(&detections_with_confidence(0.0), None);
        }

        // Single spike
        let result = state.update(&detections_with_confidence(0.95), None);
        // With baseline_mean near 0, ewm after one spike:
        // ewm = 0.95 * 0.1538 = 0.146 (rest was ~0)
        // score = 0.146 - ~0 = 0.146, below THRESHOLD_WARNING (0.35)
        assert_eq!(result, DetectionResult::Safe);

        // Follow with zeros — should stay safe
        for _ in 0..10 {
            let result = state.update(&detections_with_confidence(0.0), None);
            assert_eq!(result, DetectionResult::Safe);
        }
    }

    #[test]
    fn job_id_change_resets_per_print() {
        let mut state = DetectionState::new(1.0);
        // Build up some state with job 1
        for _ in 0..40 {
            state.update(&detections_with_confidence(0.8), Some(1));
        }
        let ewm_before = state.ewm_mean;
        assert!(ewm_before > 0.5);
        let long_before = state.baseline_mean;

        // Switch to job 2 — should reset per-print state
        state.update(&detections_with_confidence(0.0), Some(2));
        assert!(
            state.ewm_mean < 0.01,
            "ewm should be near 0 after reset + zero input, got {}",
            state.ewm_mean
        );
        assert_eq!(state.frame_count, 1);
        // Long-term should be preserved
        assert!(
            (state.baseline_mean - long_before).abs() < 0.1,
            "baseline_mean should be approximately preserved"
        );
    }

    #[test]
    fn reset_per_print_clears_state() {
        let mut state = DetectionState::new(1.0);
        // Build up state
        for _ in 0..40 {
            state.update(&detections_with_confidence(0.8), Some(1));
        }
        assert!(state.ewm_mean > 0.5);

        // Caller resets (e.g. printer goes idle)
        state.reset_per_print();
        assert!(
            state.ewm_mean < 0.01,
            "ewm should be reset, got {}",
            state.ewm_mean
        );

        // Next update should be in grace period
        let result = state.update(&detections_with_confidence(0.9), Some(1));
        assert_eq!(result, DetectionResult::Safe); // grace period
        assert_eq!(state.frame_count, 1);
    }

    #[test]
    fn no_prusalink_mode_no_reset() {
        let mut state = DetectionState::new(1.0);
        // Feed frames without job_id or printing state
        for _ in 0..40 {
            state.update(&detections_with_confidence(0.7), None);
        }
        let ewm = state.ewm_mean;
        assert!(ewm > 0.4, "should have built up ewm, got {ewm}");
        assert_eq!(state.frame_count, 40);
        // No reset happens — state accumulates
    }

    #[test]
    fn high_baseline_needs_bigger_spike() {
        // Simulate a printer with high noise floor
        let mut state = DetectionState::new(1.0);
        // Feed 1000 frames of moderate noise to build up baseline_mean
        for _ in 0..1000 {
            state.update(&detections_with_confidence(0.3), None);
        }
        // baseline_mean should be near 0.3
        assert!(
            (state.baseline_mean - 0.3).abs() < 0.05,
            "long mean should be ~0.3, got {}",
            state.baseline_mean
        );

        // Reset per-print state to simulate new print
        state.reset_per_print();

        // Feed moderate confidence that would trigger on a clean printer
        // but shouldn't trigger here because baseline is high
        for _ in 0..50 {
            state.update(&detections_with_confidence(0.5), None);
        }
        // score = ewm(~0.5) - baseline_mean(~0.3) = ~0.2, below THRESHOLD_WARNING
        let result = state.update(&detections_with_confidence(0.5), None);
        assert_eq!(
            result,
            DetectionResult::Safe,
            "should be safe because baseline is high"
        );
    }

    #[test]
    fn sensitivity_multiplier() {
        // Build two states with low baseline
        let mut high = DetectionState::new(1.2);
        let mut normal = DetectionState::new(1.0);
        for _ in 0..200 {
            high.update(&detections_with_confidence(0.0), None);
            normal.update(&detections_with_confidence(0.0), None);
        }
        high.reset_per_print();
        normal.reset_per_print();

        // Multiple detections per frame (p=1.4) to ensure triggering
        let dets = vec![detection(0.7), detection(0.7)];
        let mut high_warn_frame = None;
        let mut normal_warn_frame = None;

        for i in 0..80 {
            if matches!(
                high.update(&dets, None),
                DetectionResult::Warning { .. } | DetectionResult::Failing { .. }
            ) && high_warn_frame.is_none()
            {
                high_warn_frame = Some(i);
            }
            if matches!(
                normal.update(&dets, None),
                DetectionResult::Warning { .. } | DetectionResult::Failing { .. }
            ) && normal_warn_frame.is_none()
            {
                normal_warn_frame = Some(i);
            }
        }
        assert!(
            high_warn_frame.is_some(),
            "high sensitivity should have warned"
        );
        assert!(
            high_warn_frame.unwrap() <= normal_warn_frame.unwrap_or(u32::MAX),
            "high sensitivity should warn at or before normal"
        );
    }
}
