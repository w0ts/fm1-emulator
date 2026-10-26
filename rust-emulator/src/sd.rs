// SPDX-License-Identifier: GPL-3.0-only
// An SD card on SPI1, the LCD's bus (Optimist's SD mod, firmware/hal/fm1_sd_bus.h): CLK PC9, the card's DI on PC10,
// its DO on PC8 (the LCD's D/C), its CS through an inverter from PC7 (the LCD's CS): the card is selected whenever
// PC7 is driven high. A port of Optimist's host card model (tests/sd_card_model.h): SDHC in SPI mode, CRC7 on
// commands (always on CMD0 / CMD8, on every command after CMD59), CRC16 on blocks both ways, CMD17 / CMD18 + CMD12,
// CMD24 / CMD25 (0xFC tokens, 0xFD stop), CMD9 (CSD), CMD6. Answers at once: no busy, no Nac, no timing.
// What it models is what the firmware assumes of the hardware, not what the hardware is: which way SPI1 reads (here
// every byte clocked out returns the card's byte in BUF, transmit or receive) and that a CMD25 stays open while the
// card is deselected between blocks (here it does) are the open questions of docs/SD-CARD-DESIGN.md, not answered.
// The card is a raw image file (a FAT16 / FAT32 volume with no MBR, or a whole card with one), written back block by
// block. Select it with --sd PATH (ui.rs) or FM1_SD=PATH.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub static IMAGE: OnceLock<PathBuf> = OnceLock::new();

/// The image the emulator's card holds: --sd, else FM1_SD, else none (no card: every byte reads 0xFF).
pub fn image_path() -> Option<PathBuf> {
    IMAGE
        .get()
        .cloned()
        .or_else(|| std::env::var_os("FM1_SD").map(PathBuf::from))
}

pub struct SdCard {
    img: Vec<u8>,
    file: Option<File>,
    selected: bool,
    app: bool,
    idle: bool,
    crc_on: bool,
    cmd: [u8; 6],
    cmdlen: usize,
    out: Vec<u8>,
    outpos: usize,
    wstate: u8, // 0, 1 waiting for the token, 2 the data
    wmulti: bool,
    waddr: u32,
    wbuf: Vec<u8>,
    streaming: bool, // CMD18
    rsec: u32,
    pub blocks_read: u64,
    pub blocks_written: u64,
    pub crc_refused: u64,
}

fn crc16(p: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for &b in p {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x1021
            } else {
                c << 1
            };
        }
    }
    c
}

fn crc7(p: &[u8]) -> u8 {
    let mut c: u8 = 0;
    for &b in p {
        let mut b = b;
        for _ in 0..8 {
            c <<= 1;
            if (b ^ c) & 0x80 != 0 {
                c ^= 0x09;
            }
            b <<= 1;
        }
    }
    (c << 1) | 1
}

impl SdCard {
    /// A card holding the image at path (read and written back), its size whole 512-byte sectors.
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("SD image {}: {e}", path.display()))?;
        let mut img = Vec::new();
        file.read_to_end(&mut img)
            .map_err(|e| format!("SD image {}: {e}", path.display()))?;
        img.truncate(img.len() / 512 * 512);
        if img.len() < 1024 * 512 {
            return Err(format!("SD image {}: under 512 KB", path.display()));
        }
        Ok(Self::with_image(img, Some(file)))
    }

    /// A card on an image in memory (tests).
    pub fn with_image(img: Vec<u8>, file: Option<File>) -> Self {
        Self {
            img,
            file,
            selected: false,
            app: false,
            idle: true,
            crc_on: false,
            cmd: [0; 6],
            cmdlen: 0,
            out: Vec::new(),
            outpos: 0,
            wstate: 0,
            wmulti: false,
            waddr: 0,
            wbuf: Vec::with_capacity(514),
            streaming: false,
            rsec: 0,
            blocks_read: 0,
            blocks_written: 0,
            crc_refused: 0,
        }
    }

    pub fn image(&self) -> &[u8] {
        &self.img
    }

    fn sectors(&self) -> u32 {
        (self.img.len() / 512) as u32
    }

    /// CS: a card deselected drops a command half sent and what it had left to say; a CMD25 stays open.
    pub fn select(&mut self, on: bool) {
        if on == self.selected {
            return;
        }
        self.selected = on;
        if !on {
            self.cmdlen = 0;
            self.out.clear();
            self.outpos = 0;
        }
    }

    pub fn selected(&self) -> bool {
        self.selected
    }

    fn q(&mut self, b: u8) {
        self.out.push(b);
    }

    fn q_block(&mut self, s: u32, gap: usize) {
        let at = s as usize * 512;
        let c = crc16(&self.img[at..at + 512]);
        for _ in 0..gap {
            self.q(0xFF);
        }
        self.q(0xFE);
        self.out.extend_from_slice(&self.img[at..at + 512].to_vec());
        self.q((c >> 8) as u8);
        self.q(c as u8);
        self.blocks_read += 1;
    }

    fn stream_more(&mut self) {
        if self.rsec >= self.sectors() {
            self.q(0xFF);
            self.q(0x08); // an error token: out of range
            self.streaming = false;
            return;
        }
        let s = self.rsec;
        self.q_block(s, 1);
        self.rsec += 1;
    }

    fn csd(&self) -> [u8; 16] {
        let mut c = [0u8; 16];
        let cs = self.sectors() / 1024 - 1; // CSD v2: (C_SIZE + 1) x 512 KB
        c[0] = 0x40;
        c[7] = ((cs >> 16) & 0x3F) as u8;
        c[8] = (cs >> 8) as u8;
        c[9] = cs as u8;
        c
    }

    fn command(&mut self) {
        let idx = self.cmd[0] & 0x3F;
        let arg = u32::from_be_bytes([self.cmd[1], self.cmd[2], self.cmd[3], self.cmd[4]]);
        let app = std::mem::take(&mut self.app);
        let idle = self.idle as u8;
        self.q(0xFF); // NCR
        if (self.crc_on || idx == 0 || idx == 8) && crc7(&self.cmd[..5]) != self.cmd[5] {
            self.crc_refused += 1;
            self.q(idle | 0x08);
            return;
        }
        match idx {
            59 => {
                self.crc_on = arg & 1 != 0;
                self.q(idle);
            }
            6 => {
                if self.idle {
                    self.q(0x05);
                    return;
                }
                let mut st = [0u8; 64];
                st[16] = (arg & 15) as u8;
                let c = crc16(&st);
                self.q(0);
                self.q(0xFF);
                self.q(0xFE);
                self.out.extend_from_slice(&st);
                self.q((c >> 8) as u8);
                self.q(c as u8);
            }
            0 => {
                self.idle = true;
                self.streaming = false;
                self.wstate = 0;
                self.q(0x01);
            }
            8 => {
                self.q(idle);
                self.q(0);
                self.q(0);
                self.q(((arg >> 8) & 15) as u8);
                self.q(arg as u8);
            }
            55 => {
                self.app = true;
                self.q(idle);
            }
            41 => {
                if !app {
                    self.q(0x05);
                    return;
                }
                self.idle = false;
                self.q(0x00);
            }
            58 => {
                self.q(idle);
                self.q(0xC0); // powered up, high capacity
                self.q(0xFF);
                self.q(0x80);
                self.q(0);
            }
            16 => self.q(if arg == 512 { 0 } else { 0x40 }),
            9 => {
                let c = self.csd();
                self.q(0);
                self.q(0xFF);
                self.q(0xFE);
                self.out.extend_from_slice(&c);
                let k = crc16(&c);
                self.q((k >> 8) as u8);
                self.q(k as u8);
            }
            12 => {
                self.streaming = false;
                self.q(0x00);
                self.q(0x00);
                self.q(0x00);
            }
            17 | 18 | 24 | 25 => {
                if self.idle {
                    self.q(0x05);
                    return;
                }
                if arg >= self.sectors() {
                    self.q(0x40);
                    return;
                }
                self.q(0);
                match idx {
                    24 | 25 => {
                        self.wstate = 1;
                        self.waddr = arg;
                        self.wmulti = idx == 25;
                    }
                    18 => {
                        self.streaming = true;
                        self.rsec = arg;
                    }
                    _ => self.q_block(arg, 2),
                }
            }
            _ => self.q(0x04),
        }
    }

    fn write_back(&mut self, s: u32) {
        let at = s as usize * 512;
        if let Some(f) = self.file.as_mut() {
            let ok = f.seek(SeekFrom::Start(at as u64)).is_ok()
                && f.write_all(&self.img[at..at + 512]).is_ok();
            if !ok {
                eprintln!("SD image: block {s} not written back");
            }
        }
    }

    fn take_block(&mut self) {
        self.wstate = 0;
        self.out.clear();
        self.outpos = 0;
        let (data, crc) = self.wbuf.split_at(512);
        if self.crc_on && crc16(data) != u16::from_be_bytes([crc[0], crc[1]]) {
            self.crc_refused += 1;
            self.q(0xEB);
            return;
        }
        let at = self.waddr as usize * 512;
        if at + 512 > self.img.len() {
            self.q(0xED); // a write error: past the end
            return;
        }
        let block = self.wbuf[..512].to_vec();
        self.img[at..at + 512].copy_from_slice(&block);
        let s = self.waddr;
        self.write_back(s);
        self.blocks_written += 1;
        self.q(0xE5);
        if self.wmulti {
            self.wstate = 1;
            self.waddr += 1;
        }
    }

    /// One byte clocked: in on DI, the card's DO out (0xFF deselected).
    pub fn xchg(&mut self, b: u8) -> u8 {
        if !self.selected {
            return 0xFF;
        }
        let was_streaming = self.streaming;
        if self.streaming {
            // the card listens for CMD12 while it sends
            if self.cmdlen > 0 || b & 0xC0 == 0x40 {
                self.cmd[self.cmdlen] = b;
                self.cmdlen += 1;
                if self.cmdlen == 6 {
                    self.cmdlen = 0;
                    self.out.clear();
                    self.outpos = 0;
                    self.command();
                    return 0xFF;
                }
            }
            if self.outpos >= self.out.len() {
                self.out.clear();
                self.outpos = 0;
                self.stream_more();
            }
        }
        if !was_streaming && self.wstate == 0 && (self.cmdlen > 0 || b & 0xC0 == 0x40) {
            self.out.clear();
            self.outpos = 0;
            self.cmd[self.cmdlen] = b;
            self.cmdlen += 1;
            if self.cmdlen == 6 {
                self.cmdlen = 0;
                self.command();
            }
            return 0xFF;
        }
        if self.outpos < self.out.len() {
            let r = self.out[self.outpos];
            self.outpos += 1;
            if self.outpos == self.out.len() {
                self.out.clear();
                self.outpos = 0;
            }
            return r;
        }
        match self.wstate {
            1 => {
                if self.wmulti && b == 0xFD {
                    self.wstate = 0;
                    self.wmulti = false;
                } else if b == if self.wmulti { 0xFC } else { 0xFE } {
                    self.wstate = 2;
                    self.wbuf.clear();
                }
                0xFF
            }
            2 => {
                self.wbuf.push(b);
                if self.wbuf.len() == 514 {
                    self.take_block();
                }
                0xFF
            }
            _ => 0xFF,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(c: &mut SdCard, idx: u8, arg: u32) -> Vec<u8> {
        let mut f = [
            0x40 | idx,
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
            0,
        ];
        f[5] = crc7(&f[..5]);
        for b in f {
            c.xchg(b);
        }
        (0..8).map(|_| c.xchg(0xFF)).collect()
    }

    fn r1(v: &[u8]) -> u8 {
        *v.iter().find(|&&b| b != 0xFF).unwrap_or(&0xFF)
    }

    fn ready(img: Vec<u8>) -> SdCard {
        let mut c = SdCard::with_image(img, None);
        c.select(true);
        assert_eq!(r1(&cmd(&mut c, 0, 0)), 0x01, "CMD0: idle");
        let r = cmd(&mut c, 8, 0x1AA);
        assert_eq!(
            &r[1..6],
            &[0x01, 0, 0, 0x01, 0xAA],
            "CMD8: the pattern back"
        );
        cmd(&mut c, 55, 0);
        assert_eq!(r1(&cmd(&mut c, 41, 1 << 30)), 0x00, "ACMD41: ready");
        assert_eq!(r1(&cmd(&mut c, 59, 1)), 0x00, "CMD59: CRC on");
        c
    }

    #[test]
    fn crc7_of_cmd0_is_0x95() {
        assert_eq!(crc7(&[0x40, 0, 0, 0, 0]), 0x95);
        assert_eq!(crc7(&[0x48, 0, 0, 1, 0xAA]), 0x87);
    }

    #[test]
    fn deselected_reads_ff() {
        let mut c = SdCard::with_image(vec![0; 1024 * 512], None);
        assert_eq!(c.xchg(0x40), 0xFF);
        assert_eq!(c.blocks_read, 0);
    }

    #[test]
    fn single_and_multi_block_reads() {
        let mut img = vec![0u8; 2048 * 512];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i * 7 + i / 512) as u8;
        }
        let mut c = ready(img.clone());
        let mut f = [0x51, 0, 0, 0, 3, 0];
        f[5] = crc7(&f[..5]);
        for b in f {
            c.xchg(b);
        }
        let mut got = Vec::new();
        let mut seen_token = false;
        for _ in 0..600 {
            let b = c.xchg(0xFF);
            if seen_token {
                got.push(b);
                if got.len() == 514 {
                    break;
                }
            } else if b == 0xFE {
                seen_token = true;
            }
        }
        assert_eq!(&got[..512], &img[3 * 512..4 * 512], "CMD17: sector 3");
        assert_eq!(
            u16::from_be_bytes([got[512], got[513]]),
            crc16(&img[3 * 512..4 * 512])
        );
        // CMD18 from 10: two blocks, then CMD12
        let mut f = [0x52, 0, 0, 0, 10, 0];
        f[5] = crc7(&f[..5]);
        for b in f {
            c.xchg(b);
        }
        for k in 0..2u32 {
            let mut n = 0;
            while c.xchg(0xFF) != 0xFE {
                n += 1;
                assert!(n < 50, "a token");
            }
            let d: Vec<u8> = (0..514).map(|_| c.xchg(0xFF)).collect();
            let s = (10 + k) as usize * 512;
            assert_eq!(&d[..512], &img[s..s + 512], "CMD18 block {k}");
        }
        let r = cmd(&mut c, 12, 0);
        assert!(r.contains(&0x00), "CMD12 answered");
        assert!(!c.streaming, "the stream over");
    }

    fn write_block(c: &mut SdCard, token: u8, data: &[u8]) -> u8 {
        c.xchg(0xFF);
        c.xchg(token);
        for &b in data {
            c.xchg(b);
        }
        let k = crc16(data);
        c.xchg((k >> 8) as u8);
        c.xchg(k as u8);
        c.xchg(0xFF)
    }

    #[test]
    fn writes_single_and_multi_across_deselect() {
        let mut c = ready(vec![0u8; 2048 * 512]);
        let a = vec![0xA5u8; 512];
        let mut f = [0x58, 0, 0, 0, 5, 0];
        f[5] = crc7(&f[..5]);
        for b in f {
            c.xchg(b);
        }
        for _ in 0..3 {
            c.xchg(0xFF);
        }
        assert_eq!(
            write_block(&mut c, 0xFE, &a) & 0x1F,
            0x05,
            "CMD24: accepted"
        );
        assert_eq!(&c.image()[5 * 512..6 * 512], &a[..]);
        // CMD25 at 100, the card deselected between the blocks
        let mut f = [0x59, 0, 0, 0, 100, 0];
        f[5] = crc7(&f[..5]);
        for b in f {
            c.xchg(b);
        }
        for _ in 0..3 {
            c.xchg(0xFF);
        }
        for k in 0..3u8 {
            let d = vec![k + 1; 512];
            assert_eq!(
                write_block(&mut c, 0xFC, &d) & 0x1F,
                0x05,
                "CMD25 block {k}"
            );
            c.select(false);
            c.select(true);
        }
        c.xchg(0xFD);
        for k in 0..3usize {
            assert!(c.image()[(100 + k) * 512..(101 + k) * 512]
                .iter()
                .all(|&b| b == k as u8 + 1));
        }
        assert_eq!(c.blocks_written, 4);
    }

    #[test]
    fn a_bad_crc_is_refused() {
        let mut c = ready(vec![0u8; 2048 * 512]);
        for b in [0x51u8, 0, 0, 0, 1, 0x00] {
            c.xchg(b);
        }
        let r: Vec<u8> = (0..4).map(|_| c.xchg(0xFF)).collect();
        assert_eq!(r1(&r) & 0x08, 0x08, "the CRC error bit");
        assert_eq!(c.crc_refused, 1);
    }
}
