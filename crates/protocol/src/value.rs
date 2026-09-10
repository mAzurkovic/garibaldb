//! Values and data types.
//!
//! `Decimal` holds a 128-bit integer and a scale, so 38 digits stay exact.
//! No float appears here, and a `Decimal` travels as a JSON string. This gives
//! `[FR19]`.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The largest count of digits a `DECIMAL` holds. `[FR18]`
pub const MAX_PRECISION: u8 = 38;

/// The type of a column. `[FR14]`-`[FR17]`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Integer,
    Text,
    Boolean,
    Decimal { p: u8, s: u8 },
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Integer => f.write_str("INTEGER"),
            DataType::Text => f.write_str("TEXT"),
            DataType::Boolean => f.write_str("BOOLEAN"),
            DataType::Decimal { p, s } => write!(f, "DECIMAL({p},{s})"),
        }
    }
}

impl FromStr for DataType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let t = s.trim().to_ascii_uppercase();
        match t.as_str() {
            "INTEGER" => return Ok(DataType::Integer),
            "TEXT" => return Ok(DataType::Text),
            "BOOLEAN" => return Ok(DataType::Boolean),
            _ => {}
        }
        let args = t
            .strip_prefix("DECIMAL(")
            .and_then(|a| a.strip_suffix(')'))
            .ok_or_else(|| format!("unknown data type: {s}"))?;
        let (p, s) = args
            .split_once(',')
            .ok_or_else(|| format!("DECIMAL needs a precision and a scale: {s}"))?;
        let p: u8 = p.trim().parse().map_err(|_| "bad precision".to_string())?;
        let s: u8 = s.trim().parse().map_err(|_| "bad scale".to_string())?;
        // [FR18]
        if p == 0 || p > MAX_PRECISION {
            return Err(format!("precision must be 1 to {MAX_PRECISION}"));
        }
        if s > p {
            return Err("scale must not exceed the precision".to_string());
        }
        Ok(DataType::Decimal { p, s })
    }
}

impl Serialize for DataType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DataType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// An exact decimal number. The value is `units * 10^-scale`.
///
/// `PartialEq` compares the stored digits, so `12.20` and `12.2` differ. Use
/// [`Decimal::compare`] for the numeric order that `[FR39]` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    pub units: i128,
    pub scale: u8,
}

/// The count of decimal digits in `units`. Zero has one digit.
fn digits(units: i128) -> u8 {
    let mut n = units.unsigned_abs();
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

impl Decimal {
    /// Tells whether the value fits a `DECIMAL(p, s)` column. `[FR20]`
    ///
    /// Trailing zeros after the point do not count, so `12.20` fits `(3, 1)`.
    pub fn fits(&self, p: u8, s: u8) -> bool {
        let mut units = self.units;
        let mut scale = self.scale;
        while scale > s && units % 10 == 0 {
            units /= 10;
            scale -= 1;
        }
        if scale > s {
            return false;
        }
        u16::from(digits(units)) + u16::from(s - scale) <= u16::from(p)
    }

    /// Compares by numeric value. `12.20` equals `12.2`. `[FR39]`
    pub fn compare(&self, other: &Decimal) -> Ordering {
        match self.scale.cmp(&other.scale) {
            Ordering::Equal => self.units.cmp(&other.units),
            Ordering::Greater => other.compare(self).reverse(),
            // Raise self to the scale of other. An overflow means self is the
            // larger magnitude, because other holds the same scale unraised.
            Ordering::Less => match 10i128
                .checked_pow(u32::from(other.scale - self.scale))
                .and_then(|m| self.units.checked_mul(m))
            {
                Some(units) => units.cmp(&other.units),
                None if self.units < 0 => Ordering::Less,
                None => Ordering::Greater,
            },
        }
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.units);
        }
        let scale = usize::from(self.scale);
        let mut d = self.units.unsigned_abs().to_string();
        if d.len() <= scale {
            d.insert_str(0, &"0".repeat(scale + 1 - d.len()));
        }
        let (int, frac) = d.split_at(d.len() - scale);
        let sign = if self.units < 0 { "-" } else { "" };
        write!(f, "{sign}{int}.{frac}")
    }
}

impl FromStr for Decimal {
    type Err = String;

    /// Reads a decimal and keeps every digit the text holds.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("bad decimal: {s}");
        let (neg, rest) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (int, frac) = match rest.split_once('.') {
            Some((int, frac)) => {
                if frac.is_empty() {
                    return Err(bad());
                }
                (int, frac)
            }
            None => (rest, ""),
        };
        let all = format!("{int}{frac}");
        if all.is_empty() || !all.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        if frac.len() > usize::from(MAX_PRECISION) {
            return Err(format!("more than {MAX_PRECISION} digits after the point"));
        }
        let units: i128 = all
            .parse()
            .map_err(|_| format!("more than {MAX_PRECISION} digits"))?;
        if digits(units) > MAX_PRECISION {
            return Err(format!("more than {MAX_PRECISION} digits"));
        }
        Ok(Decimal {
            units: if neg { -units } else { units },
            scale: frac.len() as u8,
        })
    }
}

impl Serialize for Decimal {
    /// Writes a JSON string. A JSON number would become a float and lose digits.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Decimal {
    /// Reads a string. A JSON number is an error.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// One value in a row. `Null` is the empty value. `[FR21]`
///
/// `PartialEq` compares a decimal by its stored digits, so
/// `Decimal(12.20) != Decimal(12.2)`. SQL `=` MUST call [`Value::compare`],
/// never `==`, or `[FR39]` breaks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Integer(i64),
    Text(String),
    Boolean(bool),
    Decimal(Decimal),
    Null,
}

impl Value {
    /// The type of the value. `Null` has no type.
    ///
    /// A decimal reports the smallest type that holds its digits.
    pub fn type_of(&self) -> Option<DataType> {
        match self {
            Value::Integer(_) => Some(DataType::Integer),
            Value::Text(_) => Some(DataType::Text),
            Value::Boolean(_) => Some(DataType::Boolean),
            Value::Decimal(d) => Some(DataType::Decimal {
                p: digits(d.units).max(d.scale),
                s: d.scale,
            }),
            Value::Null => None,
        }
    }

    /// Compares two values. A `Null` or a type mismatch gives none. `[FR36]`
    ///
    /// `TEXT` follows the byte order of the UTF-8 encoding. `[FR38]`
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
            (Value::Text(a), Value::Text(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
            (Value::Boolean(a), Value::Boolean(b)) => Some(a.cmp(b)),
            (Value::Decimal(a), Value::Decimal(b)) => Some(a.compare(b)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn text_of_a_decimal_keeps_every_digit() {
        for s in [
            "0",
            "12.2",
            "12.20",
            "-0.5",
            "1002.2",
            "0.00",
            "-1234567890.0000000001",
            "99999999999999999999999999999999999999",
            "0.99999999999999999999999999999999999999",
        ] {
            assert_eq!(dec(s).to_string(), s, "round trip of {s}");
        }
    }

    #[test]
    fn a_leading_plus_and_a_bare_point_read() {
        assert_eq!(dec("+7"), Decimal { units: 7, scale: 0 });
        assert_eq!(
            dec(".50"),
            Decimal {
                units: 50,
                scale: 2
            }
        );
    }

    #[test]
    fn bad_text_gives_an_error() {
        for s in ["", "-", "1.", "1.2.3", "++5", "1 2", "abc", "1e5", "１"] {
            assert!(s.parse::<Decimal>().is_err(), "{s} must fail");
        }
    }

    #[test]
    fn more_than_38_digits_gives_an_error() {
        assert!("1".repeat(39).parse::<Decimal>().is_err());
        assert!(
            "100000000000000000000000000000000000000" // 1e38, 39 digits
                .parse::<Decimal>()
                .is_err()
        );
        assert!(format!("0.{}", "0".repeat(39)).parse::<Decimal>().is_err());
        assert!("1".repeat(38).parse::<Decimal>().is_ok());
    }

    #[test]
    fn fits_holds_at_the_boundary() {
        // value, p, s, expected
        let cases = [
            ("99999", 5, 0, true),
            ("999999", 5, 0, false),
            ("12.20", 4, 2, true),
            ("12.20", 3, 1, true),  // the trailing zero does not count
            ("12.25", 3, 1, false), // a digit is lost
            ("12.20", 2, 1, false), // too many digits before the point
            ("-12.2", 3, 1, true),
            ("0.00", 2, 2, true),
            ("0", 1, 0, true),
        ];
        for (v, p, s, want) in cases {
            assert_eq!(dec(v).fits(p, s), want, "{v} in DECIMAL({p},{s})");
        }
    }

    #[test]
    fn comparison_aligns_the_scales() {
        assert_eq!(dec("12.20").compare(&dec("12.2")), Ordering::Equal);
        assert_eq!(dec("2.10").compare(&dec("10.2")), Ordering::Less);
        assert_eq!(dec("10.2").compare(&dec("2.10")), Ordering::Greater);
        assert_eq!(dec("-0.5").compare(&dec("-0.50")), Ordering::Equal);
        assert_eq!(dec("-1").compare(&dec("0.1")), Ordering::Less);
    }

    #[test]
    fn comparison_survives_an_overflow_of_the_alignment() {
        let big = dec("99999999999999999999999999999999999999"); // scale 0
        let small = dec("0.00000000000000000000000000000000000001"); // scale 38
        assert_eq!(big.compare(&small), Ordering::Greater);
        assert_eq!(small.compare(&big), Ordering::Less);
        let neg = dec("-99999999999999999999999999999999999999");
        assert_eq!(neg.compare(&small), Ordering::Less);
        assert_eq!(small.compare(&neg), Ordering::Greater);
    }

    #[test]
    fn a_decimal_travels_as_a_json_string() {
        let d = dec("1002.2");
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"1002.2\"");
        assert_eq!(serde_json::from_str::<Decimal>("\"1002.2\"").unwrap(), d);
    }

    #[test]
    fn a_json_number_is_not_a_decimal() {
        assert!(serde_json::from_str::<Decimal>("1002.2").is_err());
        assert!(serde_json::from_str::<Decimal>("1002").is_err());
        assert!(serde_json::from_str::<Decimal>("\"abc\"").is_err());
    }

    #[test]
    fn a_data_type_travels_as_a_string() {
        let cases = [
            (DataType::Integer, "\"INTEGER\""),
            (DataType::Text, "\"TEXT\""),
            (DataType::Boolean, "\"BOOLEAN\""),
            (DataType::Decimal { p: 10, s: 2 }, "\"DECIMAL(10,2)\""),
        ];
        for (ty, json) in cases {
            assert_eq!(serde_json::to_string(&ty).unwrap(), json);
            assert_eq!(serde_json::from_str::<DataType>(json).unwrap(), ty);
        }
    }

    #[test]
    fn a_bad_data_type_gives_an_error() {
        for s in [
            "FLOAT",
            "DECIMAL",
            "DECIMAL(10)",
            "DECIMAL(39,2)", // [FR18] p is at most 38
            "DECIMAL(0,0)",
            "DECIMAL(2,3)", // [FR18] s is at most p
            "DECIMAL(a,b)",
        ] {
            assert!(s.parse::<DataType>().is_err(), "{s} must fail");
        }
    }

    #[test]
    fn type_of_reports_the_variant() {
        assert_eq!(Value::Integer(1).type_of(), Some(DataType::Integer));
        assert_eq!(Value::Text("a".into()).type_of(), Some(DataType::Text));
        assert_eq!(Value::Boolean(true).type_of(), Some(DataType::Boolean));
        assert_eq!(
            Value::Decimal(dec("12.20")).type_of(),
            Some(DataType::Decimal { p: 4, s: 2 })
        );
        assert_eq!(
            Value::Decimal(dec("0.05")).type_of(),
            Some(DataType::Decimal { p: 2, s: 2 })
        );
        assert_eq!(Value::Null.type_of(), None);
    }

    #[test]
    fn a_comparison_with_null_gives_none() {
        assert_eq!(Value::Null.compare(&Value::Integer(1)), None);
        assert_eq!(Value::Integer(1).compare(&Value::Null), None);
        assert_eq!(Value::Null.compare(&Value::Null), None);
    }

    #[test]
    fn a_comparison_of_two_types_gives_none() {
        assert_eq!(Value::Integer(1).compare(&Value::Text("1".into())), None);
        assert_eq!(
            Value::Boolean(true).compare(&Value::Decimal(dec("1"))),
            None
        );
    }

    #[test]
    fn values_of_one_type_compare() {
        assert_eq!(
            Value::Integer(1).compare(&Value::Integer(2)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::Text("a".into()).compare(&Value::Text("B".into())),
            Some(Ordering::Greater) // byte order, [FR38]
        );
        assert_eq!(
            Value::Boolean(false).compare(&Value::Boolean(true)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::Decimal(dec("12.20")).compare(&Value::Decimal(dec("12.2"))),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn a_value_travels_as_json() {
        for v in [
            Value::Integer(-1),
            Value::Text("hé".into()),
            Value::Boolean(true),
            Value::Decimal(dec("1002.20")),
            Value::Null,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), v, "{json}");
        }
    }
}
