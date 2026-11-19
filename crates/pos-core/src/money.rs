//! Money. The databases (shared with the Python app) store dollars as REAL; inside the Rust
//! code amounts are whole cents (i64), so adding and comparing is exact. Convert once, at the
//! database boundary, with `to_cents`.

/// Dollars (as stored) -> whole cents, rounded to the nearest cent (half away from zero).
/// NaN and infinities become 0 rather than garbage.
pub fn to_cents(dollars: f64) -> i64 {
    if !dollars.is_finite() {
        return 0;
    }
    (dollars * 100.0).round() as i64
}

/// `1234567` cents with symbol `$` -> `$12,345.67`; negatives read `-$2.50`.
pub fn format_money(cents: i64, symbol: &str) -> String {
    let abs = cents.unsigned_abs();
    let (whole, frac) = (abs / 100, abs % 100);
    let digits = whole.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{}{symbol}{grouped}.{frac:02}", if cents < 0 { "-" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_to_the_nearest_cent() {
        assert_eq!(to_cents(12.34), 1234);
        assert_eq!(to_cents(0.1 + 0.2), 30); // 0.30000000000000004 in floating point
        assert_eq!(to_cents(1258.0), 125_800);
        assert_eq!(to_cents(-2.5), -250);
        assert_eq!(to_cents(f64::NAN), 0);
        assert_eq!(to_cents(f64::INFINITY), 0);
    }

    #[test]
    fn formats_with_separators_and_sign() {
        assert_eq!(format_money(0, "$"), "$0.00");
        assert_eq!(format_money(5, "$"), "$0.05");
        assert_eq!(format_money(125_800, "$"), "$1,258.00");
        assert_eq!(format_money(123_456_789, "J$"), "J$1,234,567.89");
        assert_eq!(format_money(-250, "$"), "-$2.50");
        assert_eq!(format_money(100_000, "$"), "$1,000.00");
        assert_eq!(format_money(99_900, "$"), "$999.00");
    }
}
