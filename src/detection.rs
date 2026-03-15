use crate::obico::Detection;

fn streaming_sma(mean: f64, sample: f64, count: u64, window: u64) -> f64 {
    let divisor = window.min(count + 1) as f64;
    mean + (sample - mean) / divisor
}

const EWM_ALPHA: f64 = 2.0 / (12.0 + 1.0); // span=12, α=0.1538
const ROLLING_WIN_SHORT: u64 = 310; // ~50 min at 10s
const ROLLING_WIN_LONG: u64 = 7200; // ~20 hours at 10s
const GRACE_FRAMES: u64 = 30; // ~5 min at 10s
const THRESHOLD_LOW: f64 = 0.38;
const THRESHOLD_HIGH: f64 = 0.78;
const SHORT_MULTIPLE: f64 = 3.8;
const ESCALATING_FACTOR: f64 = 1.75;

#[derive(Debug, PartialEq)]
pub enum DetectionResult {
    Safe,
    Warning { score: f64 },
    Failing { score: f64 },
}

#[derive(Debug)]
pub struct DetectionState {
    ewm_mean: f64,
    rolling_mean_short: f64,
    rolling_mean_long: f64,
    frame_count: u64,
    lifetime_frame_count: u64,
    current_job_id: Option<u64>,
    sensitivity: f64,
}

impl DetectionState {
    pub fn new(sensitivity: f64) -> Self {
        Self {
            ewm_mean: 0.0,
            rolling_mean_short: 0.0,
            rolling_mean_long: 0.0,
            frame_count: 0,
            lifetime_frame_count: 0,
            current_job_id: None,
            sensitivity,
        }
    }

    /// Reset short-term state (EWM, rolling short, frame count).
    /// Call when printer stops printing so next print starts clean.
    pub fn current_score(&self) -> f64 {
        (self.ewm_mean - self.rolling_mean_long) * self.sensitivity
    }

    pub fn reset_short_term(&mut self) {
        self.ewm_mean = 0.0;
        self.rolling_mean_short = 0.0;
        self.frame_count = 0;
    }

    /// Update detection state with new detections.
    /// `job_id` comes from PrusaLink when available; triggers reset on job change.
    pub fn update(&mut self, detections: &[Detection], job_id: Option<u64>) -> DetectionResult {
        // Handle job ID changes (PrusaLink mode)
        if let Some(id) = job_id
            && self.current_job_id != Some(id)
        {
            self.reset_short_term();
            self.current_job_id = Some(id);
        }

        // Compute raw p = sum of detection confidences
        let p: f64 = detections.iter().map(|d| d.confidence).sum();

        // Update smoothed signals
        self.ewm_mean = p * EWM_ALPHA + self.ewm_mean * (1.0 - EWM_ALPHA);
        self.rolling_mean_short = streaming_sma(
            self.rolling_mean_short,
            p,
            self.frame_count,
            ROLLING_WIN_SHORT,
        );
        self.rolling_mean_long = streaming_sma(
            self.rolling_mean_long,
            p,
            self.lifetime_frame_count,
            ROLLING_WIN_LONG,
        );

        self.frame_count += 1;
        self.lifetime_frame_count += 1;

        // Grace period
        if self.frame_count <= GRACE_FRAMES {
            return DetectionResult::Safe;
        }

        let score = self.current_score();

        // Check for Failing (pause threshold) — needs 1.75x higher signal
        if self.is_failing(score / ESCALATING_FACTOR) {
            return DetectionResult::Failing { score };
        }

        // Check for Warning — escalating_factor = 1.0
        if self.is_failing(score) {
            return DetectionResult::Warning { score };
        }

        DetectionResult::Safe
    }

    fn is_failing(&self, adjusted: f64) -> bool {
        if adjusted < THRESHOLD_LOW {
            return false;
        }
        if adjusted > THRESHOLD_HIGH {
            return true;
        }
        let rolling_thresh = (self.rolling_mean_short - self.rolling_mean_long) * SHORT_MULTIPLE;
        adjusted > rolling_thresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detection(confidence: f64) -> Detection {
        Detection {
            label: "failure".to_string(),
            confidence,
            bbox: [0.0; 4],
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
        state.reset_short_term();

        // Multiple detections per frame (p=2.5) — high enough to trigger after grace
        let high_dets = vec![detection(0.85), detection(0.85), detection(0.80)];
        // During grace period (30 frames) → always Safe
        for _ in 0..30 {
            let result = state.update(&high_dets, None);
            assert_eq!(result, DetectionResult::Safe);
        }
        // Frame 31 exits grace period and should trigger
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
    fn rolling_mean_short_math() {
        let mut state = DetectionState::new(1.0);
        // Frame 0: mean = 0 + (0.6 - 0) / min(310, 1) = 0.6
        state.update(&detections_with_confidence(0.6), None);
        assert!((state.rolling_mean_short - 0.6).abs() < 1e-10);

        // Frame 1: mean = 0.6 + (0.4 - 0.6) / min(310, 2) = 0.6 + (-0.2/2) = 0.5
        state.update(&detections_with_confidence(0.4), None);
        assert!((state.rolling_mean_short - 0.5).abs() < 1e-10);

        // Frame 2: mean = 0.5 + (0.8 - 0.5) / min(310, 3) = 0.5 + 0.1 = 0.6
        state.update(&detections_with_confidence(0.8), None);
        assert!((state.rolling_mean_short - 0.6).abs() < 1e-10);
    }

    #[test]
    fn sustained_high_confidence_triggers_warning_then_failing() {
        let mut state = DetectionState::new(1.0);
        // Build low baseline (200 frames of 0)
        for _ in 0..200 {
            state.update(&detections_with_confidence(0.0), None);
        }
        state.reset_short_term();

        // Phase 1: p=1.2 triggers Warning but not Failing
        // After grace: ewm≈1.2, long≈0.16, adjusted≈1.04 → >0.78 → Warning
        // Failing: adjusted/1.75≈0.59, middle zone, rolling_thresh≈(1.2-0.16)*3.8≈3.96 → not Failing
        let moderate_dets = vec![detection(0.6), detection(0.6)];
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
        assert!(saw_warning, "should have seen Warning with p=1.2");

        // Phase 2: ramp to p=2.5, Failing requires adjusted/1.75 > 0.78
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
        for _ in 0..35 {
            state.update(&detections_with_confidence(0.0), None);
        }

        // Single spike
        let result = state.update(&detections_with_confidence(0.95), None);
        // With rolling_mean_long near 0, ewm after one spike:
        // ewm = 0.95 * 0.1538 = 0.146 (rest was ~0)
        // adjusted = 0.146 - ~0 = 0.146, below THRESHOLD_LOW (0.38)
        assert_eq!(result, DetectionResult::Safe);

        // Follow with zeros — should stay safe
        for _ in 0..10 {
            let result = state.update(&detections_with_confidence(0.0), None);
            assert_eq!(result, DetectionResult::Safe);
        }
    }

    #[test]
    fn job_id_change_resets_short_term() {
        let mut state = DetectionState::new(1.0);
        // Build up some state with job 1
        for _ in 0..40 {
            state.update(&detections_with_confidence(0.8), Some(1));
        }
        let ewm_before = state.ewm_mean;
        assert!(ewm_before > 0.5);
        let long_before = state.rolling_mean_long;

        // Switch to job 2 — should reset short-term
        state.update(&detections_with_confidence(0.0), Some(2));
        assert!(
            state.ewm_mean < 0.01,
            "ewm should be near 0 after reset + zero input, got {}",
            state.ewm_mean
        );
        assert_eq!(state.frame_count, 1);
        // Long-term should be preserved
        assert!(
            (state.rolling_mean_long - long_before).abs() < 0.1,
            "rolling_mean_long should be approximately preserved"
        );
    }

    #[test]
    fn reset_short_term_clears_state() {
        let mut state = DetectionState::new(1.0);
        // Build up state
        for _ in 0..40 {
            state.update(&detections_with_confidence(0.8), Some(1));
        }
        assert!(state.ewm_mean > 0.5);

        // Caller resets (e.g. printer goes idle)
        state.reset_short_term();
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
        // Feed 1000 frames of moderate noise to build up rolling_mean_long
        for _ in 0..1000 {
            state.update(&detections_with_confidence(0.3), None);
        }
        // rolling_mean_long should be near 0.3
        assert!(
            (state.rolling_mean_long - 0.3).abs() < 0.05,
            "long mean should be ~0.3, got {}",
            state.rolling_mean_long
        );

        // Reset short-term to simulate new print
        state.reset_short_term();

        // Feed moderate confidence that would trigger on a clean printer
        // but shouldn't trigger here because baseline is high
        for _ in 0..50 {
            state.update(&detections_with_confidence(0.5), None);
        }
        // adjusted = ewm(~0.5) - rolling_mean_long(~0.3) = ~0.2, below THRESHOLD_LOW
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
        high.reset_short_term();
        normal.reset_short_term();

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
