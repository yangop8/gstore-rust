//! Hand-rolled cryptographic digests for the SPARQL 1.1 hash built-ins
//! (`MD5`, `SHA1`, `SHA256`, `SHA384`, `SHA512`).
//!
//! These are the classic, stable algorithms (RFC 1321, RFC 3174, FIPS 180-4).
//! They are implemented here from scratch so the engine gains the SPARQL hash
//! functions without pulling in any external crate. Each `*_hex` helper hashes
//! the UTF-8 bytes of a string and returns the lowercase hex digest, which is
//! exactly the lexical form SPARQL requires.
//!
//! SHA-256 is also exposed as an incremental [`Sha256`] context, on which the
//! module builds [`HmacSha256`] / [`hmac_sha256`] (RFC 2104) and
//! [`pbkdf2_hmac_sha256`] (RFC 8018). Those back the stable, versioned password
//! hashes in [`crate::http_users`] — unlike `std`'s `DefaultHasher`, whose
//! algorithm may change between Rust releases, these outputs are fixed by
//! their specifications forever. [`constant_time_eq`] is the matching
//! timing-independent comparison.
//!
//! Correctness is pinned by `#[cfg(test)]` vectors against the published digests
//! of the empty string and `"abc"`, the RFC 4231 HMAC-SHA256 test cases, and
//! published PBKDF2-HMAC-SHA256 vectors (RFC 7914 §11 and the RFC 6070-style
//! set).

/// Render bytes as a lowercase hex string.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

// ---------------------------------------------------------------------------
// MD5 (RFC 1321)
// ---------------------------------------------------------------------------

/// MD5 digest (16 bytes).
pub fn md5(input: &[u8]) -> [u8; 16] {
    // Per-round shift amounts.
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    // Binary integer parts of the sines of integers (radians) as constants.
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes()); // MD5 uses little-endian length

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// SHA-1 (RFC 3174)
// ---------------------------------------------------------------------------

/// SHA-1 digest (20 bytes).
pub fn sha1(input: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
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

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The SHA-256 initial hash value H(0).
const SHA256_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// The SHA-256 compression function: fold one 64-byte block into `h`.
fn sha256_compress(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (i, word) in w.iter_mut().take(16).enumerate() {
        *word = u32::from_be_bytes([
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let mut v = *h;
    for i in 0..64 {
        let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
        let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
        let t1 = v[7]
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
        let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let t2 = s0.wrapping_add(maj);
        v[7] = v[6];
        v[6] = v[5];
        v[5] = v[4];
        v[4] = v[3].wrapping_add(t1);
        v[3] = v[2];
        v[2] = v[1];
        v[1] = v[0];
        v[0] = t1.wrapping_add(t2);
    }
    for (hi, vi) in h.iter_mut().zip(v.iter()) {
        *hi = hi.wrapping_add(*vi);
    }
}

/// An incremental (streaming) SHA-256 context: [`update`](Self::update) with
/// any number of byte slices, then [`finalize`](Self::finalize). Cloning a
/// context snapshots its state, which HMAC uses to precompute the keyed
/// inner/outer prefixes once instead of re-hashing the padded key per message.
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    /// Pending bytes of the not-yet-full current block.
    buf: [u8; 64],
    buf_len: usize,
    /// Total message length in bytes so far (for the final length padding).
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Sha256::new()
    }
}

impl Sha256 {
    /// A fresh context (the hash of the empty message so far).
    pub fn new() -> Sha256 {
        Sha256 {
            h: SHA256_IV,
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    /// Absorb `data` into the running hash.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 64 {
                return;
            }
            let block = self.buf;
            sha256_compress(&mut self.h, &block);
            self.buf_len = 0;
        }
        let mut blocks = data.chunks_exact(64);
        for block in &mut blocks {
            let block: &[u8; 64] = block.try_into().expect("chunks_exact yields 64-byte blocks");
            sha256_compress(&mut self.h, block);
        }
        let rest = blocks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    /// Apply the FIPS 180-4 padding and return the 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        // 0x80, then zeros up to 56 mod 64, then the 64-bit big-endian length.
        let pad_zeros = (119 - self.buf_len) % 64;
        let mut tail = [0u8; 72];
        tail[0] = 0x80;
        tail[1 + pad_zeros..1 + pad_zeros + 8].copy_from_slice(&bit_len.to_be_bytes());
        let tail_len = 1 + pad_zeros + 8;
        // `update` would also count the padding into `total_len`; that no longer
        // matters because the length field is already captured above.
        self.update(&tail[..tail_len]);
        debug_assert_eq!(self.buf_len, 0);
        let mut out = [0u8; 32];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// SHA-256 digest (32 bytes).
pub fn sha256(input: &[u8]) -> [u8; 32] {
    let mut ctx = Sha256::new();
    ctx.update(input);
    ctx.finalize()
}

// ---------------------------------------------------------------------------
// HMAC-SHA256 (RFC 2104 / RFC 4231) and PBKDF2-HMAC-SHA256 (RFC 8018)
// ---------------------------------------------------------------------------

/// SHA-256's block size in bytes (the HMAC key-padding width).
const SHA256_BLOCK: usize = 64;

/// A keyed HMAC-SHA256 instance with the inner (`K ⊕ ipad`) and outer
/// (`K ⊕ opad`) prefixes already absorbed, so each MAC costs only the message
/// blocks plus one outer block — what makes PBKDF2's many iterations cheap.
#[derive(Clone)]
pub struct HmacSha256 {
    inner: Sha256,
    outer: Sha256,
}

impl HmacSha256 {
    /// Key an HMAC. Keys longer than the block size are hashed first (RFC 2104).
    pub fn new(key: &[u8]) -> HmacSha256 {
        let mut k = [0u8; SHA256_BLOCK];
        if key.len() > SHA256_BLOCK {
            k[..32].copy_from_slice(&sha256(key));
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0x36u8; SHA256_BLOCK];
        let mut opad = [0x5cu8; SHA256_BLOCK];
        for i in 0..SHA256_BLOCK {
            ipad[i] ^= k[i];
            opad[i] ^= k[i];
        }
        let mut inner = Sha256::new();
        inner.update(&ipad);
        let mut outer = Sha256::new();
        outer.update(&opad);
        HmacSha256 { inner, outer }
    }

    /// The MAC of the concatenation of `parts` under this key.
    pub fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        for p in parts {
            inner.update(p);
        }
        let ih = inner.finalize();
        let mut outer = self.outer.clone();
        outer.update(&ih);
        outer.finalize()
    }
}

/// HMAC-SHA256 of `msg` under `key` (RFC 2104 with SHA-256, RFC 4231 vectors).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    HmacSha256::new(key).mac(&[msg])
}

/// PBKDF2 (RFC 8018 §5.2) with HMAC-SHA256 as the PRF: derive `out.len()` bytes
/// from `password` and `salt` using `iterations` rounds (`0` is treated as 1).
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let prf = HmacSha256::new(password);
    let iterations = iterations.max(1);
    for (block_idx, chunk) in out.chunks_mut(32).enumerate() {
        // INT(i): the 1-based block index, big-endian. `out` would need to be
        // > 128 GiB for this to overflow u32.
        let i = u32::try_from(block_idx + 1).unwrap_or(u32::MAX).to_be_bytes();
        let mut u = prf.mac(&[salt, &i]);
        let mut t = u;
        for _ in 1..iterations {
            u = prf.mac(&[&u]);
            for (tb, ub) in t.iter_mut().zip(u.iter()) {
                *tb ^= ub;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

/// Compare two byte strings in time independent of where they first differ
/// (lengths are not secret). Use for MAC / password-hash verification.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    // `black_box` discourages the optimizer from short-circuiting the fold.
    std::hint::black_box(diff) == 0
}

/// Lowercase hex rendering of `bytes` (public for password-hash encoding).
pub fn hex_encode(bytes: &[u8]) -> String {
    to_hex(bytes)
}

/// Decode a hex string (either case) into bytes; `None` on odd length or a
/// non-hex digit.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    b.chunks_exact(2)
        .map(|p| Some((nib(p[0])? << 4) | nib(p[1])?))
        .collect()
}

// ---------------------------------------------------------------------------
// SHA-512 / SHA-384 (FIPS 180-4)
// ---------------------------------------------------------------------------

const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

/// SHA-512 family core, parameterised by initial hash values. Returns the eight
/// 64-bit words of internal state, which the public wrappers truncate.
fn sha512_core(input: &[u8], mut h: [u64; 8]) -> [u64; 8] {
    let mut msg = input.to_vec();
    // 128-bit length; inputs here never exceed 2^64 bits, so the high half is 0.
    let bit_len = (input.len() as u128).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 128 != 112 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(128) {
        let mut w = [0u64; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&chunk[i * 8..i * 8 + 8]);
            *word = u64::from_be_bytes(b);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..80 {
            let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA512_K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (hi, vi) in h.iter_mut().zip(v.iter()) {
            *hi = hi.wrapping_add(*vi);
        }
    }
    h
}

/// SHA-512 digest (64 bytes).
pub fn sha512(input: &[u8]) -> [u8; 64] {
    let h = sha512_core(
        input,
        [
            0x6a09e667f3bcc908,
            0xbb67ae8584caa73b,
            0x3c6ef372fe94f82b,
            0xa54ff53a5f1d36f1,
            0x510e527fade682d1,
            0x9b05688c2b3e6c1f,
            0x1f83d9abfb41bd6b,
            0x5be0cd19137e2179,
        ],
    );
    let mut out = [0u8; 64];
    for (i, word) in h.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// SHA-384 digest (48 bytes) — SHA-512 with a distinct IV, truncated.
pub fn sha384(input: &[u8]) -> [u8; 48] {
    let h = sha512_core(
        input,
        [
            0xcbbb9d5dc1059ed8,
            0x629a292a367cd507,
            0x9159015a3070dd17,
            0x152fecd8f70e5939,
            0x67332667ffc00b31,
            0x8eb44a8768581511,
            0xdb0c2e0d64f98fa7,
            0x47b5481dbefa4fa4,
        ],
    );
    let mut out = [0u8; 48];
    for (i, word) in h.iter().take(6).enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---- string → lowercase-hex convenience wrappers --------------------------

pub fn md5_hex(s: &str) -> String {
    to_hex(&md5(s.as_bytes()))
}
pub fn sha1_hex(s: &str) -> String {
    to_hex(&sha1(s.as_bytes()))
}
pub fn sha256_hex(s: &str) -> String {
    to_hex(&sha256(s.as_bytes()))
}
pub fn sha384_hex(s: &str) -> String {
    to_hex(&sha384(s.as_bytes()))
}
pub fn sha512_hex(s: &str) -> String {
    to_hex(&sha512(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_vectors() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex("The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn sha1_vectors() {
        assert_eq!(sha1_hex(""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha384_vectors() {
        assert_eq!(
            sha384_hex(""),
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b"
        );
        assert_eq!(
            sha384_hex("abc"),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
    }

    #[test]
    fn sha512_vectors() {
        assert_eq!(
            sha512_hex(""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            sha512_hex("abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn streaming_sha256_matches_one_shot_at_every_split() {
        // Exercise every buffer boundary: lengths around the 55/56/64-byte
        // padding edges, fed in uneven pieces.
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in [0usize, 1, 55, 56, 57, 63, 64, 65, 119, 120, 128, 300] {
            let msg = &data[..len];
            let want = sha256(msg);
            for split in [0usize, 1, 13, 63, 64, 65] {
                let split = split.min(len);
                let mut ctx = Sha256::new();
                ctx.update(&msg[..split]);
                ctx.update(&msg[split..]);
                assert_eq!(ctx.finalize(), want, "len={len} split={split}");
            }
        }
        // 1,000,000 × 'a' (FIPS 180-2 appendix B.3), fed in 1000-byte chunks.
        let mut ctx = Sha256::new();
        let chunk = [b'a'; 1000];
        for _ in 0..1000 {
            ctx.update(&chunk);
        }
        assert_eq!(
            to_hex(&ctx.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// RFC 4231 §4.2–4.8: all seven HMAC-SHA256 test cases.
    #[test]
    fn hmac_sha256_rfc4231_vectors() {
        let tc = |key: &[u8], data: &[u8]| to_hex(&hmac_sha256(key, data));
        assert_eq!(
            tc(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            tc(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            tc(&[0xaa; 20], &[0xdd; 50]),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        let key4: Vec<u8> = (1..=25).collect();
        assert_eq!(
            tc(&key4, &[0xcd; 50]),
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"
        );
        // Test case 5 is truncated to 128 bits.
        assert_eq!(
            &tc(&[0x0c; 20], b"Test With Truncation")[..32],
            "a3b6167473100ee06e0c796c2955552b"
        );
        // Test cases 6–7: a key longer than the block size is hashed first.
        assert_eq!(
            tc(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First"),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            tc(
                &[0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than \
                  block-size data. The key needs to be hashed before being used by the \
                  HMAC algorithm."
            ),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    #[test]
    fn hmac_multi_part_equals_concatenation() {
        let mac = HmacSha256::new(b"k");
        assert_eq!(mac.mac(&[b"ab", b"", b"cd"]), hmac_sha256(b"k", b"abcd"));
    }

    fn pbkdf2_hex(p: &[u8], s: &[u8], c: u32, len: usize) -> String {
        let mut out = vec![0u8; len];
        pbkdf2_hmac_sha256(p, s, c, &mut out);
        to_hex(&out)
    }

    /// Published PBKDF2-HMAC-SHA256 vectors: the RFC 6070 inputs re-derived
    /// with SHA-256 (as widely published), plus RFC 7914 §11.
    #[test]
    fn pbkdf2_hmac_sha256_known_vectors() {
        assert_eq!(
            pbkdf2_hex(b"password", b"salt", 1, 32),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            pbkdf2_hex(b"password", b"salt", 2, 32),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            pbkdf2_hex(b"password", b"salt", 4096, 32),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
        // Multi-block output (40 bytes ⇒ two PRF blocks, second truncated).
        assert_eq!(
            pbkdf2_hex(
                b"passwordPASSWORDpassword",
                b"saltSALTsaltSALTsaltSALTsaltSALTsalt",
                4096,
                40
            ),
            "348c89dbcbd32b2f32d814b8116e84cf2b17347ebc1800181c4e2a1fb8dd53e1c635518c7dac47e9"
        );
        // Embedded NULs.
        assert_eq!(
            pbkdf2_hex(b"pass\0word", b"sa\0lt", 4096, 16),
            "89b69d0516f829893c696226650a8687"
        );
        // RFC 7914 §11 (the PBKDF2-HMAC-SHA256 vectors used by scrypt).
        assert_eq!(
            pbkdf2_hex(b"passwd", b"salt", 1, 64),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
        assert_eq!(
            pbkdf2_hex(b"Password", b"NaCl", 80000, 64),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56\
             a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"
        );
    }

    #[test]
    fn constant_time_eq_and_hex_roundtrip() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
        let bytes = [0x00u8, 0x7f, 0x80, 0xff, 0x12];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert_eq!(hex_decode("ABcd").unwrap(), vec![0xab, 0xcd]);
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn longer_input_spans_multiple_blocks() {
        // 1000-char string forces several compression blocks for every digest.
        let big = "a".repeat(1000);
        assert_eq!(md5_hex(&big).len(), 32);
        assert_eq!(sha1_hex(&big).len(), 40);
        assert_eq!(sha256_hex(&big).len(), 64);
        assert_eq!(sha384_hex(&big).len(), 96);
        assert_eq!(sha512_hex(&big).len(), 128);
        // Known vector: SHA-256 of one million 'a' chars starts with cdc76e5c…;
        // here we just pin the 1000-'a' SHA-1 to guard the multi-block path.
        assert_eq!(
            sha1_hex(&big),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }
}
