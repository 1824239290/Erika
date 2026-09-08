//! Frame-luminance statistics feeding the tone map's scene-adaptive pivot.
//!
//! mpv/libplacebo measure per-frame average/peak brightness on the GPU
//! (`--hdr-compute-peak`) and feed it to the tone-map curve, which is what
//! makes HDR10 (static-metadata-only) content behave like Dolby Vision's
//! per-frame L1. This module computes the same signals on the CPU for
//! software-decoded frames, sampling a sparse grid of rows/columns so a 4K
//! frame costs well under a millisecond, then smooths them with the same
//! IIR filter libplacebo uses (τ = smoothing_period).
//!
//! Only the luma plane is needed: HDR10 software frames arrive as NV12
//! (8-bit) or P010 (10-bit, MSB-aligned in 16-bit LE), both full-range after
//! `Frame::to_planar_frame`.

/// How many rows/columns of the frame are sampled (stride = dim / SAMPLES).
const SAMPLES: usize = 48;
/// libplacebo `pl_peak_detect_default_params.smoothing_period` (frames).
const SMOOTHING_PERIOD: f32 = 20.0;

const PQ_M1: f32 = 2610.0 / 4096.0 * 1.0 / 4.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// One frame's measured luminance in PQ code (12-bit-ish precision).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameLumaStats {
    /// Mean luma in PQ code over the sampled grid.
    pub avg_pq: f32,
    /// Maximum sampled luma in PQ code.
    pub max_pq: f32,
}

/// Stateful IIR smoothing of the per-frame signals, mirroring libplacebo's
/// `update_peak_buf`: `state += coeff * (measured - state)` with
/// `coeff = 1 - exp(-1/smoothing_period)`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LumaSmoother {
    pub avg_pq: f32,
    pub max_pq: f32,
    /// 0 until the first frame is pushed.
    initialized: bool,
}

impl LumaSmoother {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a frame's measurements into the running estimate and return the
    /// smoothed (avg, max) in PQ code.
    pub fn push(&mut self, measured: FrameLumaStats) -> (f32, f32) {
        let coeff = 1.0 - (-1.0 / SMOOTHING_PERIOD).exp();
        if !self.initialized {
            self.avg_pq = measured.avg_pq;
            self.max_pq = measured.max_pq;
            self.initialized = true;
        } else {
            self.avg_pq += coeff * (measured.avg_pq - self.avg_pq);
            self.max_pq += coeff * (measured.max_pq - self.max_pq);
        }
        (self.avg_pq, self.max_pq)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Convert a normalized luma sample in [0, 1] (after range expansion and
/// bit-depth normalization) to its PQ code.
fn pq_code_of_luma(luma: f32) -> f32 {
    let p = luma.clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}

/// Fold the sampled PQ codes into a FrameLumaStats, averaging in the PQ
/// (perceptual) domain like libplacebo's histogram.
fn fold(codes: impl Iterator<Item = f32>) -> Option<FrameLumaStats> {
    let mut sum = 0.0_f64;
    let mut max_code = 0.0_f32;
    let mut count = 0_u64;
    for code in codes {
        sum += code as f64;
        if code > max_code {
            max_code = code;
        }
        count += 1;
    }
    if count == 0 {
        return None;
    }
    Some(FrameLumaStats {
        avg_pq: (sum / count as f64) as f32,
        max_pq: max_code,
    })
}

/// Convert a normalized PQ-encoded luma sample to a PQ code over 10 k nits.
/// The luma plane of an HDR10 frame holds PQ-encoded Y'; decode it to linear
/// nits first (PQ EOTF), then re-encode so the average is perceptual.
fn sample_code(normalized: f32) -> f32 {
    let linear_nits = pq_eotf(normalized.clamp(0.0, 1.0)) * 10000.0;
    pq_code_of_luma(linear_nits / 10000.0)
}

/// Measure the average and peak luma of an NV12 luma plane (8-bit, full
/// range) by sampling a sparse grid of rows and columns.
pub fn measure_nv12_luma(luma: &[u8], width: u32, height: u32) -> Option<FrameLumaStats> {
    if width == 0 || height == 0 || luma.len() < (width as usize) * (height as usize) {
        return None;
    }
    let w = width as usize;
    let h = height as usize;
    let x_step = (w / SAMPLES).max(1);
    let y_step = (h / SAMPLES).max(1);
    fold((0..h).step_by(y_step).flat_map(move |y| {
        let row = &luma[y * w..y * w + w];
        (0..w)
            .step_by(x_step)
            .map(move |x| sample_code(row[x] as f32 / 255.0))
    }))
}

/// Measure the average and peak luma of a P010 luma plane (10-bit samples
/// MSB-aligned in 16-bit little-endian, full range).
pub fn measure_p010_luma(luma: &[u8], width: u32, height: u32) -> Option<FrameLumaStats> {
    if width == 0 || height == 0 || luma.len() < (width as usize) * (height as usize) * 2 {
        return None;
    }
    let w = width as usize;
    let h = height as usize;
    let x_step = (w / SAMPLES).max(1);
    let y_step = (h / SAMPLES).max(1);
    fold((0..h).step_by(y_step).flat_map(move |y| {
        let row = &luma[y * w * 2..(y + 1) * w * 2];
        (0..w).step_by(x_step).map(move |x| {
            let sample = u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]);
            // 10-bit MSB-aligned: value << 6. Normalize to full 10-bit range.
            let normalized = (sample >> 6) as f32 / 1023.0;
            sample_code(normalized)
        })
    }))
}

fn pq_eotf(code: f32) -> f32 {
    let p = code.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
    let num = (p - PQ_C1).max(0.0);
    let den = (PQ_C2 - PQ_C3 * p).max(1e-9);
    (num / den).powf(1.0 / PQ_M1)
}

/// Convert a PQ code back to nits (for diagnostics and for feeding the
/// `tone_map_extra.y` scene-average slot, which expects nits).
pub fn pq_code_to_nits(code: f32) -> f32 {
    10000.0 * pq_eotf(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_black_frame_measures_near_zero() {
        let (w, h) = (1920_u32, 1080_u32);
        let luma = vec![0_u8; (w * h) as usize];
        let stats = measure_nv12_luma(&luma, w, h).unwrap();
        assert!(pq_code_to_nits(stats.avg_pq) < 0.5, "avg {}", stats.avg_pq);
        assert!(pq_code_to_nits(stats.max_pq) < 0.5, "max {}", stats.max_pq);
    }

    #[test]
    fn nv12_full_white_measures_ten_k_nits() {
        let (w, h) = (1920_u32, 1080_u32);
        let luma = vec![255_u8; (w * h) as usize];
        let stats = measure_nv12_luma(&luma, w, h).unwrap();
        assert!((pq_code_to_nits(stats.avg_pq) - 10000.0).abs() < 20.0);
        assert!((pq_code_to_nits(stats.max_pq) - 10000.0).abs() < 20.0);
    }

    #[test]
    fn p010_white_measures_ten_k_nits() {
        let (w, h) = (1920_u32, 1080_u32);
        let mut luma = Vec::with_capacity((w * h * 2) as usize);
        for _ in 0..(w * h) as usize {
            luma.extend_from_slice(&(1023_u16 << 6).to_le_bytes());
        }
        let stats = measure_p010_luma(&luma, w, h).unwrap();
        assert!((pq_code_to_nits(stats.avg_pq) - 10000.0).abs() < 20.0);
    }

    #[test]
    fn bright_highlights_raise_the_peak_but_not_the_mean() {
        let (w, h) = (1920_u32, 1080_u32);
        // Mostly dim (16/255) with a small bright region (255).
        let mut luma = vec![16_u8; (w * h) as usize];
        for y in (h / 4)..(h / 2) {
            for x in (w / 4)..(w / 2) {
                luma[(y * w + x) as usize] = 255;
            }
        }
        let stats = measure_nv12_luma(&luma, w, h).unwrap();
        let avg = pq_code_to_nits(stats.avg_pq);
        let max = pq_code_to_nits(stats.max_pq);
        assert!(avg < 100.0, "avg {avg}");
        assert!(max > 9000.0, "max {max}");
    }

    #[test]
    fn smoother_converges_and_tracks_step() {
        let mut smoother = LumaSmoother::new();
        let dim = FrameLumaStats {
            avg_pq: 0.2,
            max_pq: 0.5,
        };
        // First sample initializes directly.
        let (avg, max) = smoother.push(dim);
        assert_eq!(avg, 0.2);
        assert_eq!(max, 0.5);
        // A sustained new level converges asymptotically (τ=20).
        let bright = FrameLumaStats {
            avg_pq: 0.6,
            max_pq: 0.9,
        };
        let mut last = 0.0_f32;
        for _ in 0..300 {
            let (avg, _) = smoother.push(bright);
            last = avg;
        }
        assert!((last - 0.6).abs() < 0.01, "converged avg {last}");
        // A single bright frame only nudges the estimate a little.
        let mut s2 = LumaSmoother::new();
        s2.push(dim);
        s2.push(bright);
        let (avg, max) = (s2.avg_pq, s2.max_pq);
        assert!(avg < 0.25 && avg > 0.2, "avg {avg}");
        assert!(max < 0.55 && max > 0.5, "max {max}");
    }

    #[test]
    fn reset_clears_state() {
        let mut smoother = LumaSmoother::new();
        smoother.push(FrameLumaStats {
            avg_pq: 0.8,
            max_pq: 0.9,
        });
        smoother.reset();
        assert_eq!(smoother, LumaSmoother::default());
    }
}
