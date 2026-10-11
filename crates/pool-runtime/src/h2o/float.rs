//! Python `repr(float)` formatting (shortest round-trip decimal), ported from
//! refactor-tool's `lightning_float.rs` so state and descriptions render identically.
use std::cmp::Ordering;

pub fn render(value: f64) -> String {
    // json.dumps defaults to allow_nan=True, independent of the JSON layout.
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() { "-Infinity" } else { "Infinity" }.into();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0.0");
    }
    let absolute = value.abs();
    let (digits, exponent) = exact(absolute);
    let (coefficient, scale) = shortest(&digits, exponent, absolute);
    format!("{sign}{}", notation(coefficient, scale))
}

/// Nonzero finite binary64 is exactly M*2^E. For E<0 use M*5^-E*10^E;
/// otherwise multiply M by 2^E. No float formatting or rounded arithmetic.
fn exact(value: f64) -> (Vec<u8>, i32) {
    let bits = value.to_bits();
    let encoded_exponent = ((bits >> 52) & 0x7ff) as i32;
    let mut mantissa = bits & ((1_u64 << 52) - 1);
    let mut exponent = -1074;
    if encoded_exponent != 0 {
        mantissa |= 1_u64 << 52;
        exponent = encoded_exponent - 1023 - 52;
    }
    // Cancel denominator powers first, avoiding unnecessary decimal zeros.
    if exponent < 0 {
        let cancel = mantissa.trailing_zeros().min((-exponent) as u32);
        mantissa >>= cancel;
        exponent += cancel as i32;
    }
    let mut digits: Vec<u8> = mantissa.to_string().bytes().rev().map(|b| b - b'0').collect();
    let factor = if exponent < 0 { 5 } else { 2 };
    for _ in 0..exponent.unsigned_abs() {
        let mut carry = 0;
        for digit in &mut digits {
            let product = *digit * factor + carry;
            *digit = product % 10;
            carry = product / 10;
        }
        if carry != 0 {
            digits.push(carry);
        }
    }
    digits.reverse();
    let decimal_exponent = digits.len() as i32 - 1 + exponent.min(0);
    (digits, decimal_exponent)
}

fn shortest(digits: &[u8], exponent: i32, value: f64) -> (u64, i32) {
    let mut lower = 0;
    // Seventeen significant digits suffice for every finite binary64 value.
    for precision in 1..=17 {
        lower = lower * 10 + u64::from(*digits.get(precision - 1).unwrap_or(&0));
        let scale = exponent - precision as i32 + 1;
        let remainder = digits.get(precision..).unwrap_or_default();
        let exact_lower = remainder.iter().all(|&d| d == 0);
        // The decimal grid brackets the exact value with these two neighbors.
        // Its binary64 rounding interval is contiguous and contains that value:
        // if either side contains a grid point, its nearest neighbor is valid.
        // Check both sides even at powers of two, whose interval is asymmetric.
        let lower_valid = roundtrips(lower, scale, value);
        let upper_valid = !exact_lower && roundtrips(lower + 1, scale, value);
        let chosen = match (lower_valid, upper_valid) {
            (false, false) => continue,
            (true, false) => lower,
            (false, true) => lower + 1,
            (true, true) => {
                // Compare the *exact* discarded fraction with one half. At an
                // exact tie choose the even coefficient, never a rounded f64.
                let half = match remainder[0].cmp(&5) {
                    Ordering::Equal if remainder[1..].iter().any(|&d| d != 0) => Ordering::Greater,
                    comparison => comparison,
                };
                if half == Ordering::Less || (half == Ordering::Equal && lower % 2 == 0) { lower } else { lower + 1 }
            }
        };
        return (chosen, scale);
    }
    unreachable!("binary64 always has a 17-digit round-trip decimal")
}

fn roundtrips(coefficient: u64, scale: i32, value: f64) -> bool {
    format!("{coefficient}e{scale}").parse::<f64>().expect("integer decimal candidate").to_bits() == value.to_bits()
}

fn notation(mut coefficient: u64, mut scale: i32) -> String {
    while coefficient.is_multiple_of(10) {
        coefficient /= 10;
        scale += 1;
    }
    let mut digits = coefficient.to_string();
    let exponent = digits.len() as i32 - 1 + scale;
    // Python uses scientific notation outside -4..=15, signed exponents of
    // at least two digits, and .0 for integral floats in fixed notation.
    if !(-4..16).contains(&exponent) {
        if digits.len() > 1 {
            digits.insert(1, '.');
        }
        return format!("{digits}e{exponent:+03}");
    }
    let point = digits.len() as i32 + scale;
    if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point >= digits.len() as i32 {
        format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
    } else {
        digits.insert(point as usize, '.');
        digits
    }
}
