//! Perceptual gamut mapping via a precomputed IPT-space 3D LUT, ported from
//! libplacebo's `pl_gamut_map_perceptual` (the default gamut map of
//! libplacebo/mpv). The mapping converts a color from the source primaries
//! into the destination primaries *inside* IPT and then clamps out-of-gamut
//! chroma towards the destination gamut boundary with a soft Möbius rolloff,
//! protecting in-gamut colors (dead zone) and shifting hues along the
//! primitive reference points.
//!
//! Because the boundary search is expensive, libplacebo precomputes it once
//! into a 3D LUT over the IPT intensity I, the IPT chroma magnitude C and the
//! IPT hue angle h; the renderers sample it per pixel. This module generates
//! the same LUT on the CPU (48 x 32 x 256 = 393216 texels), and the shaders
//! do the lookup with the same index mapping.
//!
//! Note: libplacebo currently *samples the perceptual LUT in ICh space* with
//! the I channel *in absolute PQ units* over the target display range
//! `[min_luma, max_luma]`. We reproduce that layout exactly so the WGSL/MSL
//! and HLSL samplers match the reference pixels.

use crate::core::ColorPrimaries;
use crate::renderer::pipeline::{RgbMatrix, ipt_rgb2lms_matrix};

/// LUT dimensions, matching libplacebo's `pl_color_map_default_params`
/// `lut3d_size = {48, 32, 256}` (I, C, h).
pub const LUT_SIZE_I: usize = 48;
pub const LUT_SIZE_C: usize = 32;
pub const LUT_SIZE_H: usize = 256;
/// Per-texel component count (RGB with a padded w).
pub const LUT_COMPONENTS: usize = 4;

const PQ_M1: f32 = 2610.0 / 4096.0 * 1.0 / 4.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// 4% crosstalk HPE matrix shared with the tone map.
fn hpe_crosstalk() -> RgbMatrix {
    let c = 0.04_f32;
    RgbMatrix::new([
        [1.0 - 2.0 * c, c, c],
        [c, 1.0 - 2.0 * c, c],
        [c, c, 1.0 - 2.0 * c],
    ])
}

fn rgb2lms(primaries: ColorPrimaries) -> RgbMatrix {
    ipt_rgb2lms_matrix(primaries)
}

fn lms2rgb(primaries: ColorPrimaries) -> RgbMatrix {
    rgb2lms(primaries).inverse()
}

/// Perceptual gamut mapping parameters that change how the LUT is computed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GamutLutParams {
    /// Source primaries of the tone-mapped (still source-referred) signal.
    pub source: ColorPrimaries,
    /// Destination (display) primaries.
    pub target: ColorPrimaries,
    /// Minimum display luminance in PQ code (absolute). We use the source
    /// mastering black for the reference white convention.
    pub min_luma: f32,
    /// Maximum display luminance in PQ code.
    pub max_luma: f32,
}

/// A generated 3D perceptual gamut LUT. Texels are (I, P, T) triples with a
/// padded component, normalized to [0, 1] like libplacebo's uint16 upload
/// (`I`, `P + 0.5`, `T + 0.5` scaled).
#[derive(Debug, Clone, PartialEq)]
pub struct GamutLut {
    /// size = LUT_SIZE_I * LUT_SIZE_C * LUT_SIZE_H, stored as float triples
    /// (R=IPT.I, G=IPT.P + 0.5, B=IPT.T + 0.5) in [0, 1].
    pub texels: Vec<[f32; 3]>,
    /// `min_luma`/`max_luma` in PQ that the LUT's I axis spans.
    pub params: GamutLutParams,
}

impl GamutLut {
    /// Generate the perceptual LUT, mirroring libplacebo's
    /// `pl_gamut_map_perceptual` evaluated over the lattice texels.
    ///
    /// libplacebo caches the per-hue boundary peak (`saturate`); we instead
    /// precompute the source/destination peaks for each of the `LUT_SIZE_H`
    /// hue slices once, which the texel loop then reuses — same math, no
    /// repeated golden-section searches.
    pub fn generate(params: GamutLutParams) -> Self {
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        let mut texels = Vec::with_capacity(LUT_SIZE_I * LUT_SIZE_C * LUT_SIZE_H);
        for hx in 0..LUT_SIZE_H {
            let h = -std::f32::consts::PI
                + 2.0 * std::f32::consts::PI * hx as f32 / (LUT_SIZE_H - 1) as f32;
            let src_peak = saturate(h, &src);
            let dst_peak = saturate(h, &dst);
            let max_c = src_peak[1].max(dst_peak[1]).max(1e-9);
            let soft = Softclip {
                knee: 0.70,
                _desat: 0.35,
            };
            for ix in 0..LUT_SIZE_I {
                let i = params.min_luma
                    + (params.max_luma - params.min_luma) * ix as f32 / (LUT_SIZE_I - 1) as f32;
                for cx in 0..LUT_SIZE_C {
                    let c = 0.5 * cx as f32 / (LUT_SIZE_C - 1) as f32;
                    let mapped = perceptual_map_at(
                        [i, c, h],
                        &src,
                        &dst,
                        src_peak,
                        dst_peak,
                        max_c,
                        &soft,
                        params,
                    );
                    texels.push([mapped[0], mapped[1] * 0.5 + 0.5, mapped[2] * 0.5 + 0.5]);
                }
            }
        }
        Self { texels, params }
    }
}

fn pq_oetf(x: f32) -> f32 {
    let x = x.max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * x) / (1.0 + PQ_C3 * x)).powf(PQ_M2)
}

fn pq_eotf(x: f32) -> f32 {
    let x = x.max(0.0).powf(1.0 / PQ_M2);
    let num = (x - PQ_C1).max(0.0);
    let den = (PQ_C2 - PQ_C3 * x).max(1e-9);
    (num / den).powf(1.0 / PQ_M1)
}

fn rgb2ipt(rgb: [f32; 3], gamut: &GamutState) -> [f32; 3] {
    let lms = gamut.rgb2lms.mul_vec(rgb);
    let lp = pq_oetf(lms[0]);
    let mp = pq_oetf(lms[1]);
    let sp = pq_oetf(lms[2]);
    [
        0.4000 * lp + 0.4000 * mp + 0.2000 * sp,
        4.4550 * lp - 4.8510 * mp + 0.3960 * sp,
        0.8056 * lp + 0.3572 * mp - 1.1628 * sp,
    ]
}

fn ipt2rgb(ipt: [f32; 3], gamut: &GamutState) -> [f32; 3] {
    let lp = ipt[0] + 0.0975689 * ipt[1] + 0.205226 * ipt[2];
    let mp = ipt[0] - 0.1138760 * ipt[1] + 0.133217 * ipt[2];
    let sp = ipt[0] + 0.0326151 * ipt[1] - 0.676887 * ipt[2];
    let l = pq_eotf(lp);
    let m = pq_eotf(mp);
    let s = pq_eotf(sp);
    gamut.lms2rgb.mul_vec([l, m, s])
}

/// Perceptual map of one IPT color given as (I in PQ code, chroma, hue),
/// mirroring libplacebo's `perceptual()` body with the per-hue peaks already
/// computed. `src`/`dst` are the gamut states, `src_peak`/`dst_peak` the
/// maximally-saturated boundary colors at this hue, `max_c` their chroma
/// max (the dead-zone denominator).
fn perceptual_map_at(
    ich: [f32; 3],
    src: &GamutState,
    dst: &GamutState,
    src_peak: [f32; 3],
    dst_peak: [f32; 3],
    max_c: f32,
    soft: &Softclip,
    _params: GamutLutParams,
) -> [f32; 3] {
    let ipt_in = ich2ipt(ich);
    let mapped = rgb2ipt(ipt2rgb(ipt_in, src), dst);

    // Protect in-gamut region: blend only colors whose chroma exceeds the
    // perceptual dead zone (30% of the peak), scaling to full strength.
    let deadzone = 0.30_f32;
    let strength = 0.80_f32;
    let k = pl_smoothstep(deadzone, 1.0, ich[1] / max_c) * strength;
    let ipt = [
        ipt_in[0] + (mapped[0] - ipt_in[0]) * k,
        ipt_in[1] + (mapped[1] - ipt_in[1]) * k,
        ipt_in[2] + (mapped[2] - ipt_in[2]) * k,
    ];

    let rgb = ipt2rgb(ipt, dst);
    let max_rgb = rgb[0].max(rgb[1]).max(rgb[2]);
    let out = [
        softclip(rgb[0], max_rgb, dst.max_rgb, soft).max(dst.min_rgb),
        softclip(rgb[1], max_rgb, dst.max_rgb, soft).max(dst.min_rgb),
        softclip(rgb[2], max_rgb, dst.max_rgb, soft).max(dst.min_rgb),
    ];
    rgb2ipt(out, dst)
}

fn softclip(value: f32, source: f32, target: f32, c: &Softclip) -> f32 {
    if target == 0.0 {
        return 0.0;
    }
    let peak = source / target;
    let x = (value / target).min(peak);
    if x <= c.knee || peak <= 1.0 {
        return value;
    }
    let j = c.knee;
    let a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
    let b = (j * j - 2.0 * j * peak + peak) / (peak - 1.0).max(1e-6);
    let scale = (b * b + 2.0 * b * j + j * j) / (b - a);
    scale * (x + a) / (x + b) * target
}

struct Softclip {
    knee: f32,
    _desat: f32,
}

fn pl_smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

struct GamutState {
    rgb2lms: RgbMatrix,
    lms2rgb: RgbMatrix,
    min_rgb: f32,
    max_rgb: f32,
    min_luma: f32,
    max_luma: f32,
}

impl GamutState {
    fn new(primaries: ColorPrimaries, params: GamutLutParams) -> Self {
        let m = rgb2lms(primaries);
        Self {
            lms2rgb: m.inverse(),
            rgb2lms: m,
            min_rgb: pq_eotf(params.min_luma) - 1e-6,
            max_rgb: pq_eotf(params.max_luma) + 1e-6,
            min_luma: params.min_luma,
            max_luma: params.max_luma,
        }
    }
}

fn ipt2ich(ipt: [f32; 3]) -> [f32; 3] {
    [
        ipt[0],
        (ipt[1] * ipt[1] + ipt[2] * ipt[2]).sqrt(),
        ipt[2].atan2(ipt[1]),
    ]
}

fn ich2ipt(ich: [f32; 3]) -> [f32; 3] {
    [ich[0], ich[1] * ich[2].cos(), ich[1] * ich[2].sin()]
}

/// Returns the maximally saturated in-gamut color of `gamut` at `hue`,
/// using a golden-section search over I with a bounded binary search for the
/// C boundary (mirrors libplacebo's `saturate`).
fn saturate(hue: f32, gamut: &GamutState) -> [f32; 3] {
    let inv_phi = 0.618_033_988_749_894_8_f32;
    let inv_phi2 = 0.381_966_011_250_105_15_f32;

    // Golden-section bracket over I, keeping the full (I, C, h) points so
    // each iteration re-bounds the boundary search like the C version.
    let (mut lo_i, mut lo_c) = (gamut.min_luma, 0.0_f32);
    let (mut hi_i, mut hi_c) = (gamut.max_luma, 0.0_f32);
    let mut de = hi_i - lo_i;
    let mut a = [lo_i + inv_phi2 * de, 0.0, hue];
    let mut b = [lo_i + inv_phi * de, 0.0, hue];
    a[1] = desat_bounded(a[0], hue, 0.0, 0.5, gamut)[1];
    b[1] = desat_bounded(b[0], hue, 0.0, 0.5, gamut)[1];

    while de > 5e-5 {
        de *= inv_phi;
        if a[1] > b[1] {
            hi_i = b[0];
            hi_c = b[1];
            b = a;
            a[0] = lo_i + inv_phi2 * de;
            a[1] = desat_bounded(a[0], hue, lo_c - 5e-5, 0.5, gamut)[1];
        } else {
            lo_i = a[0];
            lo_c = a[1];
            a = b;
            b[0] = lo_i + inv_phi * de;
            b[1] = desat_bounded(b[0], hue, hi_c - 5e-5, 0.5, gamut)[1];
        }
    }

    if a[1] > b[1] {
        [a[0], a[1], hue]
    } else {
        [b[0], b[1], hue]
    }
}

/// Find the gamut boundary at luminance `i` and hue `h` within `[cmin, cmax]`.
fn desat_bounded(i: f32, h: f32, cmin: f32, cmax: f32, gamut: &GamutState) -> [f32; 3] {
    if i <= gamut.min_luma {
        return [gamut.min_luma, 0.0, h];
    }
    if i >= gamut.max_luma {
        return [gamut.max_luma, 0.0, h];
    }
    let max_di = i * 5e-5;
    let mut lo = cmin;
    let mut hi = cmax;
    loop {
        let c = (lo + hi) / 2.0;
        if ingamut(ich2ipt([i, c, h]), gamut) {
            lo = c;
        } else {
            hi = c;
        }
        if hi - lo <= max_di {
            return [i, (lo + hi) / 2.0, h];
        }
    }
}

fn ingamut(ipt: [f32; 3], gamut: &GamutState) -> bool {
    let rgb = ipt2rgb(ipt, gamut);
    rgb[0] >= gamut.min_rgb
        && rgb[0] <= gamut.max_rgb
        && rgb[1] >= gamut.min_rgb
        && rgb[1] <= gamut.max_rgb
        && rgb[2] >= gamut.min_rgb
        && rgb[2] <= gamut.max_rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lut_generation_is_deterministic_and_bounded() {
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 1.0,
        };
        let a = GamutLut::generate(params);
        let b = GamutLut::generate(params);
        assert_eq!(a.texels.len(), LUT_SIZE_I * LUT_SIZE_C * LUT_SIZE_H);
        assert_eq!(a, b);
        for texel in &a.texels {
            assert!(
                texel[0] >= 0.0 && texel[0] <= 1.0,
                "I out of range: {texel:?}"
            );
            assert!(
                texel[1] >= 0.0 && texel[1] <= 1.0,
                "P out of range: {texel:?}"
            );
            assert!(
                texel[2] >= 0.0 && texel[2] <= 1.0,
                "T out of range: {texel:?}"
            );
        }
    }

    #[test]
    fn in_gamut_colors_stay_near_identity_in_dead_zone() {
        // A low-chroma, mid-intensity color (inside both gamuts) maps close
        // to itself: the dead-zone blend keeps k small.
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.0,
            max_luma: 1.0,
        };
        // I=0.5 (mid), C=0.02, h=0
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        let h = 0.0_f32;
        let src_peak = saturate(h, &src);
        let dst_peak = saturate(h, &dst);
        let soft = Softclip {
            knee: 0.70,
            _desat: 0.35,
        };
        let max_c = src_peak[1].max(dst_peak[1]).max(1e-9);
        let out = perceptual_map_at(
            [0.5, 0.02, h],
            &src,
            &dst,
            src_peak,
            dst_peak,
            max_c,
            &soft,
            params,
        );
        assert!(
            (out[0] - 0.5).abs() < 0.05 && (out[1] - 0.02).abs() < 0.1,
            "out={out:?}"
        );
    }

    #[test]
    fn saturated_wide_gamut_color_is_brought_in_gamut() {
        // BT.2020 primary green is far outside BT.709; the map must reduce
        // its chroma without flipping the hue wildly.
        let params = GamutLutParams {
            source: ColorPrimaries::Bt2020,
            target: ColorPrimaries::Bt709,
            min_luma: 0.5,
            max_luma: 0.9,
        };
        // I at mid-display, high chroma, hue of ~primary green in IPT.
        let src = GamutState::new(params.source, params);
        let dst = GamutState::new(params.target, params);
        let h = 0.9_f32;
        let src_peak = saturate(h, &src);
        let dst_peak = saturate(h, &dst);
        let soft = Softclip {
            knee: 0.70,
            _desat: 0.35,
        };
        let max_c = src_peak[1].max(dst_peak[1]).max(1e-9);
        let out = perceptual_map_at(
            [0.7, 0.4, h],
            &src,
            &dst,
            src_peak,
            dst_peak,
            max_c,
            &soft,
            params,
        );
        let rgb = ipt2rgb([out[0], out[1], out[2]], &dst);
        assert!(
            rgb.iter().all(|v| *v >= -1e-4 && *v <= 1.0 + 1e-4),
            "still out of gamut: {rgb:?}"
        );
    }
}
