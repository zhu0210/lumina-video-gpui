//! Copy-only SDR Y'CbCr semantics shared by every renderer.
//!
//! The affine matrix is deliberately kept here rather than in a GPU adapter.
//! That makes the CPU downgrade and the fixed GPUI shader use the same range
//! expansion, neutral chroma, and matrix coefficients.  The matrix is stored
//! column-major because that is the representation consumed by WGSL/GPUI.

use crate::{CpuPlane, FrameExtent};

/// Matrix used by an 8-bit Y'CbCr frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMatrix {
    /// No matrix was supplied by the decoder.
    #[default]
    Unknown,
    /// ITU-R BT.601.
    Bt601,
    /// ITU-R BT.709.
    Bt709,
    /// FCC 73.682 / SMPTE 170M-era matrix.
    Fcc,
    /// SMPTE 240M matrix.
    Smpte240m,
    /// BT.2020 matrix, outside this SDR ticket.
    Bt2020,
    /// A matrix outside the known SDR set.
    Unsupported,
}

/// RGB primaries carried by a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorPrimaries {
    /// No primaries were supplied by the decoder.
    #[default]
    Unknown,
    /// BT.709 primaries.
    Bt709,
    /// BT.470M primaries.
    Bt470m,
    /// ITU-R BT.470BG primaries.
    Bt470Bg,
    /// SMPTE 170M primaries.
    Smpte170m,
    /// SMPTE 240M primaries.
    Smpte240m,
    /// Film primaries.
    Film,
    /// BT.2020 primaries, outside this SDR ticket.
    Bt2020,
    /// Adobe RGB primaries.
    Adobergb,
    /// A primaries set outside the known SDR set.
    Unsupported,
}

/// Transfer characteristic carried by a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorTransfer {
    /// No transfer characteristic was supplied by the decoder.
    #[default]
    Unknown,
    /// BT.601 transfer characteristic.
    Bt601,
    /// BT.709 transfer characteristic.
    Bt709,
    /// SMPTE 240M transfer characteristic.
    Smpte240m,
    /// sRGB transfer characteristic.
    Srgb,
    /// Pure gamma 1.0 transfer.
    Gamma10,
    /// Pure gamma 1.8 transfer.
    Gamma18,
    /// Pure gamma 2.0 transfer.
    Gamma20,
    /// Pure gamma 2.2 transfer.
    Gamma22,
    /// Pure gamma 2.8 transfer.
    Gamma28,
    /// BT.2020 10-bit transfer, outside this SDR ticket.
    Bt202010,
    /// BT.2020 12-bit transfer, outside this SDR ticket.
    Bt202012,
    /// SMPTE ST 2084/PQ transfer, outside this SDR ticket.
    Smpte2084,
    /// ARIB STD-B67/HLG transfer, outside this SDR ticket.
    AribStdB67,
    /// Adobe RGB transfer.
    Adobergb,
    /// Logarithmic transfer characteristic.
    Log100,
    /// Logarithmic transfer characteristic.
    Log316,
    /// A transfer characteristic outside the known set.
    Unsupported,
}

/// Code range used by an 8-bit Y'CbCr frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorRange {
    /// No range was supplied by the decoder.
    #[default]
    Unknown,
    /// Full/video-independent range, 0..255.
    Full,
    /// Legal/video range, Y 16..235 and Cb/Cr 16..240.
    Limited,
    /// A range outside the known SDR set.
    Unsupported,
}

/// Horizontal 4:2:0 chroma siting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChromaHorizontal {
    /// No horizontal siting was supplied.
    #[default]
    Unknown,
    /// Chroma is centered over the corresponding 2x2 luma block.
    Centered,
    /// Chroma is cosited with the left luma sample.
    Cosited,
    /// A horizontal siting outside the supported set.
    Unsupported,
}

/// Vertical 4:2:0 chroma siting, including interlaced markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChromaVertical {
    /// No vertical siting was supplied.
    #[default]
    Unknown,
    /// Chroma is centered over the corresponding 2x2 luma block.
    Centered,
    /// Chroma is cosited with the top luma sample.
    Cosited,
    /// Alternate-line/interlaced chroma, not implemented here.
    AlternateLine,
    /// DV chroma siting, not implemented here.
    Dv,
    /// A vertical siting outside the supported set.
    Unsupported,
}

/// Copy-only color metadata crossing the native-frame seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorMetadata {
    pub matrix: ColorMatrix,
    pub primaries: ColorPrimaries,
    pub transfer: ColorTransfer,
    pub range: ColorRange,
    pub chroma_horizontal: ChromaHorizontal,
    pub chroma_vertical: ChromaVertical,
}

/// A render decision made once for one negotiated extent and color tuple.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorRenderDecision {
    /// The fixed GPUI shader can consume this matrix and sample geometry.
    Gpu([[f32; 4]; 4]),
    /// The worker must produce an RGBA frame using this affine matrix.
    CpuRgba([[f32; 4]; 4]),
    /// Unknown matrix/range or an explicit HDR/out-of-ticket value.
    Unsupported,
    /// Known non-HDR SDR metadata that needs a color implementation not in
    /// this ticket. This is a session-level decision, not a per-frame retry.
    UnsupportedSdrColor,
}

/// Errors from the allocation-free NV12-to-RGBA conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorConvertError {
    MissingPlane,
    OutputTooSmall,
    InvalidSiting,
}

/// Builds the production BT.601/BT.709/FCC/SMPTE 240M affine transform.
///
/// The input is normalized `[Y, Cb, Cr, 1]`; legal-range expansion is exact
/// for 8-bit video code values (Y 16..235 and chroma 16..240), with neutral
/// chroma code 128. The outer array is column-major.
pub fn yuv_to_rgb_matrix(matrix: ColorMatrix, range: ColorRange) -> Option<[[f32; 4]; 4]> {
    let (kr, kb) = match matrix {
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt709 => (0.2126, 0.0722),
        ColorMatrix::Fcc => (0.30, 0.11),
        ColorMatrix::Smpte240m => (0.212, 0.087),
        ColorMatrix::Unknown | ColorMatrix::Bt2020 | ColorMatrix::Unsupported => return None,
    };
    let kg = 1.0 - kr - kb;
    let (y_scale, y_offset, chroma_scale, chroma_offset) = match range {
        ColorRange::Full => (1.0, 0.0, 1.0, -128.0 / 255.0),
        ColorRange::Limited => (255.0 / 219.0, -16.0 / 219.0, 255.0 / 224.0, -128.0 / 224.0),
        ColorRange::Unknown | ColorRange::Unsupported => return None,
    };
    let red_cr = 2.0 * (1.0 - kr);
    let blue_cb = 2.0 * (1.0 - kb);
    let green_cb = 2.0 * kb * (1.0 - kb) / kg;
    let green_cr = 2.0 * kr * (1.0 - kr) / kg;
    Some([
        [y_scale, y_scale, y_scale, 0.0],
        [0.0, -green_cb * chroma_scale, blue_cb * chroma_scale, 0.0],
        [red_cr * chroma_scale, -green_cr * chroma_scale, 0.0, 0.0],
        [
            y_offset + red_cr * chroma_offset,
            y_offset - green_cb * chroma_offset - green_cr * chroma_offset,
            y_offset + blue_cb * chroma_offset,
            1.0,
        ],
    ])
}

/// Returns the matrix element applied by the fixed GPUI shader for one pixel.
pub fn apply_yuv_matrix(matrix: &[[f32; 4]; 4], y: u8, cb: u8, cr: u8) -> [u8; 3] {
    apply_yuv_matrix_f32(
        matrix,
        f32::from(y) / 255.0,
        f32::from(cb) / 255.0,
        f32::from(cr) / 255.0,
    )
    .map(to_rgb_code)
}

/// Applies the shared affine matrix without quantizing its RGB result.
///
/// The CPU downgrade uses this same float core after bilinear chroma
/// resampling, so its result is quantized only once at the final RGBA write.
fn apply_yuv_matrix_f32(matrix: &[[f32; 4]; 4], y: f32, cb: f32, cr: f32) -> [f32; 3] {
    let [[y_red, y_green, y_blue, _], [cb_red, cb_green, cb_blue, _], [cr_red, cr_green, cr_blue, _], [offset_red, offset_green, offset_blue, _]] =
        *matrix;
    [
        y_red * y + cb_red * cb + cr_red * cr + offset_red,
        y_green * y + cb_green * cb + cr_green * cr + offset_green,
        y_blue * y + cb_blue * cb + cr_blue * cr + offset_blue,
    ]
}

/// Selects the one-time worker/GPU path for a negotiated color tuple.
pub fn render_decision(color: ColorMetadata) -> ColorRenderDecision {
    if matches!(
        color.matrix,
        ColorMatrix::Unknown | ColorMatrix::Bt2020 | ColorMatrix::Unsupported
    ) || matches!(color.range, ColorRange::Unknown | ColorRange::Unsupported)
    {
        return ColorRenderDecision::Unsupported;
    }
    if matches!(
        color.primaries,
        ColorPrimaries::Bt2020 | ColorPrimaries::Unknown | ColorPrimaries::Unsupported
    ) || matches!(
        color.transfer,
        ColorTransfer::Bt202010
            | ColorTransfer::Bt202012
            | ColorTransfer::Smpte2084
            | ColorTransfer::AribStdB67
            | ColorTransfer::Unknown
            | ColorTransfer::Unsupported
    ) {
        return if matches!(
            color.primaries,
            ColorPrimaries::Bt2020 | ColorPrimaries::Unsupported
        ) || matches!(
            color.transfer,
            ColorTransfer::Bt202010
                | ColorTransfer::Bt202012
                | ColorTransfer::Smpte2084
                | ColorTransfer::AribStdB67
                | ColorTransfer::Unsupported
        ) {
            ColorRenderDecision::Unsupported
        } else {
            ColorRenderDecision::UnsupportedSdrColor
        };
    }
    if !matches!(color.matrix, ColorMatrix::Bt601 | ColorMatrix::Bt709) {
        return ColorRenderDecision::UnsupportedSdrColor;
    }
    let Some(matrix) = yuv_to_rgb_matrix(color.matrix, color.range) else {
        return ColorRenderDecision::Unsupported;
    };
    if !matches!(color.primaries, ColorPrimaries::Bt709) {
        return ColorRenderDecision::UnsupportedSdrColor;
    }
    let transfer_supported = matches!(
        color.transfer,
        ColorTransfer::Srgb
            | ColorTransfer::Bt601
            | ColorTransfer::Bt709
            | ColorTransfer::Gamma10
            | ColorTransfer::Gamma18
            | ColorTransfer::Gamma20
            | ColorTransfer::Gamma22
            | ColorTransfer::Gamma28
    );
    let chroma_supported = matches!(
        (color.chroma_horizontal, color.chroma_vertical),
        (ChromaHorizontal::Centered, ChromaVertical::Centered)
            | (ChromaHorizontal::Centered, ChromaVertical::Cosited)
            | (ChromaHorizontal::Cosited, ChromaVertical::Centered)
            | (ChromaHorizontal::Cosited, ChromaVertical::Cosited)
    );
    if !transfer_supported || !chroma_supported {
        return ColorRenderDecision::UnsupportedSdrColor;
    }
    // Zed samples Y and half-resolution CbCr at identical normalized
    // coordinates: each UV texel is centered over its 2x2 luma block.
    // MPEG2/H_COSITED needs a -0.5 luma-pixel horizontal shift, which the
    // fixed shader cannot carry.
    if matches!(color.transfer, ColorTransfer::Srgb)
        && matches!(
            (
                color.matrix,
                color.range,
                color.chroma_horizontal,
                color.chroma_vertical
            ),
            (
                ColorMatrix::Bt601 | ColorMatrix::Bt709,
                ColorRange::Full | ColorRange::Limited,
                ChromaHorizontal::Centered,
                ChromaVertical::Centered
            )
        )
    {
        ColorRenderDecision::Gpu(matrix)
    } else {
        ColorRenderDecision::CpuRgba(matrix)
    }
}

/// Converts one NV12 frame into a preallocated RGBA payload.
///
/// Chroma samples are bilinearly resampled with clamp-to-edge. Centered axes
/// use `luma_index / 2 - 0.25`; cosited axes use `luma_index / 2`. Horizontal
/// and vertical siting are intentionally independent.
pub fn nv12_to_rgba_into(
    planes: &[CpuPlane],
    extent: FrameExtent,
    color: ColorMetadata,
    matrix: &[[f32; 4]; 4],
    output: &mut [u8],
) -> Result<(), ColorConvertError> {
    let y_plane = planes.first().ok_or(ColorConvertError::MissingPlane)?;
    let uv_plane = planes.get(1).ok_or(ColorConvertError::MissingPlane)?;
    nv12_bytes_to_rgba_into(
        &y_plane.bytes,
        y_plane.stride,
        &uv_plane.bytes,
        uv_plane.stride,
        extent,
        color,
        matrix,
        output,
    )
}

/// Borrowed-plane form used by the legacy renderer path. It has the same
/// production math and sampling semantics as [`nv12_to_rgba_into`] but does
/// not manufacture an owning plane container. The separate byte/stride
/// arguments intentionally preserve the zero-copy plane seam.
#[allow(clippy::too_many_arguments)]
pub fn nv12_bytes_to_rgba_into(
    y_bytes: &[u8],
    y_stride: usize,
    uv_bytes: &[u8],
    uv_stride: usize,
    extent: FrameExtent,
    color: ColorMetadata,
    matrix: &[[f32; 4]; 4],
    output: &mut [u8],
) -> Result<(), ColorConvertError> {
    if !matches!(
        color.chroma_horizontal,
        ChromaHorizontal::Centered | ChromaHorizontal::Cosited
    ) || !matches!(
        color.chroma_vertical,
        ChromaVertical::Centered | ChromaVertical::Cosited
    ) {
        return Err(ColorConvertError::InvalidSiting);
    }
    let width = usize::try_from(extent.width).map_err(|_| ColorConvertError::OutputTooSmall)?;
    let height = usize::try_from(extent.height).map_err(|_| ColorConvertError::OutputTooSmall)?;
    let chroma_width = width.saturating_add(1) / 2;
    let chroma_height = height.saturating_add(1) / 2;
    let byte_count = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ColorConvertError::OutputTooSmall)?;
    if output.len() < byte_count {
        return Err(ColorConvertError::OutputTooSmall);
    }
    let transfer = color.transfer;
    for y in 0..height {
        for x in 0..width {
            let y_index = y.saturating_mul(y_stride).saturating_add(x);
            let y_value = y_bytes.get(y_index).copied().unwrap_or(0);
            let cb = sample_chroma(
                uv_bytes,
                uv_stride,
                x,
                y,
                true,
                (color.chroma_horizontal, color.chroma_vertical),
                (chroma_width, chroma_height),
            );
            let cr = sample_chroma(
                uv_bytes,
                uv_stride,
                x,
                y,
                false,
                (color.chroma_horizontal, color.chroma_vertical),
                (chroma_width, chroma_height),
            );
            let mut rgb =
                apply_yuv_matrix_f32(matrix, f32::from(y_value) / 255.0, cb / 255.0, cr / 255.0);
            if !matches!(transfer, ColorTransfer::Srgb) {
                for channel in &mut rgb {
                    let encoded = channel.clamp(0.0, 1.0);
                    *channel = encode_srgb(decode_transfer(encoded, transfer));
                }
            }
            let [red, green, blue] = rgb.map(to_rgb_code);
            let output_index = y.saturating_mul(width).saturating_add(x).saturating_mul(4);
            if let Some(pixel) = output.get_mut(output_index..output_index.saturating_add(4)) {
                pixel.copy_from_slice(&[red, green, blue, 255]);
            }
        }
    }
    Ok(())
}

/// Converts borrowed planar 4:2:0 bytes for the legacy CPU compatibility
/// path. The matrix application remains the same production function used by
/// NV12 and GPU fixtures. The separate byte/stride arguments intentionally
/// preserve the zero-copy plane seam.
#[allow(clippy::too_many_arguments)]
pub fn yuv420p_bytes_to_rgba_into(
    y_bytes: &[u8],
    y_stride: usize,
    u_bytes: &[u8],
    u_stride: usize,
    v_bytes: &[u8],
    v_stride: usize,
    extent: FrameExtent,
    matrix: &[[f32; 4]; 4],
    output: &mut [u8],
) -> Result<(), ColorConvertError> {
    let width = usize::try_from(extent.width).map_err(|_| ColorConvertError::OutputTooSmall)?;
    let height = usize::try_from(extent.height).map_err(|_| ColorConvertError::OutputTooSmall)?;
    let byte_count = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ColorConvertError::OutputTooSmall)?;
    if output.len() < byte_count {
        return Err(ColorConvertError::OutputTooSmall);
    }
    for y in 0..height {
        for x in 0..width {
            let y_value = y_bytes
                .get(y.saturating_mul(y_stride).saturating_add(x))
                .copied()
                .unwrap_or(0);
            let chroma_x = x / 2;
            let chroma_y = y / 2;
            let u_value = u_bytes
                .get(chroma_y.saturating_mul(u_stride).saturating_add(chroma_x))
                .copied()
                .unwrap_or(128);
            let v_value = v_bytes
                .get(chroma_y.saturating_mul(v_stride).saturating_add(chroma_x))
                .copied()
                .unwrap_or(128);
            let [red, green, blue] = apply_yuv_matrix(matrix, y_value, u_value, v_value);
            let output_index = y.saturating_mul(width).saturating_add(x).saturating_mul(4);
            if let Some(pixel) = output.get_mut(output_index..output_index.saturating_add(4)) {
                pixel.copy_from_slice(&[red, green, blue, 255]);
            }
        }
    }
    Ok(())
}

fn sample_chroma(
    bytes: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    cb: bool,
    siting: (ChromaHorizontal, ChromaVertical),
    dimensions: (usize, usize),
) -> f32 {
    let (horizontal, vertical) = siting;
    let (width, height) = dimensions;
    let x_position = siting_position(x, horizontal);
    let y_position = siting_position(
        y,
        match vertical {
            ChromaVertical::Centered => ChromaHorizontal::Centered,
            ChromaVertical::Cosited => ChromaHorizontal::Cosited,
            ChromaVertical::Unknown
            | ChromaVertical::AlternateLine
            | ChromaVertical::Dv
            | ChromaVertical::Unsupported => ChromaHorizontal::Unknown,
        },
    );
    let width = width.max(1);
    let height = height.max(1);
    let x_max = width.saturating_sub(1) as f32;
    let y_max = height.saturating_sub(1) as f32;
    let x_position = x_position.clamp(0.0, x_max);
    let y_position = y_position.clamp(0.0, y_max);
    let x0 = x_position.floor() as usize;
    let y0 = y_position.floor() as usize;
    let x1 = x0.saturating_add(1).min(width.saturating_sub(1));
    let y1 = y0.saturating_add(1).min(height.saturating_sub(1));
    let tx = x_position - x0 as f32;
    let ty = y_position - y0 as f32;
    let offset = if cb { 0 } else { 1 };
    let top_left = plane_value(bytes, stride, x0, y0, offset);
    let top_right = plane_value(bytes, stride, x1, y0, offset);
    let bottom_left = plane_value(bytes, stride, x0, y1, offset);
    let bottom_right = plane_value(bytes, stride, x1, y1, offset);
    let top = top_left + (top_right - top_left) * tx;
    let bottom = bottom_left + (bottom_right - bottom_left) * tx;
    top + (bottom - top) * ty
}

fn siting_position(index: usize, siting: ChromaHorizontal) -> f32 {
    match siting {
        ChromaHorizontal::Centered => index as f32 / 2.0 - 0.25,
        ChromaHorizontal::Cosited => index as f32 / 2.0,
        ChromaHorizontal::Unknown | ChromaHorizontal::Unsupported => 0.0,
    }
}

fn plane_value(bytes: &[u8], stride: usize, x: usize, y: usize, channel: usize) -> f32 {
    let index = y
        .saturating_mul(stride)
        .saturating_add(x.saturating_mul(2))
        .saturating_add(channel);
    f32::from(bytes.get(index).copied().unwrap_or(128))
}

fn decode_transfer(value: f32, transfer: ColorTransfer) -> f32 {
    match transfer {
        ColorTransfer::Bt601 | ColorTransfer::Bt709 => {
            if value < 0.081 {
                value / 4.5
            } else {
                ((value + 0.099) / 1.099).powf(1.0 / 0.45)
            }
        }
        ColorTransfer::Gamma10 => value,
        ColorTransfer::Gamma18 => value.powf(1.8),
        ColorTransfer::Gamma20 => value.powf(2.0),
        ColorTransfer::Gamma22 => value.powf(2.2),
        ColorTransfer::Gamma28 => value.powf(2.8),
        ColorTransfer::Srgb | ColorTransfer::Unknown | ColorTransfer::Unsupported => value,
        ColorTransfer::Smpte240m => value,
        ColorTransfer::Bt202010
        | ColorTransfer::Bt202012
        | ColorTransfer::Smpte2084
        | ColorTransfer::AribStdB67
        | ColorTransfer::Adobergb
        | ColorTransfer::Log100
        | ColorTransfer::Log316 => value,
    }
}

fn encode_srgb(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn to_code(value: f32) -> u8 {
    value.clamp(0.0, 255.0).round() as u8
}

fn to_rgb_code(value: f32) -> u8 {
    to_code(value.clamp(0.0, 1.0) * 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(matrix: ColorMatrix, range: ColorRange) -> ColorMetadata {
        ColorMetadata {
            matrix,
            primaries: ColorPrimaries::Bt709,
            transfer: ColorTransfer::Srgb,
            range,
            chroma_horizontal: ChromaHorizontal::Centered,
            chroma_vertical: ChromaVertical::Centered,
        }
    }

    fn reference_matrix(
        matrix: ColorMatrix,
        range: ColorRange,
        y: u8,
        cb: f64,
        cr: f64,
    ) -> [u8; 3] {
        let (kr, kb) = match matrix {
            ColorMatrix::Bt601 => (0.299_f64, 0.114_f64),
            ColorMatrix::Bt709 => (0.2126_f64, 0.0722_f64),
            _ => return [0; 3],
        };
        let kg = 1.0 - kr - kb;
        let y = f64::from(y);
        let (y_value, cb_value, cr_value) = match range {
            ColorRange::Full => (y / 255.0, (cb - 128.0) / 255.0, (cr - 128.0) / 255.0),
            ColorRange::Limited => (
                (y - 16.0) / 219.0,
                (cb - 128.0) / 224.0,
                (cr - 128.0) / 224.0,
            ),
            ColorRange::Unknown | ColorRange::Unsupported => return [0; 3],
        };
        let red_cr = 2.0 * (1.0 - kr);
        let blue_cb = 2.0 * (1.0 - kb);
        let green_cb = 2.0 * kb * (1.0 - kb) / kg;
        let green_cr = 2.0 * kr * (1.0 - kr) / kg;
        [
            y_value + red_cr * cr_value,
            y_value - green_cb * cb_value - green_cr * cr_value,
            y_value + blue_cb * cb_value,
        ]
        .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    fn reference_sample(bytes: &[u8], stride: usize, x: usize, y: usize, channel: usize) -> f64 {
        let x_position = (x as f64 / 2.0 - 0.25).clamp(0.0, 1.0);
        let y_position = (y as f64 / 2.0 - 0.25).clamp(0.0, 0.0);
        let x0 = x_position.floor() as usize;
        let x1 = (x0 + 1).min(1);
        let tx = x_position - x0 as f64;
        let read = |xx: usize| {
            f64::from(
                bytes
                    .get(y_position.floor() as usize * stride + xx * 2 + channel)
                    .copied()
                    .unwrap_or(128),
            )
        };
        let left = read(x0);
        let right = read(x1);
        left + (right - left) * tx
    }

    #[test]
    fn four_sdr_fixtures_match_independent_f64_reference() {
        let y_row = [0, 255, 64, 192];
        let planes = [
            CpuPlane::new([y_row, y_row].concat(), 4),
            CpuPlane::new(vec![128, 128, 240, 16], 4),
        ];
        for (matrix, range) in [
            (ColorMatrix::Bt601, ColorRange::Full),
            (ColorMatrix::Bt601, ColorRange::Limited),
            (ColorMatrix::Bt709, ColorRange::Full),
            (ColorMatrix::Bt709, ColorRange::Limited),
        ] {
            let Some(transform) = yuv_to_rgb_matrix(matrix, range) else {
                panic!("SDR fixture transform must exist");
            };
            assert!(matches!(
                render_decision(metadata(matrix, range)),
                ColorRenderDecision::Gpu(gpu_transform) if gpu_transform == transform
            ));
            let mut rgba = vec![0_u8; 4 * 2 * 4];
            let result = nv12_to_rgba_into(
                &planes,
                FrameExtent::new(4, 2),
                metadata(matrix, range),
                &transform,
                &mut rgba,
            );
            assert!(result.is_ok());
            let Some(uv_plane) = planes.get(1) else {
                panic!("fixture UV plane must exist");
            };
            for y in 0..2 {
                for x in 0..4 {
                    let Some(&y_value) = y_row.get(x) else {
                        panic!("fixture Y value must exist");
                    };
                    let cb = reference_sample(&uv_plane.bytes, 4, x, y, 0);
                    let cr = reference_sample(&uv_plane.bytes, 4, x, y, 1);
                    let expected = reference_matrix(matrix, range, y_value, cb, cr);
                    let output_index = (y * 4 + x) * 4;
                    let Some(pixel) = rgba.get(output_index..output_index + 3) else {
                        panic!("fixture output pixel must exist");
                    };
                    let actual = match pixel {
                        [red, green, blue] => [*red, *green, *blue],
                        _ => panic!("fixture pixel must have three RGB bytes"),
                    };
                    assert!(
                        actual
                            .iter()
                            .zip(expected.iter())
                            .all(|(actual, expected)| actual.abs_diff(*expected) <= 1),
                        "{matrix:?} {range:?} at ({x},{y}): {actual:?} vs {expected:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn matrix_is_independent_of_primaries_and_gpu_contract_is_explicit() {
        let bt601 = yuv_to_rgb_matrix(ColorMatrix::Bt601, ColorRange::Limited);
        let bt709 = yuv_to_rgb_matrix(ColorMatrix::Bt709, ColorRange::Limited);
        assert_ne!(bt601, bt709);
        let mut color = metadata(ColorMatrix::Bt601, ColorRange::Limited);
        color.primaries = ColorPrimaries::Bt470Bg;
        assert_eq!(yuv_to_rgb_matrix(color.matrix, color.range), bt601);
        assert_eq!(
            render_decision(color),
            ColorRenderDecision::UnsupportedSdrColor
        );
        color.primaries = ColorPrimaries::Bt709;
        assert!(matches!(
            render_decision(color),
            ColorRenderDecision::Gpu(_)
        ));
        assert_eq!(yuv_to_rgb_matrix(color.matrix, color.range), bt601);
        color.transfer = ColorTransfer::Bt709;
        assert!(matches!(
            render_decision(color),
            ColorRenderDecision::CpuRgba(transform) if Some(transform) == bt601
        ));
        color.chroma_horizontal = ChromaHorizontal::Cosited;
        assert!(matches!(
            render_decision(color),
            ColorRenderDecision::CpuRgba(_)
        ));
        color.matrix = ColorMatrix::Unknown;
        assert_eq!(render_decision(color), ColorRenderDecision::Unsupported);
        color.matrix = ColorMatrix::Bt709;
        color.primaries = ColorPrimaries::Adobergb;
        assert_eq!(
            render_decision(color),
            ColorRenderDecision::UnsupportedSdrColor
        );
    }

    #[test]
    fn centered_bilinear_sampling_honors_horizontal_and_vertical_siting() {
        let planes = [
            CpuPlane::new(vec![128; 16], 4),
            CpuPlane::new(vec![16, 128, 128, 240, 240, 16, 16, 240], 4),
        ];
        let mut centered = vec![0_u8; 4 * 4 * 4];
        let centered_color = metadata(ColorMatrix::Bt709, ColorRange::Full);
        let Some(matrix) = yuv_to_rgb_matrix(ColorMatrix::Bt709, ColorRange::Full) else {
            panic!("BT.709 matrix must exist");
        };
        assert!(nv12_to_rgba_into(
            &planes,
            FrameExtent::new(4, 4),
            centered_color,
            &matrix,
            &mut centered,
        )
        .is_ok());
        let mut cosited = vec![0_u8; 4 * 4 * 4];
        let mut cosited_color = centered_color;
        cosited_color.chroma_horizontal = ChromaHorizontal::Cosited;
        cosited_color.chroma_vertical = ChromaVertical::Cosited;
        assert!(nv12_to_rgba_into(
            &planes,
            FrameExtent::new(4, 4),
            cosited_color,
            &matrix,
            &mut cosited,
        )
        .is_ok());
        assert_ne!(centered, cosited);
    }

    #[test]
    fn bilinear_chroma_keeps_fractional_samples_until_final_quantization() {
        let bytes = [0, 0, 255, 255, 255, 255, 0, 0];
        let sample = sample_chroma(
            &bytes,
            4,
            1,
            1,
            true,
            (ChromaHorizontal::Centered, ChromaVertical::Centered),
            (2, 2),
        );
        assert!(sample.fract() > 0.0);
    }
}
