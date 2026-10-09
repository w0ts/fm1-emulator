// SPDX-License-Identifier: GPL-3.0-only
//! The Bluetooth LE security functions the virtual peripheral needs, written
//! from the public specifications: AES-128 (FIPS-197, encryption only), the
//! link layer's AES-CCM (Core Vol 6 Part E), LE legacy pairing's c1 / s1 and
//! the random address hash ah (Core Vol 3 Part H 2.2.2-2.2.4).
//!
//! Byte order: `aes128` and `e` take and give blocks most significant octet
//! first (as FIPS-197 and the Core's e() are written). Everything that goes
//! over the air (keys in SMP PDUs, IVs, SKDs) is least significant octet
//! first; the `_le` helpers turn it around.

const SBOX: [u8; 256] = {
    // The S-box computed as the affine map of the GF(2^8) inverse (FIPS-197 5.1.1).
    let mut s = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut inv = 0u8;
        if i != 0 {
            let mut j = 1;
            while j < 256 {
                if gmul(i as u8, j as u8) == 1 {
                    inv = j as u8;
                }
                j += 1;
            }
        }
        let mut x = inv;
        let mut v = inv;
        let mut k = 0;
        while k < 4 {
            x = x.rotate_left(1);
            v ^= x;
            k += 1;
        }
        s[i] = v ^ 0x63;
        i += 1;
    }
    s
};

const fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1B;
        }
        b >>= 1;
    }
    p
}

/// AES-128 encryption of one block (FIPS-197), most significant octet first.
pub fn aes128(key: &[u8; 16], input: &[u8; 16]) -> [u8; 16] {
    let mut w = [[0u8; 4]; 44];
    for (i, word) in w.iter_mut().take(4).enumerate() {
        word.copy_from_slice(&key[4 * i..4 * i + 4]);
    }
    let mut rcon = 1u8;
    for i in 4..44 {
        let mut t = w[i - 1];
        if i % 4 == 0 {
            t = [
                SBOX[t[1] as usize] ^ rcon,
                SBOX[t[2] as usize],
                SBOX[t[3] as usize],
                SBOX[t[0] as usize],
            ];
            rcon = gmul(rcon, 2);
        }
        for k in 0..4 {
            w[i][k] = w[i - 4][k] ^ t[k];
        }
    }
    let mut s = *input;
    let add = |s: &mut [u8; 16], round: usize| {
        for c in 0..4 {
            for r in 0..4 {
                s[4 * c + r] ^= w[4 * round + c][r];
            }
        }
    };
    add(&mut s, 0);
    for round in 1..=10 {
        for b in s.iter_mut() {
            *b = SBOX[*b as usize];
        }
        // ShiftRows: row r rotates left by r (column-major state).
        let t = s;
        for c in 0..4 {
            for r in 0..4 {
                s[4 * c + r] = t[4 * ((c + r) % 4) + r];
            }
        }
        if round != 10 {
            for c in 0..4 {
                let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
                s[4 * c] = gmul(a[0], 2) ^ gmul(a[1], 3) ^ a[2] ^ a[3];
                s[4 * c + 1] = a[0] ^ gmul(a[1], 2) ^ gmul(a[2], 3) ^ a[3];
                s[4 * c + 2] = a[0] ^ a[1] ^ gmul(a[2], 2) ^ gmul(a[3], 3);
                s[4 * c + 3] = gmul(a[0], 3) ^ a[1] ^ a[2] ^ gmul(a[3], 2);
            }
        }
        add(&mut s, round);
    }
    s
}

fn rev16(x: &[u8; 16]) -> [u8; 16] {
    let mut o = *x;
    o.reverse();
    o
}

/// e() on values least significant octet first (as SMP sends them).
pub fn e_le(key: &[u8; 16], input: &[u8; 16]) -> [u8; 16] {
    rev16(&aes128(&rev16(key), &rev16(input)))
}

/// One direction of an encrypted LL link (Core Vol 6 Part E 2).
#[derive(Clone, Debug, Default)]
pub struct Ccm {
    /// The session key, most significant octet first.
    pub key: [u8; 16],
    /// IVm (4, least significant first) then IVs (4).
    pub iv: [u8; 8],
    /// packetCounter (39 bits).
    pub counter: u64,
    /// directionBit: 1 master -> slave.
    pub direction: u8,
}

impl Ccm {
    fn block(&self, flags: u8, tail: u16) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = flags;
        b[1..6].copy_from_slice(&self.counter.to_le_bytes()[..5]);
        b[5] = (b[5] & 0x7F) | self.direction << 7;
        b[6..14].copy_from_slice(&self.iv);
        b[14..16].copy_from_slice(&tail.to_be_bytes());
        b
    }

    fn mic(&self, header: u8, plain: &[u8]) -> [u8; 4] {
        let mut x = aes128(&self.key, &self.block(0x49, plain.len() as u16));
        x[1] ^= 0x01;
        x[2] ^= header & 0xE3;
        x = aes128(&self.key, &x);
        for chunk in plain.chunks(16) {
            for (a, b) in x.iter_mut().zip(chunk) {
                *a ^= b;
            }
            x = aes128(&self.key, &x);
        }
        let s0 = aes128(&self.key, &self.block(0x01, 0));
        [x[0] ^ s0[0], x[1] ^ s0[1], x[2] ^ s0[2], x[3] ^ s0[3]]
    }

    fn ctr(&self, data: &mut [u8]) {
        for (i, chunk) in data.chunks_mut(16).enumerate() {
            let s = aes128(&self.key, &self.block(0x01, i as u16 + 1));
            for (a, b) in chunk.iter_mut().zip(s) {
                *a ^= b;
            }
        }
    }

    /// Encrypt a payload; returns payload + MIC. The counter steps on.
    pub fn encrypt(&mut self, header: u8, plain: &[u8]) -> Vec<u8> {
        let mic = self.mic(header, plain);
        let mut out = plain.to_vec();
        self.ctr(&mut out);
        out.extend(mic);
        self.counter = (self.counter + 1) & 0x7F_FFFF_FFFF;
        out
    }

    /// Decrypt payload + MIC; None when the MIC is wrong (the counter stays).
    pub fn decrypt(&mut self, header: u8, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 5 {
            return None;
        }
        let (body, mic) = data.split_at(data.len() - 4);
        let mut plain = body.to_vec();
        self.ctr(&mut plain);
        if self.mic(header, &plain) != mic {
            return None;
        }
        self.counter = (self.counter + 1) & 0x7F_FFFF_FFFF;
        Some(plain)
    }
}

/// The session key: e(LTK, SKDs || SKDm), from the LL_ENC_REQ / RSP fields
/// (LTK, SKDm, SKDs least significant octet first). Most significant first.
pub fn session_key(ltk: &[u8; 16], skdm: &[u8; 8], skds: &[u8; 8]) -> [u8; 16] {
    let mut skd = [0u8; 16];
    for i in 0..8 {
        skd[i] = skds[7 - i];
        skd[8 + i] = skdm[7 - i];
    }
    aes128(&rev16(ltk), &skd)
}

/// What c1 hashes besides the key and the random: the Pairing Request /
/// Response PDUs (opcode first) and the initiator's and responder's
/// addresses with their types (1 random), least significant octet first.
#[derive(Clone, Copy, Debug, Default)]
pub struct PairingContext {
    pub preq: [u8; 7],
    pub pres: [u8; 7],
    pub iat: u8,
    pub ia: [u8; 6],
    pub rat: u8,
    pub ra: [u8; 6],
}

/// c1 (Core Vol 3 Part H 2.2.3), every value least significant octet first.
pub fn c1(k: &[u8; 16], r: &[u8; 16], ctx: &PairingContext) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = ctx.iat;
    p[1] = ctx.rat;
    p[2..9].copy_from_slice(&ctx.preq);
    p[9..16].copy_from_slice(&ctx.pres);
    let mut x = [0u8; 16];
    for i in 0..16 {
        x[i] = r[i] ^ p[i];
    }
    x = e_le(k, &x);
    let mut p2 = [0u8; 16];
    p2[0..6].copy_from_slice(&ctx.ra);
    p2[6..12].copy_from_slice(&ctx.ia);
    for i in 0..16 {
        x[i] ^= p2[i];
    }
    e_le(k, &x)
}

/// s1 (Core Vol 3 Part H 2.2.4): e(k, r1' || r2'), least significant first.
pub fn s1(k: &[u8; 16], r1: &[u8; 16], r2: &[u8; 16]) -> [u8; 16] {
    let mut x = [0u8; 16];
    x[..8].copy_from_slice(&r2[..8]);
    x[8..].copy_from_slice(&r1[..8]);
    e_le(k, &x)
}

/// ah (Core Vol 3 Part H 2.2.2): e(irk, 0^104 || prand) mod 2^24, the IRK
/// most significant octet first.
pub fn ah(irk: &[u8; 16], prand: u32) -> u32 {
    let mut b = [0u8; 16];
    b[13..16].copy_from_slice(&prand.to_be_bytes()[1..]);
    let o = aes128(irk, &b);
    u32::from_be_bytes([0, o[13], o[14], o[15]])
}

/// A resolvable private address from an IRK (least significant octet first
/// in SMP order: `irk_le`) and 22 random bits: hash in octets 0-2, prand
/// (top two bits 01) in octets 3-5.
pub fn rpa(irk_le: &[u8; 16], random: u32) -> [u8; 6] {
    let prand = (random & 0x3F_FFFF) | 0x40_0000;
    let hash = ah(&rev16(irk_le), prand);
    let h = hash.to_le_bytes();
    let p = prand.to_le_bytes();
    [h[0], h[1], h[2], p[0], p[1], p[2]]
}

/// Whether `addr` (least significant first) is an RPA made from `irk_le`.
pub fn rpa_resolves(irk_le: &[u8; 16], addr: &[u8; 6]) -> bool {
    let prand = u32::from_le_bytes([addr[3], addr[4], addr[5], 0]);
    prand >> 22 == 1
        && ah(&rev16(irk_le), prand) == u32::from_le_bytes([addr[0], addr[1], addr[2], 0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(s: &str) -> [u8; N] {
        let mut o = [0u8; N];
        for (i, b) in o.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        o
    }
    fn hex_le<const N: usize>(s: &str) -> [u8; N] {
        let mut o: [u8; N] = hex(s);
        o.reverse();
        o
    }

    #[test]
    fn aes_fips197_c1_and_the_core_session_key() {
        let k: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f");
        let p: [u8; 16] = hex("00112233445566778899aabbccddeeff");
        assert_eq!(
            aes128(&k, &p),
            hex::<16>("69c4e0d86a7b0430d8cdb78070b4c55a")
        );
        let ltk: [u8; 16] = hex_le("4C68384139F574D836BCF34E9DFB01BF");
        let skdm: [u8; 8] = hex_le("ACBDCEDFE0F10213");
        let skds: [u8; 8] = hex_le("0213243546576879");
        assert_eq!(
            session_key(&ltk, &skdm, &skds),
            hex::<16>("99AD1B5226A37E3E058E3B8E27C2C666")
        );
    }

    fn sample(direction: u8, counter: u64) -> Ccm {
        Ccm {
            key: hex("99AD1B5226A37E3E058E3B8E27C2C666"),
            iv: hex("24ABDCBABEBAAFDE"),
            counter,
            direction,
        }
    }

    #[test]
    fn ccm_core_samples() {
        let mut m = sample(1, 0);
        let plain = m.decrypt(0x0F, &hex::<5>("9FCDA7F448")).unwrap();
        assert_eq!(plain, [0x06]);
        assert_eq!(m.counter, 1);
        let mut s = sample(0, 1);
        let p: [u8; 27] = hex("170037363534333231304142434445464748494A4B4C4D4E4F5051");
        let c = s.encrypt(0x06, &p);
        assert_eq!(
            c,
            hex::<31>("F38881E7BD94C9C369B9A66846DD4786AA8C39CE540D0DAE3ADCDF89B96088")
        );
        let mut d = sample(0, 1);
        let mut bad = c.clone();
        bad[3] ^= 0x10;
        assert!(d.decrypt(0x06, &bad).is_none());
        assert_eq!(d.decrypt(0x06 ^ 0x1C, &c).unwrap(), p);
    }

    #[test]
    fn c1_s1_core_samples() {
        let k = [0u8; 16];
        let r: [u8; 16] = hex_le("5783D52156AD6F0E6388274EC6702EE0");
        let ctx = PairingContext {
            preq: hex_le("07071000000101"),
            pres: hex_le("05000800000302"),
            iat: 1,
            ia: hex_le("A1A2A3A4A5A6"),
            rat: 0,
            ra: hex_le("B1B2B3B4B5B6"),
        };
        assert_eq!(
            c1(&k, &r, &ctx),
            hex_le::<16>("1E1E3FEF878988EAD2A74DC5BEF13B86")
        );
        let r1: [u8; 16] = hex_le("000F0E0D0C0B0A091122334455667788");
        let r2: [u8; 16] = hex_le("010203040506070899AABBCCDDEEFF00");
        assert_eq!(
            s1(&k, &r1, &r2),
            hex_le::<16>("9A1FE1F0E8B0F49B5B4216AE796DA062")
        );
    }

    #[test]
    fn ah_core_sample_and_rpa_round_trip() {
        let irk: [u8; 16] = hex("ec0234a357c8ad05341010a60a397d9b");
        assert_eq!(ah(&irk, 0x708194), 0x0dfbaa);
        let irk_le: [u8; 16] = hex_le("ec0234a357c8ad05341010a60a397d9b");
        let a = rpa(&irk_le, 0x1234);
        assert_eq!(a[5] >> 6, 1);
        assert!(rpa_resolves(&irk_le, &a));
        let mut other = a;
        other[0] ^= 1;
        assert!(!rpa_resolves(&irk_le, &other));
    }
}
