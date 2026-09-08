# Dolby Vision HDR Mapping

This document describes Erika's implementation of Dolby Vision RPU (Reference Processing Unit) metadata processing and color mapping.

## Overview

Dolby Vision is an HDR format that enhances video quality through per-frame metadata called RPU. The implementation follows [libplacebo](https://github.com/haasn/libplacebo)'s approach (the renderer behind mpv's Dolby Vision support) to ensure compatibility and correctness.

## Supported Profiles

| Profile | Description | Decode Strategy | RPU Available |
|---------|-------------|-----------------|---------------|
| **5** | Single layer, non-backward compatible (IPTPQc2) | Hardware decode on VideoToolbox & D3D11VA; software fallback on mobile backends | ✅ Yes |
| **8** | Single layer with backward-compatible base (e.g. 8.1 HDR10, 8.4 HLG) | Hardware decode allowed | ✅ Yes |

> **Note on Profile Architecture**: Profile 8 is a single-layer profile where the base layer carries standard signaling (such as HDR10 PQ for 8.1 or HLG for 8.4) with Dolby Vision RPU metadata interleaved as NAL units. Profile 7 is the dual-layer profile (Base Layer + Enhancement Layer / FEL / MEL) primarily used on Ultra HD Blu-ray discs.

### Decode Strategy and Metadata Extraction

On desktop platforms:
- **macOS (VideoToolbox)** and **Windows (D3D11VA)** decoders preserve frame side data (`AV_FRAME_DATA_DOVI_METADATA`) alongside hardware texture surfaces (CVPixelBuffer / D3D11 texture). This enables hardware-accelerated decoding while feeding RPU uniforms directly into GPU shaders for per-frame reshaping and color mapping.

On mobile/embedded backends:
- Hardware decoders like **MediaCodec** (Android) and generic **AvCodec** backends may not expose RPU side data attached to output frames.
- For **Profile 5**, because the stream uses IPTPQc2 rather than standard YCbCr and lacks backward compatibility, playback falls back to software decode via FFmpeg's `avcodec` to reliably access `AV_FRAME_DATA_DOVI_METADATA`.
- For **Profile 8**, hardware decode can safely be preserved: if RPU side data is unavailable, the video still displays with correct HDR10/HLG colors because the base layer is backward compatible.

See `dolby_vision_decode_fallback()` in `crates/erika/src/playback.rs`.

## Pipeline Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. Container (MP4/MKV)                                          │
│    - dvcC/dvvC box → Dolby Vision profile (5 or 8)             │
└────────────────────┬────────────────────────────────────────────┘
                     ↓
┌─────────────────────────────────────────────────────────────────┐
│ 2. FFmpeg Decoder (software for Profile 5)                     │
│    - Decodes compressed HEVC stream                             │
│    - Parses RPU from NAL units                                  │
│    - Emits AV_FRAME_DATA_DOVI_METADATA side data               │
└────────────────────┬────────────────────────────────────────────┘
                     ↓
┌─────────────────────────────────────────────────────────────────┐
│ 3. Metadata Extraction (ffmpeg.rs:4372)                         │
│    - Reads AVDOVIMetadata via pointer arithmetic                │
│    - Normalizes pivots by base layer bit depth                  │
│    - Scales coefficients by 2^(-coef_log2_denom)                │
│    - Produces DoviSourceMetadata                                │
└────────────────────┬────────────────────────────────────────────┘
                     ↓
┌─────────────────────────────────────────────────────────────────┐
│ 4. Uniform Packing (pipeline.rs:207)                            │
│    - Converts to vec4-aligned DoviUniforms (~3KB)               │
│    - Packs polynomial and MMR coefficients                      │
│    - Applies signal offset correction for the uploaded bit depth │
└────────────────────┬────────────────────────────────────────────┘
                     ↓
┌─────────────────────────────────────────────────────────────────┐
│ 5. GPU Shader (wgsl/metal/hlsl)                                 │
│    a. Reshaping: piecewise polynomial/MMR per component         │
│    b. Nonlinear matrix: RPU's ycc_to_rgb                        │
│    c. PQ linearization: EOTF                                    │
│    d. LMS→RGB: composite HPE inverse × rgb_to_lms               │
│    e. Tone mapping to display                                   │
└─────────────────────────────────────────────────────────────────┘
```

## Reshaping Algorithm

The core of Dolby Vision mapping is **per-component piecewise reshaping**. Each component (Y, Cb, Cr) is transformed through curves defined by pivots and coefficients.

### Polynomial Segments

For a segment between pivots `p[i]` and `p[i+1]`, if the input signal `s` falls in that range:

```
output = (c2 · s + c1) · s + c0
```

### MMR (Multivariate Polynomial Regression)

For higher-order mapping, MMR mixes all three input components:

```rust
// Order 1
output = constant + dot([a, b, c], [R, G, B]) + dot([d, e, f, g], [R·G, R·B, G·B, R·G·B])

// Order 2: adds R², G², B² and squared cross terms
// Order 3: adds R³, G³, B³ and cubed cross terms
```

See `dovi_reshaped_signal()` in shaders for implementation.

## Color Transform Flow

After reshaping, the signal goes through:

1. **Offset subtraction**: `reshaped - nonlinear_offset`
2. **Nonlinear matrix**: RPU's `ycc_to_rgb` (still PQ-encoded)
3. **PQ linearization**: Convert PQ code to linear light
4. **LMS to RGB**: `(HPE⁻¹ × rgb_to_lms) × linearized`
5. **Gamut/tone mapping**: Standard HDR pipeline continues

## Tone Mapping (libplacebo color map)

The tone map mirrors libplacebo's color map (`pl_shader_color_map`, the
engine behind mpv's `--vo=gpu-next`): RGB in source primaries (absolute
nits) — HPE-LMS — PQ encode — IPT. The operator's curve runs on the IPT
intensity axis, libplacebo's chroma rule (`hull` cubic on the pre/post
intensity pair) protects saturation, and the decode back to RGB lands in
the target primaries, so the primaries conversion happens inside the
roundtrip instead of a separate gamut matrix.

Operators (`ToneMapOperator`, default `Spline` to match mpv's
`--tone-mapping=auto`):

| code | operator | curve parameter (`ToneMapConfig::curve_param`) |
|---|---|---|
| 0 | Clip | — |
| 1 | Reinhard | contrast (default 0.5) |
| 2 | Mobius | linear knee (default 0.3) |
| 3 | BT.2390 | knee offset (default 1.0) |
| 4 | **Spline** (default) | slope contrast (default 0.30) |
| 5 | BT.2446 method A | — |
| 6 | SMPTE ST 2094-10 | knee target (coeffs solved per frame on the CPU) |

Black-point compensation uses `target black = target peak / contrast`
(`contrast_ratio`; auto 1000:1 for SDR, 0 for HDR/EDR targets) and is gated
so SDR → SDR rendering is untouched. The encode maps `[black, peak]` onto
`[0, 1]`, so the compensated floor lands back on code 0.

## Scene-Adaptive HDR10 (measured luma)

HDR10 streams carry only static ST.2086 mastering metadata, so without
further input the tone-map pivot would sit at the fixed 40% knee for every
scene. `crates/erika/src/luma_stats.rs` gives HDR10 the same treatment
Dolby Vision L1 gives Profile 5/8: for software-decoded PQ frames the
presenter samples a sparse grid of the luma plane (NV12/P010), linearizes
each sample, re-encodes to PQ and averages in that perceptual domain, then
smooths the running estimate with libplacebo's IIR filter
(`coeff = 1 - exp(-1/20)`). The smoothed scene average (nits) is attached to
the frame (`PlayerVideoFrame::scene_avg_nits`) and folded into
`SourceColorState.measured_scene_avg_nits`, which `tone_map_extra.y`
prefers over the L1 average — so the spline pivot follows the content.

Hardware-decoded frames (VideoToolbox/D3D11VA/MediaCodec) have no CPU luma
plane and keep the static-metadata path. The estimator resets on generation
changes so seeks cannot carry the previous scene's brightness forward.

## Perceptual Gamut Mapping (IPT 3D LUT)

After the tone map, when an HDR source is tone-mapped into a smaller gamut
(BT.2020 → BT.709), the renderer applies libplacebo's `pl_gamut_map_perceptual`
instead of the fast `gamut_compress`: a CPU-generated 48 × 32 × 256 IPT-space
LUT (`renderer::gamut`) whose texels hold the perceptually mapped
`(I, P + 0.5, T + 0.5)` color — the I axis spans the target display PQ range
and each texel's chroma is rolled off to the destination gamut boundary by a
per-hue golden-section search plus a Möbius soft clip. The shaders rebuild RGB
in the source primaries, run the LMS-PQ-IPT roundtrip, sample the LUT in ICh
space and decode back to the target primaries; the LUT texture is cached per
(source, target, target-peak) and bound as binding/slot 4 (WGSL), texture 2
(Metal) or t2 (D3D11), always with a dummy 1×1×1 fallback so the fast path
keeps a valid layout.

## Per-Frame L1 Brightness Metadata

The RPU's dynamic DM extension blocks carry **level 1** per-frame brightness
metadata: `min_pq` / `max_pq` / `avg_pq` in 12-bit PQ codes. FFmpeg's RPU
decoder copies these blocks into the frame side data (`AVDOVIMetadata`
`ext_block_offset` region); `frame_dovi_level1` validates that region and
extracts the level 1 block.

The frame's `max_pq` replaces the static mastering peak (`source_max_pq`) as
the tone map's source peak, and the frame's `avg_pq` becomes the scene
average that drives the tone-map pivot, matching libplacebo's handling of
the RPU's CIE-Y metadata. This makes the default spline curve
scene-adaptive: a dark scene is not compressed against a 4000-nit mastering
peak, so its highlights stay distinct. The **static mastering display peak
is untouched** — output-mode negotiation (SDR/EDR per display) must never
react to per-frame brightness.

An absent, all-zero, or inverted level 1 block falls back to the static
`source_max_pq` (and the fixed 40% knee when no average is known); the RPU
itself is never rejected over L1.

## Forced BT.2020/PQ

Dolby Vision Profile 5/8 VUI tags are **unreliable**. The implementation forces:

```rust
self.primaries = ColorPrimaries::Bt2020;
self.transfer = TransferFunction::Pq;
```

Without this, a stream tagged as "unspecified transfer" would decode PQ-encoded samples with sRGB gamma, producing completely wrong brightness.

See `SourceColorState::dovi()` in `pipeline.rs:624`.

## Testing Strategy

### Unit Tests

- `frame_reads_dovi_side_data`: Verifies FFmpeg side data parsing
- `dovi_uniforms_pack_pivots_poly_and_mmr`: Validates uniform packing
- `dovi_source_forces_pq_when_stream_tags_are_missing`: Confirms PQ forcing
- `dovi_l1_brightness_metadata_is_parsed`: Verifies level 1 ext-block parsing and invalid-L1 fallback
- `dovi_source_uses_per_frame_l1_peak_when_present`: Confirms L1 max_pq replaces the static peak while mastering metadata stays static
- `dolby_vision_profile_5_stays_on_hardware_for_videotoolbox_and_d3d11va`: Verifies desktop hardware decoders stay on hardware for Profile 5
- `dolby_vision_profile_5_falls_back_to_software_on_mobile_backends`: Verifies mobile backends fall back to software decode for Profile 5
- `dolby_vision_profile_8_stays_on_hardware_decode`: Profile 8 hardware decode preservation

### Integration Tests (require samples)

Set environment variables to enable:

- `ERIKA_DV_SAMPLE`: Profile 5 sample (RPU mapping verification)
- `ERIKA_DV_PROFILE_8_SAMPLE`: Profile 8 sample (hardware decode test)

## Known Limitations

1. **Dual-layer FEL / MEL residual composition not supported**: Profile 7 dual-layer enhancement layers (FEL/MEL) are not composed. If non-trivial NLQ or EL residual is detected, playback gracefully falls back to the base layer.
2. **Mobile hardware decode RPU extraction**: On mobile backends (MediaCodec), hardware decoders do not expose RPU side data, requiring software decode for Profile 5.
3. **Uniform buffer size**: ~3KB may exceed limits on very old mobile GPUs (pre-2015)
4. **CPU plane upload on software decode**: When software decode fallback is used, software planes incur CPU-to-GPU texture upload overhead.

## References

- [Dolby Vision Specification](https://professional.dolby.com/dolby-vision/)
- [libplacebo dovi_reshape implementation](https://github.com/haasn/libplacebo/blob/master/src/shaders/dovi.c)
- [FFmpeg AVDOVIMetadata](https://ffmpeg.org/doxygen/trunk/structAVDOVIMetadata.html)
- [BT.2100 PQ EOTF](https://www.itu.int/rec/R-REC-BT.2100)

## Implementation Files

| File | Purpose |
|------|---------|
| `crates/erika/src/ffmpeg.rs:4344+` | Container and frame metadata extraction |
| `crates/erika/src/playback.rs:5873` | Profile-based decode fallback logic |
| `crates/erika/src/renderer/pipeline.rs:67+` | Data structures and uniform packing |
| `crates/erika/src/renderer/wgpu_video.wgsl:225+` | WGSL shader implementation |
| `crates/erika/src/renderer/metal/apple.rs:2993+` | Metal shader implementation |
| `crates/erika/src/renderer/d3d11.rs:285+` | HLSL shader implementation |
