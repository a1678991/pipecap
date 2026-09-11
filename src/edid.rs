//! EDID parsing, decoding and *capping*.
//!
//! Capping means producing a new EDID that no longer advertises any video
//! timing above a given refresh rate. On Apple Silicon the display
//! coprocessor allocates display pipes from the *highest* mode in the EDID,
//! so lowering that maximum is what frees a pipe for another monitor.
//!
//! This module is pure Rust with no platform dependencies so that it can be
//! unit-tested anywhere.

use serde::Serialize;
use std::fmt;
use thiserror::Error;

/// Size of one EDID block.
pub const BLOCK: usize = 128;

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
/// A "dummy" 18-byte display descriptor (tag 0x10), used to blank a DTD slot.
const DUMMY_DESCRIPTOR: [u8; 18] = [0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

#[derive(Debug, Error)]
pub enum EdidError {
    #[error("EDID too short: {0} bytes (need at least 128)")]
    TooShort(usize),
    #[error("EDID length {0} is not a multiple of 128")]
    BadLength(usize),
    #[error("bad EDID header (not an EDID)")]
    BadHeader,
    #[error("checksum mismatch in block {0}")]
    Checksum(usize),
    #[error(
        "the preferred timing in the base block ({0}) is above the cap; refusing to remove it"
    )]
    PreferredExceedsCap(String),
    #[error("max refresh must be positive")]
    BadCap,
}

/// Where a timing was found inside the EDID.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TimingSource {
    BaseDtd { index: usize },
    CtaDtd { block: usize, index: usize },
    CtaVic { block: usize, vic: u8 },
    DisplayId { block: usize, index: usize },
}

impl fmt::Display for TimingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimingSource::BaseDtd { index } => write!(f, "base DTD #{}", index + 1),
            TimingSource::CtaDtd { block, index } => {
                write!(f, "CTA DTD #{} (block {block})", index + 1)
            }
            TimingSource::CtaVic { block, vic } => write!(f, "CTA VIC {vic} (block {block})"),
            TimingSource::DisplayId { block, index } => {
                write!(f, "DisplayID timing #{} (block {block})", index + 1)
            }
        }
    }
}

/// One video timing advertised by the EDID.
#[derive(Debug, Clone, Serialize)]
pub struct Timing {
    pub h_active: u32,
    pub v_active: u32,
    pub h_total: u32,
    pub v_total: u32,
    pub pixel_clock_hz: u64,
    pub refresh_hz: f64,
    pub interlaced: bool,
    pub source: TimingSource,
}

impl Timing {
    fn new(
        h_active: u32,
        v_active: u32,
        h_total: u32,
        v_total: u32,
        pixel_clock_hz: u64,
        interlaced: bool,
        source: TimingSource,
    ) -> Self {
        let denom = (h_total as f64) * (v_total as f64);
        let refresh_hz = if denom > 0.0 {
            pixel_clock_hz as f64 / denom
        } else {
            0.0
        };
        Timing {
            h_active,
            v_active,
            h_total,
            v_total,
            pixel_clock_hz,
            refresh_hz,
            interlaced,
            source,
        }
    }

    /// Pixels per second inside the active area (what the display pipe must render).
    pub fn active_pixel_rate(&self) -> f64 {
        self.h_active as f64 * self.v_active as f64 * self.refresh_hz
    }

    /// Pixels per second including blanking (the link pixel clock).
    pub fn total_pixel_rate(&self) -> f64 {
        self.pixel_clock_hz as f64
    }
}

impl fmt::Display for Timing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{}{} @ {:.2} Hz ({:.2} MHz, {})",
            self.h_active,
            self.v_active,
            if self.interlaced { "i" } else { "" },
            self.refresh_hz,
            self.pixel_clock_hz as f64 / 1e6,
            self.source
        )
    }
}

/// The Display Range Limits descriptor (tag 0xFD) of the base block.
#[derive(Debug, Clone, Serialize)]
pub struct RangeLimits {
    pub descriptor_index: usize,
    pub v_min_hz: u32,
    pub v_max_hz: u32,
    pub h_min_khz: u32,
    pub h_max_khz: u32,
    pub max_pixel_clock_mhz: u32,
}

/// Decoded summary of an EDID.
#[derive(Debug, Clone, Serialize)]
pub struct EdidInfo {
    /// Hex of bytes 8..12 (manufacturer + product code), a stable identifier.
    pub id: String,
    pub manufacturer: String,
    pub product_code: u16,
    pub serial: u32,
    pub week: u8,
    pub year: u16,
    pub version: String,
    pub name: Option<String>,
    pub serial_string: Option<String>,
    pub extension_count: u8,
    pub length: usize,
    pub blocks: Vec<String>,
    pub range_limits: Option<RangeLimits>,
    pub timings: Vec<Timing>,
}

impl EdidInfo {
    /// The (progressive) timing with the highest active pixel rate.
    pub fn max_timing(&self) -> Option<&Timing> {
        self.timings
            .iter()
            .filter(|t| !t.interlaced)
            .max_by(|a, b| a.active_pixel_rate().total_cmp(&b.active_pixel_rate()))
    }

    /// Human readable label, e.g. `TEST 4K160` or `TST 0x0160`.
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) if !n.is_empty() => n.clone(),
            _ => format!("{} 0x{:04x}", self.manufacturer, self.product_code),
        }
    }
}

/// Identifier (hex of bytes 8..12) of an EDID, if long enough.
pub fn id_of(bytes: &[u8]) -> Option<String> {
    bytes.get(8..12).map(hex)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn block_sum(block: &[u8]) -> u8 {
    block.iter().fold(0u8, |a, &b| a.wrapping_add(b))
}

fn fix_checksum(block: &mut [u8]) {
    let s = block_sum(&block[..BLOCK - 1]);
    block[BLOCK - 1] = 0u8.wrapping_sub(s);
}

/// Validate header, length and every block checksum.
pub fn validate(bytes: &[u8]) -> Result<(), EdidError> {
    if bytes.len() < BLOCK {
        return Err(EdidError::TooShort(bytes.len()));
    }
    if bytes.len() % BLOCK != 0 {
        return Err(EdidError::BadLength(bytes.len()));
    }
    if bytes[..8] != HEADER {
        return Err(EdidError::BadHeader);
    }
    for (i, blk) in bytes.chunks(BLOCK).enumerate() {
        if block_sum(blk) != 0 {
            return Err(EdidError::Checksum(i));
        }
    }
    Ok(())
}

fn parse_dtd(d: &[u8], source: TimingSource) -> Option<Timing> {
    let pc = (d[0] as u64 | (d[1] as u64) << 8) * 10_000;
    if pc == 0 {
        return None;
    }
    let ha = d[2] as u32 | ((d[4] as u32) >> 4) << 8;
    let hb = d[3] as u32 | ((d[4] as u32) & 0xf) << 8;
    let va = d[5] as u32 | ((d[7] as u32) >> 4) << 8;
    let vb = d[6] as u32 | ((d[7] as u32) & 0xf) << 8;
    let interlaced = d[17] & 0x80 != 0;
    Some(Timing::new(
        ha,
        va,
        ha + hb,
        va + vb,
        pc,
        interlaced,
        source,
    ))
}

/// CTA-861 short video descriptor table (progressive formats only):
/// (h_active, v_active, h_total, v_total, pixel clock Hz).
fn vic_timing(vic: u8) -> Option<(u32, u32, u32, u32, u64)> {
    Some(match vic {
        1 => (640, 480, 800, 525, 25_175_000),
        2 | 3 => (720, 480, 858, 525, 27_000_000),
        4 => (1280, 720, 1650, 750, 74_250_000),
        16 => (1920, 1080, 2200, 1125, 148_500_000),
        17 | 18 => (720, 576, 864, 625, 27_000_000),
        19 => (1280, 720, 1980, 750, 74_250_000),
        31 => (1920, 1080, 2640, 1125, 148_500_000),
        32 => (1920, 1080, 2750, 1125, 74_250_000),
        33 => (1920, 1080, 2640, 1125, 74_250_000),
        34 => (1920, 1080, 2200, 1125, 74_250_000),
        60 => (1280, 720, 3300, 750, 59_400_000),
        61 => (1280, 720, 3960, 750, 74_250_000),
        62 => (1280, 720, 3300, 750, 74_250_000),
        63 => (1920, 1080, 2200, 1125, 297_000_000),
        64 => (1920, 1080, 2640, 1125, 297_000_000),
        93 => (3840, 2160, 5500, 2250, 297_000_000),
        94 => (3840, 2160, 5280, 2250, 297_000_000),
        95 => (3840, 2160, 4400, 2250, 297_000_000),
        96 => (3840, 2160, 5280, 2250, 594_000_000),
        97 => (3840, 2160, 4400, 2250, 594_000_000),
        98 => (4096, 2160, 5500, 2250, 297_000_000),
        99 => (4096, 2160, 5280, 2250, 297_000_000),
        100 => (4096, 2160, 4400, 2250, 297_000_000),
        101 => (4096, 2160, 5280, 2250, 594_000_000),
        102 => (4096, 2160, 4400, 2250, 594_000_000),
        103 => (3840, 2160, 5500, 2250, 297_000_000),
        104 => (3840, 2160, 5280, 2250, 297_000_000),
        105 => (3840, 2160, 4400, 2250, 297_000_000),
        106 => (3840, 2160, 5280, 2250, 594_000_000),
        107 => (3840, 2160, 4400, 2250, 594_000_000),
        114 | 116 => (3840, 2160, 5500, 2250, 594_000_000),
        115 => (4096, 2160, 5500, 2250, 594_000_000),
        117 | 121 => (3840, 2160, 5280, 2250, 1_188_000_000),
        118 | 122 => (3840, 2160, 4400, 2250, 1_188_000_000),
        119 => (4096, 2160, 5280, 2250, 1_188_000_000),
        120 => (4096, 2160, 4400, 2250, 1_188_000_000),
        194 | 202 => (7680, 4320, 11000, 4500, 1_188_000_000),
        195 | 203 => (7680, 4320, 10800, 4400, 1_188_000_000),
        196 | 204 => (7680, 4320, 9000, 4400, 1_188_000_000),
        197 | 205 => (7680, 4320, 11000, 4500, 2_376_000_000),
        198 | 206 => (7680, 4320, 10800, 4400, 2_376_000_000),
        199 | 207 => (7680, 4320, 9000, 4400, 2_376_000_000),
        200 | 208 => (7680, 4320, 10560, 4500, 4_752_000_000),
        201 | 209 => (7680, 4320, 9000, 4400, 4_752_000_000),
        _ => return None,
    })
}

/// Decode a short video descriptor byte into its VIC.
fn svd_vic(byte: u8) -> u8 {
    let low = byte & 0x7f;
    if (1..=64).contains(&low) {
        low
    } else {
        byte
    }
}

fn vic_as_timing(vic: u8, block: usize) -> Option<Timing> {
    vic_timing(vic).map(|(ha, va, ht, vt, pc)| {
        Timing::new(
            ha,
            va,
            ht,
            vt,
            pc,
            false,
            TimingSource::CtaVic { block, vic },
        )
    })
}

/// A parsed CTA-861 extension block.
struct CtaBlock {
    revision: u8,
    flags: u8,
    /// (tag, payload). For extended tags the first payload byte is the extended tag.
    data_blocks: Vec<(u8, Vec<u8>)>,
    dtds: Vec<[u8; 18]>,
}

fn parse_cta(blk: &[u8]) -> CtaBlock {
    let d = blk[2] as usize;
    let mut data_blocks = Vec::new();
    let mut dtds = Vec::new();
    if d >= 4 {
        let mut i = 4;
        while i < d.min(BLOCK - 1) {
            let tag = blk[i] >> 5;
            let ln = (blk[i] & 0x1f) as usize;
            let end = (i + 1 + ln).min(BLOCK - 1);
            data_blocks.push((tag, blk[i + 1..end].to_vec()));
            i += 1 + ln;
        }
        let mut j = d;
        while j + 18 < BLOCK {
            let c = &blk[j..j + 18];
            if c[0] == 0 && c[1] == 0 {
                break;
            }
            dtds.push(c.try_into().expect("18 bytes"));
            j += 18;
        }
    }
    CtaBlock {
        revision: blk[1],
        flags: blk[3],
        data_blocks,
        dtds,
    }
}

fn build_cta(c: &CtaBlock) -> [u8; BLOCK] {
    let mut out = [0u8; BLOCK];
    out[0] = 0x02;
    out[1] = c.revision;
    out[3] = c.flags;
    let mut i = 4;
    for (tag, body) in &c.data_blocks {
        out[i] = (tag << 5) | (body.len() as u8 & 0x1f);
        out[i + 1..i + 1 + body.len()].copy_from_slice(body);
        i += 1 + body.len();
    }
    out[2] = i as u8;
    for dtd in &c.dtds {
        out[i..i + 18].copy_from_slice(dtd);
        i += 18;
    }
    fix_checksum(&mut out);
    out
}

/// Size of one timing descriptor in a DisplayID Type I (0x03) / Type VII (0x22) block.
///
/// Type I descriptors are always 20 bytes. Type VII descriptors are 20 bytes
/// plus the "payload bytes" count stored in bits 6..4 of the block revision
/// byte (DisplayID 2.1), so a decoder must honour that field to stay aligned.
fn displayid_entry_size(tag: u8, revision_byte: u8) -> usize {
    if tag == 0x22 {
        20 + ((revision_byte & 0x70) >> 4) as usize
    } else {
        20
    }
}

/// Pixel clock unit: DisplayID 1.x Type I uses 10 kHz, DisplayID 2.x Type VII uses 1 kHz.
fn displayid_clock_unit(tag: u8) -> u64 {
    if tag == 0x22 {
        1_000
    } else {
        10_000
    }
}

fn parse_displayid_timing(d: &[u8], unit: u64, source: TimingSource) -> Timing {
    let pc = ((d[0] as u64) | (d[1] as u64) << 8 | (d[2] as u64) << 16) + 1;
    let ha = (d[4] as u32 | (d[5] as u32) << 8) + 1;
    let hb = (d[6] as u32 | (d[7] as u32) << 8) + 1;
    let va = (d[12] as u32 | (d[13] as u32) << 8) + 1;
    let vb = (d[14] as u32 | (d[15] as u32) << 8) + 1;
    let interlaced = d[3] & 0x10 != 0;
    Timing::new(ha, va, ha + hb, va + vb, pc * unit, interlaced, source)
}

/// A parsed DisplayID extension block (as embedded in EDID, tag 0x70).
struct DisplayIdBlock {
    version: u8,
    product_type: u8,
    ext_count: u8,
    /// (tag, revision, payload)
    blocks: Vec<(u8, u8, Vec<u8>)>,
}

fn parse_displayid(blk: &[u8]) -> DisplayIdBlock {
    let sec_len = blk[2] as usize;
    let end = (5 + sec_len).min(BLOCK - 1);
    let mut blocks = Vec::new();
    let mut i = 5;
    while i + 3 <= end {
        let (tag, rev, ln) = (blk[i], blk[i + 1], blk[i + 2] as usize);
        if tag == 0 && ln == 0 {
            break;
        }
        let e = (i + 3 + ln).min(end);
        blocks.push((tag, rev, blk[i + 3..e].to_vec()));
        i += 3 + ln;
    }
    DisplayIdBlock {
        version: blk[1],
        product_type: blk[3],
        ext_count: blk[4],
        blocks,
    }
}

fn build_displayid(d: &DisplayIdBlock) -> [u8; BLOCK] {
    let mut sec = vec![d.version, 0, d.product_type, d.ext_count];
    for (tag, rev, body) in &d.blocks {
        sec.extend_from_slice(&[*tag, *rev, body.len() as u8]);
        sec.extend_from_slice(body);
    }
    sec[1] = (sec.len() - 4) as u8;
    let chk = 0u8.wrapping_sub(block_sum(&sec));
    let mut out = [0u8; BLOCK];
    out[0] = 0x70;
    out[1..1 + sec.len()].copy_from_slice(&sec);
    out[1 + sec.len()] = chk;
    fix_checksum(&mut out);
    out
}

fn descriptor_text(d: &[u8]) -> String {
    let s: String = d[5..18]
        .iter()
        .take_while(|&&c| c != 0x0a && c != 0)
        .map(|&c| c as char)
        .collect();
    s.trim().to_string()
}

fn parse_range_limits(d: &[u8], index: usize) -> RangeLimits {
    let flags = d[4];
    let v_off = if flags & 0x02 != 0 { 255 } else { 0 };
    let v_min_off = if flags & 0x03 == 0x03 { 255 } else { 0 };
    let h_off = if flags & 0x08 != 0 { 255 } else { 0 };
    let h_min_off = if flags & 0x0c == 0x0c { 255 } else { 0 };
    RangeLimits {
        descriptor_index: index,
        v_min_hz: d[5] as u32 + v_min_off,
        v_max_hz: d[6] as u32 + v_off,
        h_min_khz: d[7] as u32 + h_min_off,
        h_max_khz: d[8] as u32 + h_off,
        max_pixel_clock_mhz: d[9] as u32 * 10,
    }
}

/// Parse an EDID (base block plus extensions). Checksums are *not* required
/// to be valid here so that damaged EDIDs can still be inspected; use
/// [`validate`] for strict checking.
pub fn parse(bytes: &[u8]) -> Result<EdidInfo, EdidError> {
    if bytes.len() < BLOCK {
        return Err(EdidError::TooShort(bytes.len()));
    }
    if bytes[..8] != HEADER {
        return Err(EdidError::BadHeader);
    }
    let mfg = (bytes[8] as u16) << 8 | bytes[9] as u16;
    let manufacturer: String = [10u16, 5, 0]
        .iter()
        .map(|s| (((mfg >> s) & 0x1f) as u8 + 64) as char)
        .collect();
    let mut info = EdidInfo {
        id: hex(&bytes[8..12]),
        manufacturer,
        product_code: bytes[10] as u16 | (bytes[11] as u16) << 8,
        serial: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        week: bytes[16],
        year: 1990 + bytes[17] as u16,
        version: format!("{}.{}", bytes[18], bytes[19]),
        name: None,
        serial_string: None,
        extension_count: bytes[126],
        length: bytes.len(),
        blocks: vec!["base".to_string()],
        range_limits: None,
        timings: Vec::new(),
    };
    for i in 0..4 {
        let d = &bytes[54 + 18 * i..54 + 18 * i + 18];
        if d[0] == 0 && d[1] == 0 {
            match d[3] {
                0xfc => info.name = Some(descriptor_text(d)),
                0xff => info.serial_string = Some(descriptor_text(d)),
                0xfd => info.range_limits = Some(parse_range_limits(d, i)),
                _ => {}
            }
        } else if let Some(t) = parse_dtd(d, TimingSource::BaseDtd { index: i }) {
            info.timings.push(t);
        }
    }
    for (block, blk) in bytes.chunks(BLOCK).enumerate().skip(1) {
        if blk.len() < BLOCK {
            info.blocks
                .push(format!("truncated block ({} bytes)", blk.len()));
            break;
        }
        match blk[0] {
            0x02 => {
                let cta = parse_cta(blk);
                info.blocks.push(format!("CTA-861 rev {}", cta.revision));
                for (tag, body) in &cta.data_blocks {
                    if *tag == 2 {
                        for &b in body {
                            if let Some(t) = vic_as_timing(svd_vic(b), block) {
                                info.timings.push(t);
                            }
                        }
                    }
                }
                for (i, d) in cta.dtds.iter().enumerate() {
                    if let Some(t) = parse_dtd(d, TimingSource::CtaDtd { block, index: i }) {
                        info.timings.push(t);
                    }
                }
            }
            0x70 => {
                let did = parse_displayid(blk);
                info.blocks.push(format!(
                    "DisplayID v{:x}.{:x}",
                    did.version >> 4,
                    did.version & 0xf
                ));
                let mut index = 0;
                for (tag, rev, body) in &did.blocks {
                    if *tag == 0x03 || *tag == 0x22 {
                        let sz = displayid_entry_size(*tag, *rev);
                        let unit = displayid_clock_unit(*tag);
                        for chunk in body.chunks_exact(sz) {
                            info.timings.push(parse_displayid_timing(
                                chunk,
                                unit,
                                TimingSource::DisplayId { block, index },
                            ));
                            index += 1;
                        }
                    }
                }
            }
            0xf0 => info.blocks.push("block map".to_string()),
            other => info.blocks.push(format!("unknown extension 0x{other:02x}")),
        }
    }
    Ok(info)
}

/// Options for [`cap`].
#[derive(Debug, Clone)]
pub struct CapOptions {
    /// Timings with a refresh rate above this are removed.
    pub max_hz: f64,
    /// Value to write into the range-limits "max pixel clock" field. When
    /// `None` it is derived from the highest remaining timing.
    pub max_pixel_clock_mhz: Option<u32>,
}

/// Result of [`cap`].
#[derive(Debug, Clone, Serialize)]
pub struct CapResult {
    #[serde(serialize_with = "ser_hex")]
    pub bytes: Vec<u8>,
    pub dropped: Vec<Timing>,
    pub notes: Vec<String>,
    pub max_timing: Option<Timing>,
    /// True when nothing had to change (the EDID was already within the cap).
    pub unchanged: bool,
}

fn ser_hex<S: serde::Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex(b))
}

/// Produce a copy of `bytes` that advertises no timing above `opts.max_hz`.
///
/// Every touched block gets its checksum recomputed and the result is
/// validated before being returned. The base-block preferred timing is never
/// removed; if it is above the cap an error is returned instead.
pub fn cap(bytes: &[u8], opts: &CapOptions) -> Result<CapResult, EdidError> {
    if opts.max_hz.is_nan() || opts.max_hz <= 0.0 {
        return Err(EdidError::BadCap);
    }
    validate(bytes)?;
    // Rates within half a hertz of the cap are kept (144.05 Hz still counts as 144 Hz).
    let limit = opts.max_hz + 0.5;
    let mut out = bytes.to_vec();
    let mut dropped = Vec::new();
    let mut notes = Vec::new();

    // --- base block DTDs
    for i in 0..4 {
        let o = 54 + 18 * i;
        if let Some(t) = parse_dtd(&out[o..o + 18], TimingSource::BaseDtd { index: i }) {
            if t.refresh_hz > limit {
                if i == 0 {
                    return Err(EdidError::PreferredExceedsCap(t.to_string()));
                }
                out[o..o + 18].copy_from_slice(&DUMMY_DESCRIPTOR);
                dropped.push(t);
            }
        }
    }

    // --- extension blocks
    let nblocks = out.len() / BLOCK;
    for block in 1..nblocks {
        let off = block * BLOCK;
        match out[off] {
            0x02 => {
                let mut cta = parse_cta(&out[off..off + BLOCK]);
                let mut changed = false;
                // Which SVD indices (in the Video Data Block) survive; needed to remap Y420CMDB.
                let mut kept_svd: Vec<bool> = Vec::new();
                for (tag, body) in cta.data_blocks.iter_mut() {
                    if *tag == 2 {
                        let mut nb = Vec::with_capacity(body.len());
                        for &b in body.iter() {
                            let vic = svd_vic(b);
                            let keep = match vic_as_timing(vic, block) {
                                Some(t) if t.refresh_hz > limit => {
                                    dropped.push(t);
                                    false
                                }
                                Some(_) => true,
                                None => {
                                    notes.push(format!("kept unknown VIC {vic} (block {block})"));
                                    true
                                }
                            };
                            kept_svd.push(keep);
                            if keep {
                                nb.push(b);
                            }
                        }
                        if nb.len() != body.len() {
                            changed = true;
                            *body = nb;
                        }
                    } else if *tag == 7 && body.first() == Some(&14) {
                        // YCbCr 4:2:0 Video Data Block: list of VICs.
                        let mut nb = vec![14u8];
                        for &b in &body[1..] {
                            match vic_as_timing(b, block) {
                                Some(t) if t.refresh_hz > limit => {
                                    changed = true;
                                    if !dropped.iter().any(|d| d.source == t.source) {
                                        dropped.push(t);
                                    }
                                }
                                _ => nb.push(b),
                            }
                        }
                        *body = nb;
                    }
                }
                // YCbCr 4:2:0 Capability Map: bit i refers to the i-th SVD of the VDB.
                if changed && kept_svd.iter().any(|k| !k) {
                    for (tag, body) in cta.data_blocks.iter_mut() {
                        if *tag == 7 && body.first() == Some(&15) && body.len() > 1 {
                            let old = &body[1..];
                            let mut newbits = vec![0u8; old.len()];
                            let mut j = 0;
                            for (i, &k) in kept_svd.iter().enumerate() {
                                if k {
                                    if old.get(i / 8).map(|b| b >> (i % 8) & 1) == Some(1) {
                                        newbits[j / 8] |= 1 << (j % 8);
                                    }
                                    j += 1;
                                }
                            }
                            let mut nb = vec![15u8];
                            nb.extend_from_slice(&newbits);
                            *body = nb;
                            notes.push(format!(
                                "remapped YCbCr 4:2:0 capability map (block {block})"
                            ));
                        }
                    }
                }
                // A Video Data Block with no VICs left (or an empty 4:2:0 block) is dropped
                // entirely; a 4:2:0 capability map without a VDB is meaningless too.
                let vdb_empty = cta
                    .data_blocks
                    .iter()
                    .any(|(tag, body)| *tag == 2 && body.is_empty());
                if vdb_empty {
                    changed = true;
                    cta.data_blocks.retain(|(tag, body)| {
                        !(*tag == 2 && body.is_empty())
                            && !(*tag == 7 && matches!(body.first(), Some(14) | Some(15)))
                    });
                    notes.push(format!("removed empty video data block(s) (block {block})"));
                } else if cta
                    .data_blocks
                    .iter()
                    .any(|(tag, body)| *tag == 7 && body.len() == 1 && body[0] == 14)
                {
                    changed = true;
                    cta.data_blocks
                        .retain(|(tag, body)| !(*tag == 7 && body.len() == 1 && body[0] == 14));
                }
                let before = cta.dtds.len();
                let mut idx = 0;
                cta.dtds.retain(|d| {
                    let keep = match parse_dtd(d, TimingSource::CtaDtd { block, index: idx }) {
                        Some(t) if t.refresh_hz > limit => {
                            dropped.push(t);
                            false
                        }
                        _ => true,
                    };
                    idx += 1;
                    keep
                });
                if changed || cta.dtds.len() != before {
                    out[off..off + BLOCK].copy_from_slice(&build_cta(&cta));
                }
            }
            0x70 => {
                let mut did = parse_displayid(&out[off..off + BLOCK]);
                let mut changed = false;
                let mut index = 0;
                let mut newblocks = Vec::new();
                for (tag, rev, body) in did.blocks.drain(..) {
                    if tag == 0x03 || tag == 0x22 {
                        let sz = displayid_entry_size(tag, rev);
                        let unit = displayid_clock_unit(tag);
                        let mut nb = Vec::with_capacity(body.len());
                        for chunk in body.chunks_exact(sz) {
                            let t = parse_displayid_timing(
                                chunk,
                                unit,
                                TimingSource::DisplayId { block, index },
                            );
                            index += 1;
                            if t.refresh_hz > limit {
                                dropped.push(t);
                                changed = true;
                            } else {
                                nb.extend_from_slice(chunk);
                            }
                        }
                        if nb.is_empty() {
                            notes.push(format!(
                                "removed empty DisplayID timing block 0x{tag:02x} (block {block})"
                            ));
                            continue;
                        }
                        newblocks.push((tag, rev, nb));
                    } else {
                        newblocks.push((tag, rev, body));
                    }
                }
                did.blocks = newblocks;
                if changed {
                    out[off..off + BLOCK].copy_from_slice(&build_displayid(&did));
                }
            }
            _ => {}
        }
    }

    // --- range limits descriptor
    let after = parse(&out)?;
    let max_timing = after.max_timing().cloned();
    if let (Some(rl), false) = (&after.range_limits, dropped.is_empty()) {
        let o = 54 + 18 * rl.descriptor_index;
        let mut v_max = rl.v_max_hz;
        let cap_hz = opts.max_hz.floor() as u32;
        if cap_hz < v_max {
            v_max = cap_hz;
            if v_max <= 255 {
                out[o + 4] &= !0x03; // clear vertical offset flags
                out[o + 6] = v_max as u8;
            } else {
                out[o + 4] = (out[o + 4] & !0x03) | 0x02;
                out[o + 6] = (v_max - 255) as u8;
            }
            notes.push(format!(
                "range limits: max vertical rate {} -> {} Hz",
                rl.v_max_hz, v_max
            ));
        }
        let derived = after
            .timings
            .iter()
            .map(|t| (t.pixel_clock_hz as f64 / 1e7).ceil() as u32 * 10)
            .max()
            .unwrap_or(rl.max_pixel_clock_mhz);
        let new_pclk = opts
            .max_pixel_clock_mhz
            .unwrap_or(derived)
            .min(rl.max_pixel_clock_mhz);
        if new_pclk != rl.max_pixel_clock_mhz && new_pclk > 0 {
            out[o + 9] = (new_pclk / 10).min(255) as u8;
            notes.push(format!(
                "range limits: max pixel clock {} -> {} MHz",
                rl.max_pixel_clock_mhz,
                (new_pclk / 10).min(255) * 10
            ));
        }
    }
    fix_checksum(&mut out[..BLOCK]);
    validate(&out)?;
    let unchanged = out == bytes;
    Ok(CapResult {
        bytes: out,
        dropped,
        notes,
        max_timing,
        unchanged,
    })
}

/// Highest whole refresh rate at which `w`x`h` stays under `active_limit` pixels/s.
pub fn max_hz_for(w: u32, h: u32, active_limit: f64) -> u32 {
    if w == 0 || h == 0 {
        return 0;
    }
    (active_limit / (w as f64 * h as f64)).floor() as u32
}

/// Render a multi-line human readable decode.
pub fn describe(info: &EdidInfo) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{} (id {}, {} product 0x{:04x}, serial {}, week {} of {}, EDID {}, {} bytes)\n",
        info.label(),
        info.id,
        info.manufacturer,
        info.product_code,
        info.serial,
        info.week,
        info.year,
        info.version,
        info.length
    ));
    s.push_str(&format!("blocks: {}\n", info.blocks.join(", ")));
    if let Some(rl) = &info.range_limits {
        s.push_str(&format!(
            "range limits: V {}-{} Hz, H {}-{} kHz, max pixel clock {} MHz\n",
            rl.v_min_hz, rl.v_max_hz, rl.h_min_khz, rl.h_max_khz, rl.max_pixel_clock_mhz
        ));
    }
    s.push_str("timings:\n");
    let mut ts: Vec<&Timing> = info.timings.iter().collect();
    ts.sort_by(|a, b| b.active_pixel_rate().total_cmp(&a.active_pixel_rate()));
    for t in ts {
        s.push_str(&format!(
            "  {:<52} active {:>6.3} Gpx/s  total {:>6.3} Gpx/s\n",
            t.to_string(),
            t.active_pixel_rate() / 1e9,
            t.total_pixel_rate() / 1e9
        ));
    }
    if let Some(m) = info.max_timing() {
        s.push_str(&format!(
            "max: {}x{} @ {:.2} Hz\n",
            m.h_active, m.v_active, m.refresh_hz
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const DP160: &[u8] = include_bytes!("../tests/fixtures/mon_4k160_dp.bin");
    const HDMI240: &[u8] = include_bytes!("../tests/fixtures/mon_4k240_hdmi.bin");

    #[test]
    fn parses_4k160() {
        validate(DP160).unwrap();
        let i = parse(DP160).unwrap();
        assert_eq!(i.manufacturer, "TST");
        assert_eq!(i.name.as_deref(), Some("TEST 4K160"));
        assert_eq!(i.id, "52746001");
        assert_eq!(i.blocks, vec!["base", "CTA-861 rev 3", "DisplayID v1.2"]);
        let m = i.max_timing().unwrap();
        assert_eq!((m.h_active, m.v_active), (3840, 2160));
        assert!((m.refresh_hz - 160.0).abs() < 0.01);
        assert_eq!(i.range_limits.as_ref().unwrap().v_max_hz, 160);
    }

    #[test]
    fn parses_4k240_with_block_map() {
        validate(HDMI240).unwrap();
        let i = parse(HDMI240).unwrap();
        assert_eq!(i.manufacturer, "TST");
        assert_eq!(i.blocks[1], "block map");
        let m = i.max_timing().unwrap();
        assert!((m.refresh_hz - 240.08).abs() < 0.01);
        assert!(i
            .timings
            .iter()
            .any(|t| t.source == TimingSource::CtaVic { block: 2, vic: 118 }));
    }

    #[test]
    fn cap_4k160_to_144_drops_only_160() {
        let r = cap(
            DP160,
            &CapOptions {
                max_hz: 144.0,
                max_pixel_clock_mhz: None,
            },
        )
        .unwrap();
        assert_eq!(r.dropped.len(), 1);
        assert!((r.dropped[0].refresh_hz - 160.0).abs() < 0.01);
        assert_eq!(r.bytes.len(), DP160.len());
        validate(&r.bytes).unwrap();
        let i = parse(&r.bytes).unwrap();
        assert!((i.max_timing().unwrap().refresh_hz - 144.0).abs() < 0.01);
        assert_eq!(i.range_limits.as_ref().unwrap().v_max_hz, 144);
        assert_eq!(i.range_limits.as_ref().unwrap().max_pixel_clock_mhz, 1290);
        assert_eq!(i.id, "52746001", "identity bytes must not change");
        assert!(!r.unchanged);
        // DisplayID section shrank to a single 20-byte timing.
        assert_eq!(r.bytes[256 + 2], 23);
    }

    #[test]
    fn cap_to_100_drops_vic_118_and_cta_dtd() {
        let r = cap(
            DP160,
            &CapOptions {
                max_hz: 100.0,
                max_pixel_clock_mhz: None,
            },
        )
        .unwrap();
        assert!(r
            .dropped
            .iter()
            .any(|t| t.source == TimingSource::CtaVic { block: 1, vic: 118 }));
        assert!(r
            .dropped
            .iter()
            .any(|t| matches!(t.source, TimingSource::CtaDtd { .. })));
        validate(&r.bytes).unwrap();
        let i = parse(&r.bytes).unwrap();
        assert!(i.timings.iter().all(|t| t.refresh_hz <= 100.05));
        assert!(i
            .timings
            .iter()
            .any(|t| t.source == TimingSource::CtaVic { block: 1, vic: 97 }));
    }

    #[test]
    fn cap_4k240_keeps_144_and_below() {
        let r = cap(
            HDMI240,
            &CapOptions {
                max_hz: 144.0,
                max_pixel_clock_mhz: None,
            },
        )
        .unwrap();
        assert_eq!(r.dropped.len(), 1);
        let i = parse(&r.bytes).unwrap();
        assert_eq!(
            i.timings
                .iter()
                .filter(|t| matches!(t.source, TimingSource::DisplayId { .. }))
                .count(),
            3
        );
        assert!((i.max_timing().unwrap().refresh_hz - 144.05).abs() < 0.01);
    }

    #[test]
    fn cap_above_max_is_noop() {
        let r = cap(
            DP160,
            &CapOptions {
                max_hz: 500.0,
                max_pixel_clock_mhz: None,
            },
        )
        .unwrap();
        assert!(r.dropped.is_empty());
        assert!(r.unchanged);
        assert_eq!(r.bytes, DP160);
    }

    #[test]
    fn cap_is_idempotent() {
        let o = CapOptions {
            max_hz: 144.0,
            max_pixel_clock_mhz: None,
        };
        let a = cap(DP160, &o).unwrap();
        let b = cap(&a.bytes, &o).unwrap();
        assert!(b.unchanged);
        assert_eq!(a.bytes, b.bytes);
    }

    #[test]
    fn refuses_to_drop_preferred_timing() {
        let e = cap(
            DP160,
            &CapOptions {
                max_hz: 30.0,
                max_pixel_clock_mhz: None,
            },
        )
        .unwrap_err();
        assert!(matches!(e, EdidError::PreferredExceedsCap(_)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            validate(&[0u8; 100]),
            Err(EdidError::TooShort(100))
        ));
        let mut bad = DP160.to_vec();
        bad[200] ^= 0xff;
        assert!(matches!(validate(&bad), Err(EdidError::Checksum(1))));
        bad[0] = 1;
        assert!(matches!(validate(&bad), Err(EdidError::BadHeader)));
    }

    #[test]
    fn vic_table_refresh_rates() {
        for (vic, hz) in [
            (1u8, 60u32),
            (4, 60),
            (16, 60),
            (19, 50),
            (31, 50),
            (32, 24),
            (33, 25),
            (34, 30),
            (60, 24),
            (61, 25),
            (62, 30),
            (63, 120),
            (64, 100),
            (93, 24),
            (94, 25),
            (95, 30),
            (96, 50),
            (97, 60),
            (114, 48),
            (117, 100),
            (118, 120),
            (194, 24),
            (195, 25),
            (196, 30),
            (197, 48),
            (198, 50),
            (199, 60),
            (200, 100),
            (201, 120),
        ] {
            let t = vic_as_timing(vic, 1).unwrap_or_else(|| panic!("VIC {vic} missing"));
            assert_eq!(t.refresh_hz.round() as u32, hz, "VIC {vic}");
        }
        assert!(
            vic_as_timing(5, 1).is_none(),
            "interlaced VICs are not in the table"
        );
    }

    #[test]
    fn cap_below_every_vic_removes_the_video_data_block() {
        // Give the 4K160 fixture a 3840x2160 @ 20 Hz preferred timing (198 MHz,
        // htotal 4400, vtotal 2250) so that a 20 Hz cap keeps the preferred timing
        // but removes every VIC, which must remove the Video Data Block itself.
        let mut e = DP160.to_vec();
        let dtd = [
            0x58, 0x4d, 0x00, 0x30, 0xf2, 0x70, 0x5a, 0x80, 0xb0, 0x58, 0x8a, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x1e,
        ];
        e[54..72].copy_from_slice(&dtd);
        fix_checksum(&mut e[..BLOCK]);
        let r = cap(
            &e,
            &CapOptions {
                max_hz: 20.0,
                max_pixel_clock_mhz: None,
            },
        )
        .unwrap();
        validate(&r.bytes).unwrap();
        let i = parse(&r.bytes).unwrap();
        assert!(
            i.timings.iter().all(|t| t.refresh_hz <= 20.5),
            "{:?}",
            i.timings
        );
        let cta = parse_cta(&r.bytes[128..256]);
        assert!(
            !cta.data_blocks.iter().any(|(tag, _)| *tag == 2),
            "VDB should be gone"
        );
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("removed empty video data block")));
    }

    #[test]
    fn displayid_descriptor_sizes() {
        assert_eq!(displayid_entry_size(0x03, 0x01), 20);
        assert_eq!(displayid_entry_size(0x22, 0x00), 20);
        assert_eq!(displayid_entry_size(0x22, 0x12), 21);
        assert_eq!(displayid_entry_size(0x22, 0x72), 27);
        assert_eq!(displayid_clock_unit(0x03), 10_000);
        assert_eq!(displayid_clock_unit(0x22), 1_000);
    }

    #[test]
    fn max_hz_helper() {
        assert_eq!(max_hz_for(3840, 2160, 1_274_019_840.0), 153);
        assert_eq!(max_hz_for(0, 0, 1.0), 0);
    }
}
