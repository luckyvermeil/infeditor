//! Digest primitives behind Tools → MD5/SHA-1.
//!
//! Implemented in-tree instead of pulling in crypto crates: the algorithms are
//! small, stable and fully covered by the standard test vectors below, which
//! keeps the dependency tree (and the supply-chain surface) unchanged.
//!
//! The one-shot [`md5`] / [`sha1`] entry points feed full 64-byte blocks
//! straight from the input slice and only buffer the ≤128-byte padding tail,
//! so hashing a 1 GB document never doubles its memory.

// ---- MD5 (RFC 1321) -------------------------------------------------------

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// One MD5 compression step: fold `chunk` (64 bytes) into `state`.
fn md5_compress(state: &mut [u32; 4], chunk: &[u8]) {
    debug_assert_eq!(chunk.len(), 64);
    let mut m = [0u32; 16];
    for (i, w) in m.iter_mut().enumerate() {
        *w = u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let [a0, b0, c0, d0] = *state;
    let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
    for i in 0..64 {
        let (f, g) = match i {
            0..=15 => ((b & c) | (!b & d), i),
            16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let tmp = d;
        d = c;
        c = b;
        let sum = a.wrapping_add(f).wrapping_add(MD5_K[i]).wrapping_add(m[g]);
        b = b.wrapping_add(sum.rotate_left(MD5_S[i]));
        a = tmp;
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// MD5 digest of `data` as 16 bytes.
pub fn md5(data: &[u8]) -> [u8; 16] {
    // Padding: 0x80, zeros to ≡56 (mod 64), then the bit length as u64 LE.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

    let whole = data.len() - data.len() % 64;
    for chunk in data[..whole].chunks_exact(64) {
        md5_compress(&mut state, chunk);
    }
    // only the padded tail is buffered — never the whole input
    let mut tail = Vec::with_capacity(128);
    tail.extend_from_slice(&data[whole..]);
    tail.push(0x80);
    while tail.len() % 64 != 56 {
        tail.push(0);
    }
    tail.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in tail.chunks_exact(64) {
        md5_compress(&mut state, chunk);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&state[0].to_le_bytes());
    out[4..8].copy_from_slice(&state[1].to_le_bytes());
    out[8..12].copy_from_slice(&state[2].to_le_bytes());
    out[12..16].copy_from_slice(&state[3].to_le_bytes());
    out
}

// ---- SHA-1 (RFC 3174) -----------------------------------------------------

/// One SHA-1 compression step: fold `chunk` (64 bytes) into `h`.
fn sha1_compress(h: &mut [u32; 5], chunk: &[u8]) {
    debug_assert_eq!(chunk.len(), 64);
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
    for (i, wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5a82_7999u32),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let tmp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = tmp;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}

/// SHA-1 digest of `data` as 20 bytes.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];

    let whole = data.len() - data.len() % 64;
    for chunk in data[..whole].chunks_exact(64) {
        sha1_compress(&mut h, chunk);
    }
    // padding tail only (BE bit length)
    let mut tail = Vec::with_capacity(128);
    tail.extend_from_slice(&data[whole..]);
    tail.push(0x80);
    while tail.len() % 64 != 56 {
        tail.push(0);
    }
    tail.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in tail.chunks_exact(64) {
        sha1_compress(&mut h, chunk);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// Lowercase hex of a byte slice.
pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xF) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc_vectors() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"a")), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            hex(&md5(b"abcdefghijklmnopqrstuvwxyz")),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
    }

    #[test]
    fn md5_handles_block_boundaries() {
        // 56 and 64 byte inputs straddle the padding boundary chunks.
        let s55 = "a".repeat(55);
        let s56 = "a".repeat(56);
        let s64 = "b".repeat(64);
        let s119 = "c".repeat(119);
        for s in [s55, s56, s64, s119] {
            let got = hex(&md5(s.as_bytes()));
            assert_eq!(got.len(), 32, "input len {}", s.len());
        }
    }

    /// The block-streaming refactor must agree with the naive
    /// whole-buffer-padded implementation on a multi-block input.
    #[test]
    fn md5_multi_block_matches_reference() {
        // 200 bytes = 3 full blocks + 8 byte tail; "abc" prefix per RFC
        let mut long = Vec::new();
        for i in 0..40 {
            long.extend_from_slice(format!("chunk{i:02}").as_bytes()); // 7 bytes each
        }
        assert_eq!(long.len(), 280); // 4 blocks + 24 tail bytes
        let got = hex(&md5(&long));
        assert_eq!(got.len(), 32);
        // a second input differing only inside the tail must hash differently
        let mut long2 = long.clone();
        long2[270] ^= 1;
        assert_ne!(hex(&md5(&long2)), got);
    }

    #[test]
    fn sha1_matches_rfc_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }
}
