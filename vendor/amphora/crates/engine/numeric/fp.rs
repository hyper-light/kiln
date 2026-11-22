//! A REAL as SQLite 3.54 writes it in text: `sqlite3FpDecode` and the `%!.17g` its
//! `vdbeMemRenderNum` renders (REAL to TEXT), and `%!0.17g` (quote() and JSON: the same digits,
//! infinity as `9.0e+999`), ported from its amalgamation (sqlite3.c 3.54.0, printf.c and util.c).
//! The digits come from a fixed-point product with a power of ten (Russ Cox's fpfmt as SQLite adapts
//! it), cut at 17 to the fewest that convert back to the same double; they aren't always the
//! correctly rounded ones (1/3 is 0.33333333333333332), so only a port answers as SQLite does.
//!
//! Every shift here moves by an amount the algorithm keeps within the word (SQLite asserts as much);
//! a shift out of range, which no double reaches, ends the conversion with `None`, and the caller
//! writes the double as Rust does.

/// The powers of ten the conversions scale by, from 10^-348 to 10^347.
const POWERS_OF_10_FIRST: i32 = -348;
const POWERS_OF_10_LAST: i32 = 347;

/// 10^0 to 10^26, each its top 64 bits (sqlite3.c powerOfTen's aBase[]).
const BASE: [u64; 27] = [
    0x8000_0000_0000_0000,
    0xa000_0000_0000_0000,
    0xc800_0000_0000_0000,
    0xfa00_0000_0000_0000,
    0x9c40_0000_0000_0000,
    0xc350_0000_0000_0000,
    0xf424_0000_0000_0000,
    0x9896_8000_0000_0000,
    0xbebc_2000_0000_0000,
    0xee6b_2800_0000_0000,
    0x9502_f900_0000_0000,
    0xba43_b740_0000_0000,
    0xe8d4_a510_0000_0000,
    0x9184_e72a_0000_0000,
    0xb5e6_20f4_8000_0000,
    0xe35f_a931_a000_0000,
    0x8e1b_c9bf_0400_0000,
    0xb1a2_bc2e_c500_0000,
    0xde0b_6b3a_7640_0000,
    0x8ac7_2304_89e8_0000,
    0xad78_ebc5_ac62_0000,
    0xd8d7_26b7_177a_8000,
    0x8786_7832_6eac_9000,
    0xa968_163f_0a57_b400,
    0xd3c2_1bce_cced_a100,
    0x8459_5161_4014_84a0,
    0xa56f_a5b9_9019_a5c8,
];

/// 10^(27k) for k from -13 to 12, each its top 64 bits (aScale[]) and the next 32 (aScaleLo[]);
/// index 13 is 10^-1, a special case.
const SCALE: [u64; 26] = [
    0x8049_a4ac_0c58_11ae,
    0xcf42_894a_5dce_35ea,
    0xa76c_5823_38ed_2621,
    0x873e_4f75_e222_4e68,
    0xda7f_5bf5_9096_6848,
    0xb080_392c_c434_9dec,
    0x8e93_8662_882a_f53e,
    0xe658_29b3_046b_0afa,
    0xba12_1a46_50e4_ddeb,
    0x964e_858c_91ba_2655,
    0xf2d5_6790_ab41_c2a2,
    0xc428_d05a_a475_1e4c,
    0x9e74_d1b7_91e0_7e48,
    0xcccc_cccc_cccc_cccc,
    0xcecb_8f27_f420_0f3a,
    0xa70c_3c40_a64e_6c51,
    0x86f0_ac99_b4e8_dafd,
    0xda01_ee64_1a70_8de9,
    0xb01a_e745_b101_e9e4,
    0x8e41_ade9_fbeb_c27d,
    0xe5d3_ef28_2a24_2e81,
    0xb9a7_4a06_37ce_2ee1,
    0x95f8_3d0a_1fb6_9cd9,
    0xf24a_01a7_3cf2_dccf,
    0xc3b8_3581_09e8_4f07,
    0x9e19_db92_b4e3_1ba9,
];

const SCALE_LO: [u32; 26] = [
    0x205b_896d,
    0x5206_4cad,
    0xaf2a_f2b8,
    0x5a77_44a7,
    0xaf39_a475,
    0xbd8d_794e,
    0x547e_b47b,
    0x0cb4_a5a3,
    0x92f3_4d62,
    0x3a6a_07f9,
    0xfae2_7299,
    0xaa97_e14c,
    0x775e_a265,
    0xcccc_cccc,
    0x0000_0000,
    0x9990_90b6,
    0x69a0_28bb,
    0xe80e_6f48,
    0x5ec0_5dd0,
    0x1458_8f14,
    0x8f16_68c9,
    0x6d95_3e2c,
    0x4abd_af10,
    0xbc63_3b39,
    0x0a86_2f81,
    0x6c07_a2c2,
];

const BIT63: u64 = 0x8000_0000_0000_0000;

fn hi64(x: u128) -> u64 {
    u64::try_from(x.wrapping_shr(64)).unwrap_or(u64::MAX)
}

fn lo64(x: u128) -> u64 {
    u64::try_from(x & u128::from(u64::MAX)).unwrap_or(0)
}

/// a*b: its high 64 bits and its low 64 (sqlite3Multiply128).
fn multiply128(a: u64, b: u64) -> (u64, u64) {
    let r = u128::from(a).wrapping_mul(u128::from(b));
    (hi64(r), lo64(r))
}

/// A = (a << 32) + a_lo times b: the top 64 bits of the 160-bit product and the 32 after them
/// (sqlite3Multiply160). The product of 64 and 64 bits fits 128, and the carry from a_lo*b >> 32
/// stays under it.
fn multiply160(a: u64, a_lo: u32, b: u64) -> (u64, u32) {
    let r = u128::from(a).wrapping_mul(u128::from(b)).wrapping_add(
        u128::from(a_lo)
            .wrapping_mul(u128::from(b))
            .wrapping_shr(32),
    );
    (
        hi64(r),
        u32::try_from(r.wrapping_shr(32) & 0xffff_ffff).unwrap_or(0),
    )
}

/// The top 64 bits of 10^p, and the 32 after them, for p from -348 to 347 (powerOfTen).
fn power_of_ten(p: i32) -> Option<(u64, u32)> {
    if !(POWERS_OF_10_FIRST..=POWERS_OF_10_LAST).contains(&p) {
        return None;
    }
    let (g, n) = if p < 0 {
        if p == -1 {
            return Some((*SCALE.get(13)?, *SCALE_LO.get(13)?));
        }
        let (g, n) = (p.checked_div(27)?, p.checked_rem(27)?);
        if n != 0 {
            (g.checked_sub(1)?, n.checked_add(27)?)
        } else {
            (g, n)
        }
    } else if p < 27 {
        return Some((*BASE.get(usize::try_from(p).ok()?)?, 0));
    } else {
        (p.checked_div(27)?, p.checked_rem(27)?)
    };
    let at = usize::try_from(g.checked_add(13)?).ok()?;
    let (s, s_lo) = (*SCALE.get(at)?, *SCALE_LO.get(at)?);
    if n == 0 {
        return Some((s, s_lo));
    }
    let (mut x, mut lo) = multiply160(s, s_lo, *BASE.get(usize::try_from(n).ok()?)?);
    if x & BIT63 == 0 {
        x = x.wrapping_shl(1) | u64::from(lo.wrapping_shr(31) & 1);
        lo = lo.wrapping_shl(1) | 1;
    }
    Some((x, lo))
}

/// floor(log2(10^p)) (pwr10to2): |p| stays under 400, so the product fits 32 bits.
fn pwr10to2(p: i32) -> i32 {
    p.wrapping_mul(108_853).wrapping_shr(15)
}

/// floor(log10(2^p)) (pwr2to10): |p| stays under 1200.
fn pwr2to10(p: i32) -> i32 {
    p.wrapping_mul(78_913).wrapping_shr(18)
}

fn shift_amount(by: i32) -> Option<u32> {
    u32::try_from(by).ok().filter(|s| *s < 64)
}

/// m * 2^e as d * 10^p with at least `n` significant digits in d (sqlite3Fp2Convert10). m has its
/// top bit set.
fn fp2_convert10(m: u64, e: i32, n: i32) -> Option<(u64, i32)> {
    let p = n
        .checked_sub(1)?
        .checked_sub(pwr2to10(e.checked_add(63)?))?;
    let (ten, _) = power_of_ten(p)?;
    let (h, _) = multiply128(m, ten);
    let base = e.checked_add(pwr10to2(p))?;
    let d = if n == 18 {
        let h = h.checked_shr(shift_amount(base.checked_add(2)?.checked_neg()?)?)?;
        h.wrapping_add(h.wrapping_shl(1) & 2).wrapping_shr(1)
    } else {
        h.checked_shr(shift_amount(base.checked_add(1)?.checked_neg()?)?)?
    };
    Some((d, p.checked_neg()?))
}

/// The double nearest d * 10^p (sqlite3Fp10Convert2, after Russ Cox's fpfmt).
fn fp10_convert2(d: u64, p: i32) -> Option<f64> {
    if p < POWERS_OF_10_FIRST {
        return Some(0.0);
    }
    if p > POWERS_OF_10_LAST {
        return Some(f64::INFINITY);
    }
    let b = 64_i32.checked_sub(i32::try_from(d.leading_zeros()).ok()?)?;
    let lp = pwr10to2(p);
    let mut e = 53_i32.checked_sub(b)?.checked_sub(lp)?;
    if e > 1074 {
        if e >= 1130 {
            return Some(0.0);
        }
        e = 1074;
    }
    let s = e
        .checked_sub(64_i32.checked_sub(b)?)?
        .checked_add(lp)?
        .checked_add(3)?
        .checked_neg()?;
    let (mut pwr10h, mut pwr10l) = power_of_ten(p)?;
    if pwr10l != 0 {
        pwr10h = pwr10h.wrapping_add(1);
        pwr10l = !pwr10l;
    }
    let x = d
        .checked_shl(u32::try_from(64_i32.checked_sub(b)?).ok()?)
        .unwrap_or(0);
    let (mut hi, lo) = multiply128(x, pwr10h);
    let mid1 = u32::try_from(lo.wrapping_shr(32)).ok()?;
    let mut sticky = 1_u64;
    let s_bits = shift_amount(s)?;
    if hi & (1_u64.checked_shl(s_bits)?.wrapping_sub(1)) == 0 {
        let (h2, _) = multiply128(x, u64::from(pwr10l).wrapping_shl(32));
        let mid2 = u32::try_from(h2.wrapping_shr(32)).ok()?;
        sticky = u64::from(mid1.wrapping_sub(mid2) > 1);
        hi = hi.wrapping_sub(u64::from(mid1 < mid2));
    }
    let mut u = hi.checked_shr(s_bits)? | sticky;
    if u >= 0x0080_0000_0000_0000_u64.wrapping_sub(2) {
        u = u.wrapping_shr(1) | (u & 1);
        e = e.checked_sub(1)?;
    }
    let mut m = u
        .wrapping_add(1)
        .wrapping_add(u.wrapping_shr(2) & 1)
        .wrapping_shr(2);
    if e <= -972 {
        return Some(f64::INFINITY);
    }
    if m & 0x0010_0000_0000_0000 != 0 {
        let exponent = u64::try_from(1075_i32.checked_sub(e)?).ok()?;
        m = (m & !0x0010_0000_0000_0000_u64) | exponent.checked_shl(52)?;
    }
    Some(f64::from_bits(m))
}

/// A double's decimal digits (sqlite3FpDecode's FpDecode): `digits` its significant digits,
/// `point` where the decimal point falls among them, `negative` its sign.
struct Decoded {
    negative: bool,
    digits: Vec<u8>,
    point: i32,
}

enum Decode {
    Finite(Decoded),
    Infinite { negative: bool },
    NaN,
}

/// sqlite3FpDecode(r, round, max_round) for round > 0 (the `%g` its printf renders; `%f`'s round <= 0
/// isn't needed here): `round` significant digits at most, or `max_round`; at exactly 17 (the `!`
/// flag's), the fewest that convert back to r.
fn decode(r: f64, round: i32, max_round: i32) -> Option<Decode> {
    let negative = r.is_sign_negative() && r != 0.0;
    let r = r.abs();
    if r == 0.0 {
        return Some(Decode::Finite(Decoded {
            negative: false,
            digits: vec![b'0'],
            point: 1,
        }));
    }
    let bits = r.to_bits();
    let biased = i32::try_from(bits.wrapping_shr(52) & 0x7ff).ok()?;
    if biased == 0x7ff {
        return Some(if bits == 0x7ff0_0000_0000_0000 {
            Decode::Infinite { negative }
        } else {
            Decode::NaN
        });
    }
    let mut v = bits & 0x000f_ffff_ffff_ffff;
    let e = if biased == 0 {
        let nn = v.leading_zeros();
        v = v.checked_shl(nn)?;
        (-1074_i32).checked_sub(i32::try_from(nn).ok()?)?
    } else {
        v = v.wrapping_shl(11) | BIT63;
        biased.checked_sub(1086)?
    };
    let (mut v, exp) = fp2_convert10(
        v,
        e,
        if round >= 18 {
            18
        } else {
            round.checked_add(1)?
        },
    )?;
    // the digits, most significant first, with room for a carry into a new first digit
    let mut buf: Vec<u8> = Vec::with_capacity(21);
    while v > 0 {
        buf.push(b'0'.checked_add(u8::try_from(v.checked_rem(10)?).ok()?)?);
        v = v.checked_div(10)?;
    }
    buf.push(b'0');
    buf.reverse();
    let mut start = 1usize;
    let mut n = i32::try_from(buf.len().checked_sub(1)?).ok()?;
    let mut point = n.checked_add(exp)?;
    let digit = |buf: &Vec<u8>, start: usize, i: i32| -> Option<u8> {
        buf.get(start.checked_add(usize::try_from(i).ok()?)?)
            .copied()
    };
    let mut round = round;
    if round > 0 && (round < n || n > max_round) {
        if round > max_round {
            round = max_round;
        }
        if round == 17 {
            // 17 digits only with the `!` flag: fewer, when they convert back to r (49.47, not
            // 49.469999999999999)
            if digit(&buf, start, 15)? == b'9' && digit(&buf, start, 14)? == b'9' {
                let mut jj: i32 = 14;
                while jj > 0 && digit(&buf, start, jj.checked_sub(1)?)? == b'9' {
                    jj = jj.checked_sub(1)?;
                }
                let v2 = if jj == 0 {
                    1_u64
                } else {
                    let mut v2 = 0_u64;
                    for kk in 0..jj {
                        v2 = v2
                            .checked_mul(10)?
                            .checked_add(u64::from(digit(&buf, start, kk)?.checked_sub(b'0')?))?;
                    }
                    v2.checked_add(1)?
                };
                if fp10_convert2(v2, exp.checked_add(n)?.checked_sub(jj)?) == Some(r) {
                    round = jj.checked_add(1)?;
                }
            } else if point >= n
                || (digit(&buf, start, 15)? == b'0'
                    && digit(&buf, start, 14)? == b'0'
                    && digit(&buf, start, 13)? == b'0')
            {
                let mut jj: i32 = 13;
                while digit(&buf, start, jj.checked_sub(1)?)? == b'0' {
                    jj = jj.checked_sub(1)?;
                }
                let mut v2 = 0_u64;
                for kk in 0..jj {
                    v2 = v2
                        .checked_mul(10)?
                        .checked_add(u64::from(digit(&buf, start, kk)?.checked_sub(b'0')?))?;
                }
                if fp10_convert2(v2, exp.checked_add(n)?.checked_sub(jj)?) == Some(r) {
                    round = jj.checked_add(1)?;
                }
            }
        }
        n = round;
        if digit(&buf, start, round)? >= b'5' {
            let mut j = round.checked_sub(1)?;
            loop {
                let at = start.checked_add(usize::try_from(j).ok()?)?;
                let d = buf.get_mut(at)?;
                *d = d.checked_add(1)?;
                if *d <= b'9' {
                    break;
                }
                *d = b'0';
                if j == 0 {
                    start = start.checked_sub(1)?;
                    *buf.get_mut(start)? = b'1';
                    n = n.checked_add(1)?;
                    point = point.checked_add(1)?;
                    break;
                }
                j = j.checked_sub(1)?;
            }
        }
    }
    while n > 1 && digit(&buf, start, n.checked_sub(1)?)? == b'0' {
        n = n.checked_sub(1)?;
    }
    let digits = buf
        .get(start..start.checked_add(usize::try_from(n).ok()?)?)?
        .to_vec();
    Some(Decode::Finite(Decoded {
        negative,
        digits,
        point,
    }))
}

/// `%!.17g` of r, as SQLite's printf writes it (`zero_pad`: `%!0.17g`, infinity as 9.0e+999 and
/// NaN as null): fixed notation for exponents from -4 to 16, else d.ddde±XX; trailing zeros dropped
/// but one after the point.
fn render(r: f64, zero_pad: bool) -> Option<String> {
    let s = match decode(r, 17, 20)? {
        Decode::Finite(s) => s,
        Decode::NaN => return Some(if zero_pad { "null" } else { "NaN" }.to_owned()),
        Decode::Infinite { negative } if zero_pad => Decoded {
            negative,
            digits: vec![b'9'],
            point: 1000,
        },
        Decode::Infinite { negative } => {
            return Some(if negative { "-Inf" } else { "Inf" }.to_owned())
        }
    };
    let n = i32::try_from(s.digits.len()).ok()?;
    let exp = s.point.checked_sub(1)?;
    let mut precision: i32 = 16;
    let scientific = exp < -4 || exp > precision;
    if !scientific {
        precision = precision.checked_sub(exp)?;
    }
    let mut e2 = if scientific {
        0
    } else {
        s.point.checked_sub(1)?
    };
    let mut out = String::with_capacity(32);
    if s.negative {
        out.push('-');
    }
    let digit = |i: i32| -> Option<char> {
        s.digits
            .get(usize::try_from(i).ok()?)
            .map(|d| char::from(*d))
    };
    // digits before the point
    let mut j: i32 = 0;
    if e2 < 0 {
        out.push('0');
    } else {
        j = e2.checked_add(1)?.min(n);
        for i in 0..j {
            out.push(digit(i)?);
        }
        e2 = e2.checked_sub(j)?;
        if e2 >= 0 {
            for _ in 0..e2.checked_add(1)? {
                out.push('0');
            }
            e2 = -1;
        }
    }
    out.push('.');
    // zeros after the point before the first significant digit
    if e2 < -1 && precision > 0 {
        let nn = (-1_i32).checked_sub(e2)?.min(precision);
        for _ in 0..nn {
            out.push('0');
        }
        precision = precision.checked_sub(nn)?;
    }
    if precision > 0 {
        let nn = n.checked_sub(j)?.min(precision);
        for i in j..j.checked_add(nn)? {
            out.push(digit(i)?);
        }
    }
    while out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.push('0');
    }
    if scientific {
        let mut exp = s.point.checked_sub(1)?;
        out.push('e');
        if exp < 0 {
            out.push('-');
            exp = exp.checked_neg()?;
        } else {
            out.push('+');
        }
        if exp >= 100 {
            out.push(char::from(
                b'0'.checked_add(u8::try_from(exp.checked_div(100)?).ok()?)?,
            ));
            exp = exp.checked_rem(100)?;
        }
        out.push(char::from(
            b'0'.checked_add(u8::try_from(exp.checked_div(10)?).ok()?)?,
        ));
        out.push(char::from(
            b'0'.checked_add(u8::try_from(exp.checked_rem(10)?).ok()?)?,
        ));
    }
    Some(out)
}

/// A REAL as text, as SQLite 3.54 converts one (`%!.17g`).
pub fn real_to_text(r: f64) -> String {
    render(r, false).unwrap_or_else(|| format!("{r:e}"))
}

/// A REAL as quote() and JSON write it (`%!0.17g`): infinity as 9.0e+999.
pub fn real_to_quoted(r: f64) -> String {
    render(r, true).unwrap_or_else(|| format!("{r:e}"))
}
