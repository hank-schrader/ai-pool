//! Accept only regex syntax whose grammar compilation we can bound directly.

use super::{MAX_REPEAT, RequestError, invalid};

pub(super) fn cost(pattern: &str, path: &str) -> Result<usize, RequestError> {
    // The pinned converter treats an empty pattern as an unconstrained string.
    if pattern.is_empty() {
        return Ok(0);
    }
    let bad = || invalid(path, "pattern must use anchored ASCII literals/classes and single-atom repetitions");
    if pattern.len() > 1024 || !pattern.is_ascii() || !pattern.starts_with('^') || !pattern.ends_with('$') {
        return Err(bad());
    }
    let bytes = pattern.as_bytes();
    let end = bytes.len() - 1;
    let mut i = 1;
    let mut cost = 1usize;
    while i < end {
        match bytes[i] {
            b'[' => class(bytes, &mut i, end).ok_or_else(bad)?,
            b'\\' => {
                escape(bytes, &mut i, end).ok_or_else(bad)?;
            }
            b'.' => i += 1,
            b if b.is_ascii_graphic() && !b"^$()|*+?{}]".contains(&b) => i += 1,
            b' ' => i += 1,
            _ => return Err(bad()),
        }
        let mut repetitions = 1;
        if i < end {
            match bytes[i] {
                b'?' | b'*' | b'+' => {
                    repetitions = 2;
                    i += 1;
                }
                b'{' => {
                    i += 1;
                    let min = number(bytes, &mut i, end).ok_or_else(bad)?;
                    let max = if i < end && bytes[i] == b',' {
                        i += 1;
                        if i < end && bytes[i] == b'}' {
                            None
                        } else {
                            Some(number(bytes, &mut i, end).ok_or_else(bad)?)
                        }
                    } else {
                        Some(min)
                    };
                    if i >= end || bytes[i] != b'}' {
                        return Err(bad());
                    }
                    i += 1;
                    if max.is_some_and(|max| min > max) {
                        return Err(invalid(path, "pattern repetition maximum must be at least the minimum"));
                    }
                    repetitions = max.unwrap_or(min).max(1) as usize;
                }
                _ => {}
            }
        }
        cost = cost.checked_add(repetitions).ok_or_else(bad)?;
    }
    Ok(cost)
}

fn number(bytes: &[u8], i: &mut usize, end: usize) -> Option<u64> {
    let start = *i;
    let mut n = 0u64;
    while *i < end && bytes[*i].is_ascii_digit() {
        n = n.checked_mul(10)?.checked_add(u64::from(bytes[*i] - b'0'))?;
        if n > MAX_REPEAT {
            return None;
        }
        *i += 1;
    }
    (*i > start).then_some(n)
}

fn escape(bytes: &[u8], i: &mut usize, end: usize) -> Option<u8> {
    *i += 1;
    if *i >= end {
        return None;
    }
    let b = bytes[*i];
    if !b"trn\\\"[]-^$.|?*+(){}".contains(&b) {
        return None;
    }
    *i += 1;
    Some(match b {
        b't' => b'\t',
        b'r' => b'\r',
        b'n' => b'\n',
        _ => b,
    })
}

fn class(bytes: &[u8], i: &mut usize, end: usize) -> Option<()> {
    *i += 1;
    if *i < end && bytes[*i] == b'^' {
        *i += 1;
    }
    let start = *i;
    while *i < end && bytes[*i] != b']' {
        let low = class_char(bytes, i, end)?;
        if *i + 1 < end && bytes[*i] == b'-' && bytes[*i + 1] != b']' {
            *i += 1;
            if class_char(bytes, i, end)? < low {
                return None;
            }
        }
    }
    if *i == start || *i >= end {
        return None;
    }
    *i += 1;
    Some(())
}

fn class_char(bytes: &[u8], i: &mut usize, end: usize) -> Option<u8> {
    if *i >= end {
        return None;
    }
    if bytes[*i] == b'\\' {
        // The runtime supports fewer escapes inside character classes.
        if *i + 1 >= end || !b"trn\\\"[]-".contains(&bytes[*i + 1]) {
            return None;
        }
        return escape(bytes, i, end);
    }
    let b = bytes[*i];
    if !(b.is_ascii_graphic() || b == b' ') || b == b'[' || b == b']' {
        return None;
    }
    *i += 1;
    Some(b)
}
