//! Canonical JSON and digests. Desired state and approval statements are signed over these bytes, so they must be the bytes
//! droidd's `canonicalJson` and `paramsDigest` produce.
//!
//! The canonical form is what `JSON.stringify` writes for a value whose object keys are sorted by UTF-16 code unit, with no
//! whitespace. A number is a double, as it is in JavaScript, so it is written the way ECMAScript's `Number::toString` writes
//! it. An integer above 2^53 therefore rounds to the double that droidd would hold.
//!
//! Non-finite numbers: JSON has no NaN or infinity, and serde_json cannot hold one either. `serde_json::to_value(f64::NAN)`
//! yields `Value::Null`, not an error, so a NaN field converted by serde reaches this module as `null`. `finite_text` refuses a
//! non-finite double with `CanonError::NotFinite`. No `Value` can reach that refusal, so the tests exercise it directly.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};
use sha2::{Digest as _, Sha256};

/// Why a value cannot be canonicalized, or a digest cannot be read. No variant carries the value that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonError {
    /// A number that is not finite. serde_json cannot hold one, so this is a guard for a future value type.
    NotFinite,
    /// A digest that is not 64 lowercase hex characters.
    BadHex,
    /// `params_digest` was given a value that is not a JSON object.
    NotObject,
}

impl fmt::Display for CanonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFinite => "a number that is not finite",
            Self::BadHex => "a digest that is not 64 lowercase hex characters",
            Self::NotObject => "a value that is not a JSON object",
        })
    }
}

impl std::error::Error for CanonError {}

/// A SHA-256 digest, serialized as 64 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// The SHA-256 of `bytes`.
    pub fn of(bytes: &[u8]) -> Digest {
        let out = Sha256::digest(bytes);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&out);
        Digest(arr)
    }

    /// The digest as 64 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Reads exactly 64 lowercase hex characters. Anything else, uppercase included, is `BadHex`.
    pub fn from_hex(s: &str) -> Result<Digest, CanonError> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(CanonError::BadHex);
        }
        let mut out = [0u8; 32];
        for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
            *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(Digest(out))
    }
}

fn nibble(c: u8) -> Result<u8, CanonError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(CanonError::BadHex),
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Digest::from_hex(&text).map_err(serde::de::Error::custom)
    }
}

/// The canonical JSON of `v`: every object's keys sorted by UTF-16 code unit, no whitespace, strings as `JSON.stringify`
/// writes them, numbers as ECMAScript writes them. A non-finite number is `NotFinite`.
pub fn canonical_json(v: &Value) -> Result<String, CanonError> {
    let mut out = String::new();
    write_value(v, &mut out)?;
    Ok(out)
}

/// The SHA-256 (lowercase hex) of the canonical JSON of a call's arguments without their top-level `approval` key. A nested
/// `approval` key is part of the digest. Arguments that are not a JSON object are `NotObject`.
pub fn params_digest(args: &Value) -> Result<String, CanonError> {
    let Value::Object(map) = args else {
        return Err(CanonError::NotObject);
    };
    let mut out = String::new();
    write_object(map, &mut out, Some("approval"))?;
    Ok(Digest::of(out.as_bytes()).to_hex())
}

fn write_value(v: &Value, out: &mut String) -> Result<(), CanonError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&number_text(n)?),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => write_object(map, out, None)?,
    }
    Ok(())
}

/// One object, its keys sorted by UTF-16 code unit. `skip` names a top-level key left out of the output.
fn write_object(
    map: &Map<String, Value>,
    out: &mut String,
    skip: Option<&str>,
) -> Result<(), CanonError> {
    let mut entries: Vec<(&String, &Value)> = map
        .iter()
        .filter(|(key, _)| Some(key.as_str()) != skip)
        .collect();
    entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
    out.push('{');
    for (i, (key, value)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_string(key, out);
        out.push(':');
        write_value(value, out)?;
    }
    out.push('}');
    Ok(())
}

/// A string as `JSON.stringify` writes it: `"` and `\` escaped, the control characters below U+0020 short-escaped where JSON
/// has a short form and `\u00xx` otherwise. Every other character, DEL and U+2028 included, is written as it is.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A number as JavaScript's `JSON.stringify` writes it: the double the number holds.
fn number_text(n: &Number) -> Result<String, CanonError> {
    finite_text(n.as_f64().ok_or(CanonError::NotFinite)?)
}

/// A double as JavaScript's `JSON.stringify` writes it. NaN and the infinities have no JSON text, so they are `NotFinite`.
fn finite_text(x: f64) -> Result<String, CanonError> {
    if x.is_finite() {
        Ok(ecma_number(x))
    } else {
        Err(CanonError::NotFinite)
    }
}

/// ECMAScript's `Number::toString` for a finite double: the shortest digits that read back as the same double, placed the
/// way the specification places them. Both zeros are written `0`.
fn ecma_number(x: f64) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    // An integer below 2^53 has exactly one shortest form, its own digits, so it is written without the search below.
    if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        return format!("{}", x as i64);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let (digits, e) = shortest_digits(x.abs());
    let k = digits.len() as i32;
    // ECMAScript's n: the decimal point sits after n digits of the k significant ones.
    let n = e + 1;
    let body = if (k..=21).contains(&n) {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if (1..=21).contains(&n) {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if (-5..=0).contains(&n) {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let exponent = n - 1;
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{exponent_sign}{}", exponent.abs())
        } else {
            format!(
                "{}.{}e{exponent_sign}{}",
                &digits[..1],
                &digits[1..],
                exponent.abs()
            )
        }
    };
    format!("{sign}{body}")
}

/// The digits and the decimal exponent of a positive finite double, as ECMAScript chooses them: the fewest significant digits
/// that read back as the same double. At that length the two candidates that flank the double are considered. Of those that
/// read back, the closer one is taken; on an exact tie, the one whose last digit is even. A double that is a power of two has
/// a narrower interval below it, so the closer candidate may not read back while the farther one does: both are tried.
fn shortest_digits(x: f64) -> (String, i32) {
    // Rust's shortest output has the fewest digits that read back, which is the length ECMAScript finds too.
    let shortest = format!("{x:e}");
    let (short_mantissa, short_exp) = shortest.split_once('e').unwrap_or((shortest.as_str(), "0"));
    let k = short_mantissa.bytes().filter(u8::is_ascii_digit).count();
    // The exact decimal expansion of x. A double has at most 767 significant digits, so 801 digits are exact.
    let exact = format!("{x:.800e}");
    let (exact_mantissa, exact_exp) = exact.split_once('e').unwrap_or((exact.as_str(), "0"));
    let exact_exp: i32 = exact_exp.parse().unwrap_or(0);
    let all: Vec<u8> = exact_mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let (head, rest) = all.split_at(k.min(all.len()));
    let (upper, carry) = increment(head);
    let lower_ok = reads_back(head, exact_exp, x);
    let upper_ok = reads_back(&upper, exact_exp + carry, x);
    // On an exact tie the even candidate wins, so the upper one wins when the lower digit is odd.
    let last = head.last().copied().unwrap_or(b'0');
    let take_upper = match rest.first().copied() {
        Some(b'5') if rest[1..].iter().all(|d| *d == b'0') => (last - b'0') % 2 == 1,
        Some(d) => d >= b'5',
        None => false,
    };
    let (digits, exp) = match (lower_ok, upper_ok) {
        (true, true) if take_upper => (upper, exact_exp + carry),
        (true, _) => (head.to_vec(), exact_exp),
        (false, true) => (upper, exact_exp + carry),
        (false, false) => (
            short_mantissa.bytes().filter(u8::is_ascii_digit).collect(),
            short_exp.parse().unwrap_or(0),
        ),
    };
    let text = String::from_utf8_lossy(&digits).into_owned();
    let trimmed = text.trim_end_matches('0');
    let kept = if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    };
    (kept, exp)
}

/// The decimal `d0.d1d2… × 10^exp` with `digits` (`d0` first) incremented by one unit in its last place, and 1 when that
/// carries out of the first digit (so the exponent goes up by one).
fn increment(digits: &[u8]) -> (Vec<u8>, i32) {
    let mut out = digits.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] == b'9' {
            out[i] = b'0';
        } else {
            out[i] += 1;
            return (out, 0);
        }
    }
    if let Some(first) = out.first_mut() {
        *first = b'1';
    }
    (out, 1)
}

/// Whether the decimal `d0.d1d2… × 10^exp` parses to exactly `x`.
fn reads_back(digits: &[u8], exp: i32, x: f64) -> bool {
    let mantissa = String::from_utf8_lossy(digits);
    let text = match mantissa.split_at_checked(1) {
        Some((first, "")) => format!("{first}e{exp}"),
        Some((first, more)) => format!("{first}.{more}e{exp}"),
        None => return false,
    };
    text.parse::<f64>().ok() == Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canon(text: &str) -> String {
        let v: Value = serde_json::from_str(text).expect("test input parses");
        canonical_json(&v).expect("test input canonicalizes")
    }

    #[test]
    fn sha256_of_abc_is_the_standard_vector() {
        assert_eq!(
            Digest::of(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn digest_hex_round_trips_and_rejects_anything_else() {
        let d = Digest::of(b"abc");
        assert_eq!(Digest::from_hex(&d.to_hex()), Ok(d));
        assert_eq!(
            Digest::from_hex(&"A".repeat(64)),
            Err(CanonError::BadHex),
            "uppercase is refused"
        );
        assert_eq!(
            Digest::from_hex(&"a".repeat(63)),
            Err(CanonError::BadHex),
            "a short digest is refused"
        );
        assert_eq!(
            Digest::from_hex(&format!("{}g", "a".repeat(63))),
            Err(CanonError::BadHex),
            "a non-hex digit is refused"
        );
    }

    #[test]
    fn digest_serializes_as_lowercase_hex_and_reads_back() {
        let d = Digest::of(b"abc");
        let text = serde_json::to_string(&d).expect("serializes");
        assert_eq!(text, format!("\"{}\"", d.to_hex()));
        let back: Digest = serde_json::from_str(&text).expect("reads back");
        assert_eq!(back, d);
        assert!(serde_json::from_str::<Digest>("\"nothex\"").is_err());
    }

    #[test]
    fn object_keys_are_sorted_recursively_with_no_whitespace() {
        assert_eq!(
            canon(r#"{ "b": 1, "a": { "d": 2, "c": [3, { "y": 4, "x": 5 }] } }"#),
            r#"{"a":{"c":[3,{"x":5,"y":4}],"d":2},"b":1}"#
        );
    }

    #[test]
    fn keys_sort_by_utf16_code_unit_not_by_code_point() {
        // U+E000 sorts after U+10000 in UTF-16 (0xE000 > 0xD800) but before it by code point and by UTF-8 bytes.
        let v = json!({ "\u{E000}": 1, "\u{10000}": 2 });
        assert_eq!(
            canonical_json(&v),
            Ok(format!("{{\"{}\":2,\"{}\":1}}", "\u{10000}", "\u{E000}"))
        );
    }

    #[test]
    fn strings_are_written_as_json_stringify_writes_them() {
        let v = json!("a\"b\\c\n\r\t\u{8}\u{c}\u{1}\u{1f}\u{7f}é☕日本語");
        assert_eq!(
            canonical_json(&v),
            Ok("\"a\\\"b\\\\c\\n\\r\\t\\b\\f\\u0001\\u001f\u{7f}é☕日本語\"".to_string())
        );
    }

    #[test]
    fn numbers_are_written_as_ecmascript_writes_them() {
        let cases = [
            ("1.0", "1"),
            ("-0.0", "0"),
            ("-0", "0"),
            ("100", "100"),
            ("-1.5", "-1.5"),
            ("0.1", "0.1"),
            ("0.000001", "0.000001"),
            ("1e-7", "1e-7"),
            ("1e21", "1e+21"),
            ("123456789012345680000", "123456789012345680000"),
            ("1.2345e25", "1.2345e+25"),
            ("5e-324", "5e-324"),
            ("1.7976931348623157e308", "1.7976931348623157e+308"),
            // A JSON integer above 2^53 is a double in JavaScript: it reads back as 9007199254740992.
            ("9007199254740993", "9007199254740992"),
            // Ties and powers of two: the shortest text is not the one Rust's shortest search finds first.
            ("5.9604644775390625e-8", "5.960464477539063e-8"),
            ("32370644446535.0625", "32370644446535.062"),
            (
                "5.684341886080801486968994140625e-14",
                "5.684341886080802e-14",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(canon(text), want, "number {text}");
        }
    }

    #[test]
    fn a_non_finite_double_has_no_canonical_text() {
        // JSON cannot write NaN or an infinity, so `finite_text` refuses each one instead of writing `null`.
        assert_eq!(finite_text(f64::NAN), Err(CanonError::NotFinite));
        assert_eq!(finite_text(f64::INFINITY), Err(CanonError::NotFinite));
        assert_eq!(finite_text(f64::NEG_INFINITY), Err(CanonError::NotFinite));
        assert_eq!(finite_text(1.5), Ok("1.5".to_string()));
    }

    #[test]
    fn params_digest_of_the_empty_object_is_sha256_of_two_braces() {
        // printf '{}' | sha256sum
        assert_eq!(
            params_digest(&json!({})),
            Ok("44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a".to_string())
        );
    }

    #[test]
    fn params_digest_drops_only_the_top_level_approval_key() {
        let bare = params_digest(&json!({ "plan": "p1" }));
        assert_eq!(
            params_digest(&json!({ "plan": "p1", "approval": "x" })),
            bare
        );
        assert_ne!(
            params_digest(&json!({ "n": { "approval": "x" } })),
            params_digest(&json!({ "n": {} }))
        );
    }

    #[test]
    fn params_digest_refuses_what_is_not_an_object() {
        assert_eq!(params_digest(&json!([1])), Err(CanonError::NotObject));
        assert_eq!(params_digest(&json!("x")), Err(CanonError::NotObject));
        assert_eq!(params_digest(&Value::Null), Err(CanonError::NotObject));
    }
}
