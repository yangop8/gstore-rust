//! `nlohmann::json(x).dump()` for a `double`, bit-exact: a port of the
//! Grisu2 digit generation (`nlohmann::detail::dtoa_impl`, json.hpp 3.11.3,
//! the copy upstream bundles in `include/nlohmann/json.hpp`) and its
//! `to_chars` / `format_buffer` layout.
//!
//! Grisu2 always round-trips but does **not** always produce the shortest
//! digit string (e.g. `35.0 / 127.0` dumps as `0.27559055118110237`, where the
//! shortest form is `0.2755905511811024`), so a shortest-digits formatter is
//! not a substitute when the output has to match upstream byte for byte
//! (`confidenceDegree`).

/// A `f · 2^e` "do-it-yourself" floating-point number.
#[derive(Debug, Clone, Copy)]
struct DiyFp {
    f: u64,
    e: i32,
}

impl DiyFp {
    fn sub(x: DiyFp, y: DiyFp) -> DiyFp {
        DiyFp {
            f: x.f - y.f,
            e: x.e,
        }
    }

    /// The upper 64 bits of the 128-bit product, rounded (ties up).
    fn mul(x: DiyFp, y: DiyFp) -> DiyFp {
        let (u_lo, u_hi) = (x.f & 0xFFFF_FFFF, x.f >> 32);
        let (v_lo, v_hi) = (y.f & 0xFFFF_FFFF, y.f >> 32);
        let p0 = u_lo * v_lo;
        let p1 = u_lo * v_hi;
        let p2 = u_hi * v_lo;
        let p3 = u_hi * v_hi;
        let mut q = (p0 >> 32) + (p1 & 0xFFFF_FFFF) + (p2 & 0xFFFF_FFFF);
        q += 1u64 << 31;
        let h = p3 + (p2 >> 32) + (p1 >> 32) + (q >> 32);
        DiyFp {
            f: h,
            e: x.e + y.e + 64,
        }
    }

    fn normalize(mut x: DiyFp) -> DiyFp {
        while x.f >> 63 == 0 {
            x.f <<= 1;
            x.e -= 1;
        }
        x
    }

    fn normalize_to(x: DiyFp, e: i32) -> DiyFp {
        DiyFp {
            f: x.f << (x.e - e),
            e,
        }
    }
}

/// `(w_minus, v, w_plus)`: the normalized value and its (normalized)
/// rounding boundaries (`compute_boundaries`).
fn compute_boundaries(value: f64) -> (DiyFp, DiyFp, DiyFp) {
    const PRECISION: i32 = 53;
    const BIAS: i32 = 1023 + (PRECISION - 1);
    const MIN_EXP: i32 = 1 - BIAS;
    const HIDDEN: u64 = 1 << (PRECISION - 1);
    let bits = value.to_bits();
    let e = bits >> (PRECISION - 1);
    let f = bits & (HIDDEN - 1);
    let v = if e == 0 {
        DiyFp { f, e: MIN_EXP }
    } else {
        DiyFp {
            f: f + HIDDEN,
            e: e as i32 - BIAS,
        }
    };
    let lower_boundary_is_closer = f == 0 && e > 1;
    let m_plus = DiyFp {
        f: 2 * v.f + 1,
        e: v.e - 1,
    };
    let m_minus = if lower_boundary_is_closer {
        DiyFp {
            f: 4 * v.f - 1,
            e: v.e - 2,
        }
    } else {
        DiyFp {
            f: 2 * v.f - 1,
            e: v.e - 1,
        }
    };
    let w_plus = DiyFp::normalize(m_plus);
    let w_minus = DiyFp::normalize_to(m_minus, w_plus.e);
    (w_minus, DiyFp::normalize(v), w_plus)
}

const ALPHA: i32 = -60;

/// `kCachedPowers` (`f`, `e`, `k`): `10^k ≈ f · 2^e`, `k = -300, -292, …, 324`.
const CACHED_POWERS: [(u64, i32, i32); 79] = [
    (0xAB70FE17C79AC6CA, -1060, -300),
    (0xFF77B1FCBEBCDC4F, -1034, -292),
    (0xBE5691EF416BD60C, -1007, -284),
    (0x8DD01FAD907FFC3C, -980, -276),
    (0xD3515C2831559A83, -954, -268),
    (0x9D71AC8FADA6C9B5, -927, -260),
    (0xEA9C227723EE8BCB, -901, -252),
    (0xAECC49914078536D, -874, -244),
    (0x823C12795DB6CE57, -847, -236),
    (0xC21094364DFB5637, -821, -228),
    (0x9096EA6F3848984F, -794, -220),
    (0xD77485CB25823AC7, -768, -212),
    (0xA086CFCD97BF97F4, -741, -204),
    (0xEF340A98172AACE5, -715, -196),
    (0xB23867FB2A35B28E, -688, -188),
    (0x84C8D4DFD2C63F3B, -661, -180),
    (0xC5DD44271AD3CDBA, -635, -172),
    (0x936B9FCEBB25C996, -608, -164),
    (0xDBAC6C247D62A584, -582, -156),
    (0xA3AB66580D5FDAF6, -555, -148),
    (0xF3E2F893DEC3F126, -529, -140),
    (0xB5B5ADA8AAFF80B8, -502, -132),
    (0x87625F056C7C4A8B, -475, -124),
    (0xC9BCFF6034C13053, -449, -116),
    (0x964E858C91BA2655, -422, -108),
    (0xDFF9772470297EBD, -396, -100),
    (0xA6DFBD9FB8E5B88F, -369, -92),
    (0xF8A95FCF88747D94, -343, -84),
    (0xB94470938FA89BCF, -316, -76),
    (0x8A08F0F8BF0F156B, -289, -68),
    (0xCDB02555653131B6, -263, -60),
    (0x993FE2C6D07B7FAC, -236, -52),
    (0xE45C10C42A2B3B06, -210, -44),
    (0xAA242499697392D3, -183, -36),
    (0xFD87B5F28300CA0E, -157, -28),
    (0xBCE5086492111AEB, -130, -20),
    (0x8CBCCC096F5088CC, -103, -12),
    (0xD1B71758E219652C, -77, -4),
    (0x9C40000000000000, -50, 4),
    (0xE8D4A51000000000, -24, 12),
    (0xAD78EBC5AC620000, 3, 20),
    (0x813F3978F8940984, 30, 28),
    (0xC097CE7BC90715B3, 56, 36),
    (0x8F7E32CE7BEA5C70, 83, 44),
    (0xD5D238A4ABE98068, 109, 52),
    (0x9F4F2726179A2245, 136, 60),
    (0xED63A231D4C4FB27, 162, 68),
    (0xB0DE65388CC8ADA8, 189, 76),
    (0x83C7088E1AAB65DB, 216, 84),
    (0xC45D1DF942711D9A, 242, 92),
    (0x924D692CA61BE758, 269, 100),
    (0xDA01EE641A708DEA, 295, 108),
    (0xA26DA3999AEF774A, 322, 116),
    (0xF209787BB47D6B85, 348, 124),
    (0xB454E4A179DD1877, 375, 132),
    (0x865B86925B9BC5C2, 402, 140),
    (0xC83553C5C8965D3D, 428, 148),
    (0x952AB45CFA97A0B3, 455, 156),
    (0xDE469FBD99A05FE3, 481, 164),
    (0xA59BC234DB398C25, 508, 172),
    (0xF6C69A72A3989F5C, 534, 180),
    (0xB7DCBF5354E9BECE, 561, 188),
    (0x88FCF317F22241E2, 588, 196),
    (0xCC20CE9BD35C78A5, 614, 204),
    (0x98165AF37B2153DF, 641, 212),
    (0xE2A0B5DC971F303A, 667, 220),
    (0xA8D9D1535CE3B396, 694, 228),
    (0xFB9B7CD9A4A7443C, 720, 236),
    (0xBB764C4CA7A44410, 747, 244),
    (0x8BAB8EEFB6409C1A, 774, 252),
    (0xD01FEF10A657842C, 800, 260),
    (0x9B10A4E5E9913129, 827, 268),
    (0xE7109BFBA19C0C9D, 853, 276),
    (0xAC2820D9623BF429, 880, 284),
    (0x80444B5E7AA7CF85, 907, 292),
    (0xBF21E44003ACDD2D, 933, 300),
    (0x8E679C2F5E44FF8F, 960, 308),
    (0xD433179D9C8CB841, 986, 316),
    (0x9E19DB92B4E31BA9, 1013, 324),
];

/// `get_cached_power_for_binary_exponent`.
fn cached_power(e: i32) -> (u64, i32, i32) {
    let f = ALPHA - e - 1;
    // C integer division truncates toward zero, as Rust's does.
    let k = (f * 78913) / (1 << 18) + i32::from(f > 0);
    let index = (300 + k + 7) / 8;
    CACHED_POWERS[index as usize]
}

/// `find_largest_pow10`: `(k, 10^(k-1))` with `10^(k-1) <= n < 10^k`.
fn largest_pow10(n: u32) -> (i32, u32) {
    let mut pow10 = 1u32;
    let mut k = 1;
    while k < 10 && n >= pow10 * 10 {
        pow10 *= 10;
        k += 1;
    }
    (k, pow10)
}

fn grisu2_round(buf: &mut [u8], dist: u64, delta: u64, mut rest: u64, ten_k: u64) {
    let last = buf.len() - 1;
    while rest < dist
        && delta - rest >= ten_k
        && (rest + ten_k < dist || dist - rest > rest + ten_k - dist)
    {
        buf[last] -= 1;
        rest += ten_k;
    }
}

/// `grisu2_digit_gen`: digits of `V` with `M- <= V <= M+`; returns the
/// adjusted decimal exponent.
fn digit_gen(buf: &mut Vec<u8>, mut dec_exp: i32, m_minus: DiyFp, w: DiyFp, m_plus: DiyFp) -> i32 {
    let mut delta = DiyFp::sub(m_plus, m_minus).f;
    let mut dist = DiyFp::sub(m_plus, w).f;
    let one_e = m_plus.e;
    let one_f = 1u64 << (-one_e);
    let mut p1 = (m_plus.f >> (-one_e)) as u32;
    let mut p2 = m_plus.f & (one_f - 1);
    let (k, mut pow10) = largest_pow10(p1);
    let mut n = k;
    while n > 0 {
        let d = p1 / pow10;
        let r = p1 % pow10;
        buf.push(b'0' + d as u8);
        p1 = r;
        n -= 1;
        let rest = (u64::from(p1) << (-one_e)) + p2;
        if rest <= delta {
            dec_exp += n;
            let ten_n = u64::from(pow10) << (-one_e);
            grisu2_round(buf, dist, delta, rest, ten_n);
            return dec_exp;
        }
        pow10 /= 10;
    }
    let mut m = 0;
    loop {
        p2 *= 10;
        let d = p2 >> (-one_e);
        let r = p2 & (one_f - 1);
        buf.push(b'0' + d as u8);
        p2 = r;
        m += 1;
        delta *= 10;
        dist *= 10;
        if p2 <= delta {
            break;
        }
    }
    dec_exp -= m;
    grisu2_round(buf, dist, delta, p2, one_f);
    dec_exp
}

/// Grisu2 digits and decimal exponent of a finite, positive `value`
/// (`value = digits · 10^exp`).
fn grisu2(value: f64) -> (Vec<u8>, i32) {
    let (m_minus, v, m_plus) = compute_boundaries(value);
    let (cf, ce, ck) = cached_power(m_plus.e);
    let c = DiyFp { f: cf, e: ce };
    let w = DiyFp::mul(v, c);
    let w_minus = DiyFp::mul(m_minus, c);
    let w_plus = DiyFp::mul(m_plus, c);
    let lo = DiyFp {
        f: w_minus.f + 1,
        e: w_minus.e,
    };
    let hi = DiyFp {
        f: w_plus.f - 1,
        e: w_plus.e,
    };
    let mut buf = Vec::with_capacity(17);
    let exp = digit_gen(&mut buf, -ck, lo, w, hi);
    (buf, exp)
}

/// `nlohmann::json(x).dump()` for a `double`: `null` for NaN / infinities,
/// `0.0` / `-0.0`, otherwise Grisu2 digits laid out by `format_buffer`
/// (fixed notation for decimal exponents in `(-4, 15]`, keeping a `.0` on
/// integral values; scientific `d.ddde±XX` otherwise).
pub fn nlohmann_dump_f64(x: f64) -> String {
    if !x.is_finite() {
        return "null".to_string();
    }
    let mut out = String::new();
    let mut value = x;
    if value.is_sign_negative() {
        value = -value;
        out.push('-');
    }
    if value == 0.0 {
        out.push_str("0.0");
        return out;
    }
    let (digits, dec_exp) = grisu2(value);
    const MIN_EXP: i32 = -4;
    const MAX_EXP: i32 = 15;
    let k = digits.len() as i32;
    let n = k + dec_exp;
    let digits = String::from_utf8(digits).unwrap_or_default();
    if k <= n && n <= MAX_EXP {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
        out.push_str(".0");
    } else if 0 < n && n <= MAX_EXP {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if MIN_EXP < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = n - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", e.abs()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::nlohmann_dump_f64 as dump;

    #[test]
    fn matches_nlohmann_dump() {
        // Goldens produced by nlohmann::json(x).dump() (json.hpp 3.11.3, g++ 13).
        let cases: &[(f64, &str)] = &[
            (35.0 / 127.0, "0.27559055118110237"),
            (0.5, "0.5"),
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (2.0 / 3.0, "0.6666666666666666"),
            (100000.0, "100000.0"),
            (1e15, "1e+15"),
            (123456789012345.0, "123456789012345.0"),
            (1e16, "1e+16"),
            (1.5e20, "1.5e+20"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123.456789, "123.456789"),
            (-2.5, "-2.5"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (f64::NAN, "null"),
            (f64::INFINITY, "null"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
        ];
        for &(x, want) in cases {
            assert_eq!(dump(x), want, "{x:e}");
        }
    }
}
