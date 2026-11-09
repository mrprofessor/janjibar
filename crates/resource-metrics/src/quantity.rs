//! Kubernetes resource quantities, such as `250m`, `12345n` or `512Mi`.
//!
//! The grammar is a signed decimal number, then either a suffix (`n`, `u`,
//! `m`, `k`, `M`, `G`, `T`, `P`, `E`, `Ki`, `Mi`, `Gi`, `Ti`, `Pi`, `Ei`) or
//! a decimal exponent (`e3`, `E-2`). See `resource.Quantity` in apimachinery.

/// The value of a quantity: cores for CPU, bytes for memory.
///
/// Returns `None` for text that is not a quantity.
pub fn parse_quantity(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '+' | '-')))
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(split);
    if number.is_empty() || number == "+" || number == "-" {
        return None;
    }
    let number: f64 = number.parse().ok()?;
    Some(number * multiplier(suffix)?)
}

fn multiplier(suffix: &str) -> Option<f64> {
    const KI: f64 = 1024.0;
    let value = match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => KI,
        "Mi" => KI * KI,
        "Gi" => KI * KI * KI,
        "Ti" => KI * KI * KI * KI,
        "Pi" => KI * KI * KI * KI * KI,
        "Ei" => KI * KI * KI * KI * KI * KI,
        // `e3` and `E-2` are exponents; a bare `E` (exa) was matched above.
        exponent => {
            let digits = exponent
                .strip_prefix('e')
                .or_else(|| exponent.strip_prefix('E'))?;
            10f64.powi(digits.parse().ok()?)
        }
    };
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() <= b.abs() * 1e-12)
    }

    #[test]
    fn reads_cpu_as_cores() {
        assert!(close(parse_quantity("2"), 2.0));
        assert!(close(parse_quantity("250m"), 0.25));
        assert!(close(parse_quantity("1500u"), 0.0015));
        // metrics-server reports CPU in nanocores.
        assert!(close(parse_quantity("123456789n"), 0.123_456_789));
        assert!(close(parse_quantity("0.5"), 0.5));
    }

    #[test]
    fn reads_memory_as_bytes() {
        assert!(close(parse_quantity("12345"), 12345.0));
        assert!(close(parse_quantity("128974848"), 128_974_848.0));
        assert!(close(parse_quantity("4Ki"), 4096.0));
        assert!(close(parse_quantity("512Mi"), 536_870_912.0));
        assert!(close(parse_quantity("1.5Gi"), 1_610_612_736.0));
        assert!(close(parse_quantity("1k"), 1000.0));
        assert!(close(parse_quantity("2M"), 2e6));
        assert!(close(parse_quantity("1E"), 1e18));
    }

    #[test]
    fn reads_exponents() {
        assert!(close(parse_quantity("1e3"), 1000.0));
        assert!(close(parse_quantity("5E-2"), 0.05));
        assert!(close(parse_quantity("-1e3"), -1000.0));
    }

    #[test]
    fn rejects_what_is_not_a_quantity() {
        assert_eq!(parse_quantity(""), None);
        assert_eq!(parse_quantity("Mi"), None);
        assert_eq!(parse_quantity("-"), None);
        assert_eq!(parse_quantity("12XB"), None);
        assert_eq!(parse_quantity("1e"), None);
        assert_eq!(parse_quantity("1.2.3"), None);
    }
}
