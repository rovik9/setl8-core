//! Amount and byte formatting helpers.

use crate::error::{Error, Result};

/// 6-decimal base units to `1,250.000000`.
pub fn fmt_amount(base: u64) -> String {
    format!("{}.{:06}", group(base / 1_000_000), base % 1_000_000)
}

/// `1250`, `1250.5`, `1,250.000001` to base units. At most 6 decimals, no sign, no exponent.
pub fn parse_amount(s: &str) -> Result<u64> {
    let t: String = s.trim().chars().filter(|c| *c != ',').collect();
    if t.is_empty() {
        return Err(Error("amount is empty".into()));
    }
    let (whole, frac) = match t.split_once('.') {
        Some((w, f)) => (w, f),
        None => (t.as_str(), ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return Err(Error(format!("'{s}' is not an amount")));
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(Error(format!("'{s}' is not an amount (digits and one '.' only)")));
    }
    if frac.len() > 6 {
        return Err(Error(format!("'{s}' has more than 6 decimals")));
    }
    let w: u64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| Error(format!("'{s}' is too large")))? };
    let f: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<6}").parse().unwrap() };
    w.checked_mul(1_000_000).and_then(|x| x.checked_add(f)).ok_or_else(|| Error(format!("'{s}' is too large")))
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 1_234_567 to `1,234,567`.
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_round_trip() {
        assert_eq!(fmt_amount(1_250_000_000), "1,250.000000");
        assert_eq!(fmt_amount(0), "0.000000");
        assert_eq!(fmt_amount(1), "0.000001");
        assert_eq!(fmt_amount(u64::MAX), "18,446,744,073,709.551615");
        assert_eq!(parse_amount("1250").unwrap(), 1_250_000_000);
        assert_eq!(parse_amount("1,250.5").unwrap(), 1_250_500_000);
        assert_eq!(parse_amount("0.000001").unwrap(), 1);
        assert_eq!(parse_amount(".5").unwrap(), 500_000);
        assert_eq!(parse_amount("18446744073709.551615").unwrap(), u64::MAX);
    }

    #[test]
    fn bad_amounts_are_refused() {
        for bad in ["", ".", "-1", "1e3", "1.0000001", "abc", "1.2.3", "18446744073709.551616", "99999999999999999999"]
        {
            assert!(parse_amount(bad).is_err(), "{bad}");
        }
    }
}
