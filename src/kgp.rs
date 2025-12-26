// Copyright 2025 Tomoki Hayashi
// MIT License (https://opensource.org/licenses/MIT)

//! Kitty Graphics Protocol helpers.
//!
//! This module constructs KGP escape sequences and helper “placement” rows used to display images
//! in the terminal.

use std::io::Write;

use image::DynamicImage;
use ratatui::layout::Rect;

const TMUX_START: &str = "\x1bPtmux;\x1b\x1b";
const TMUX_ESCAPE: &str = "\x1b\x1b";
const TMUX_CLOSE: &str = "\x1b\\";

pub fn delete_all(is_tmux: bool) -> Vec<u8> {
    let (start, escape, close) = if is_tmux {
        (TMUX_START, TMUX_ESCAPE, TMUX_CLOSE)
    } else {
        ("\x1b", "\x1b", "")
    };

    let mut buf = Vec::with_capacity(128);
    _ = write!(buf, "{start}_Gq=2,a=d,d=a{escape}\\{close}");
    _ = write!(buf, "{start}_Gq=2,a=d,d=A{escape}\\{close}");
    buf
}

pub fn delete_by_id(id: u32, is_tmux: bool) -> Vec<u8> {
    let (start, escape, close) = if is_tmux {
        (TMUX_START, TMUX_ESCAPE, TMUX_CLOSE)
    } else {
        ("\x1b", "\x1b", "")
    };

    let mut buf = Vec::with_capacity(64);
    _ = write!(buf, "{start}_Gq=2,a=d,d=i,i={id}{escape}\\{close}");
    buf
}

#[derive(Default)]
pub struct KgpState {
    last_area: Option<Rect>,
    last_kgp_id: Option<u32>,
}

impl KgpState {
    pub fn last_area(&self) -> Option<Rect> {
        self.last_area
    }

    pub fn last_kgp_id(&self) -> Option<u32> {
        self.last_kgp_id
    }

    pub fn set_last(&mut self, area: Rect, kgp_id: u32) {
        self.last_area = Some(area);
        self.last_kgp_id = Some(kgp_id);
    }

    /// Invalidate kgp_id while preserving area (for erase_rows on next display).
    pub fn invalidate(&mut self) {
        self.last_kgp_id = None;
    }
}

pub fn place_rows(area: Rect, id: u32) -> Vec<Vec<u8>> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let mut rows = Vec::with_capacity(area.height as usize);
    let (id_extra, r, g, b) = (
        (id >> 24) & 0xff,
        (id >> 16) & 0xff,
        (id >> 8) & 0xff,
        id & 0xff,
    );

    for y in 0..area.height {
        let mut buf = Vec::with_capacity(area.width as usize * 4 + 64);
        _ = write!(buf, "\x1b[38;2;{r};{g};{b}m");
        _ = write!(buf, "\x1b[{};{}H", area.y + y + 1, area.x + 1);
        for x in 0..area.width {
            _ = write!(buf, "\u{10EEEE}");
            _ = write!(
                buf,
                "{}",
                *DIACRITICS.get(y as usize).unwrap_or(&DIACRITICS[0])
            );
            _ = write!(
                buf,
                "{}",
                *DIACRITICS.get(x as usize).unwrap_or(&DIACRITICS[0])
            );
            _ = write!(
                buf,
                "{}",
                *DIACRITICS.get(id_extra as usize).unwrap_or(&DIACRITICS[0])
            );
        }
        _ = write!(buf, "\x1b[0m");
        rows.push(buf);
    }

    rows
}

pub fn erase_rows(area: Rect) -> Vec<Vec<u8>> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let mut rows = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut buf = Vec::with_capacity(48);
        _ = write!(
            buf,
            "\x1b[{};{}H\x1b[{}X",
            area.y + y + 1,
            area.x + 1,
            area.width
        );
        rows.push(buf);
    }
    rows
}

pub fn encode_chunks(img: &DynamicImage, id: u32, is_tmux: bool) -> Vec<Vec<u8>> {
    let (w, h) = (img.width(), img.height());

    let (raw, format): (Vec<u8>, u8) = match img {
        DynamicImage::ImageRgb8(v) => (v.as_raw().clone(), 24),
        DynamicImage::ImageRgba8(v) => (v.as_raw().clone(), 32),
        v => (v.clone().into_rgb8().as_raw().clone(), 24),
    };

    let b64 = base64_simd::STANDARD.encode_to_string(&raw).into_bytes();

    let mut it = b64.chunks(4096).peekable();
    let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(it.len().max(1));

    let (start, escape, close) = if is_tmux {
        (TMUX_START, TMUX_ESCAPE, TMUX_CLOSE)
    } else {
        ("\x1b", "\x1b", "")
    };

    if let Some(first) = it.next() {
        let mut buf = Vec::with_capacity(first.len() + 128);
        _ = write!(
            &mut buf,
            "{start}_Gq=2,a=T,C=1,U=1,f={format},s={w},v={h},i={id},m={};",
            it.peek().is_some() as u8
        );
        buf.extend_from_slice(first);
        _ = write!(&mut buf, "{escape}\\{close}");
        chunks.push(buf);
    }

    while let Some(chunk) = it.next() {
        let mut buf = Vec::with_capacity(chunk.len() + 64);
        _ = write!(&mut buf, "{start}_Gm={};", it.peek().is_some() as u8);
        buf.extend_from_slice(chunk);
        _ = write!(&mut buf, "{escape}\\{close}");
        chunks.push(buf);
    }

    chunks
}

// From yazi's KGP implementation (and kitty docs).
static DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erase_generates_cursor_moves() {
        let area = Rect::new(2, 3, 4, 2);
        let rows = erase_rows(area);
        let bytes = rows.concat();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4;3H"));
        assert!(s.contains("\x1b[5;3H"));
    }
}
