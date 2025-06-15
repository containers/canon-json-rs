// SPDX-License-Identifier: MIT OR Apache-2.0

#![doc = include_str!("../README.md")]

#![forbid(unsafe_code)]

mod floatformat;

use std::collections::BTreeMap;
use std::io::{Error, ErrorKind, Result, Write};

use serde::Serialize;
use serde_json::ser::{CharEscape, CompactFormatter, Formatter, Serializer};

/// A [`Formatter`] that produces canonical JSON.
///
/// See the [crate-level documentation](../index.html) for more detail.
///
/// [`Formatter`]: ../serde_json/ser/trait.Formatter.html
#[derive(Debug, Default)]
pub struct CanonicalFormatter {
    object_stack: Vec<Object>,
}

/// https://www.rfc-editor.org/rfc/rfc8785#name-sorting-of-object-properties
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ObjectKey(Vec<u16>);

impl ObjectKey {
    fn new_from_str(s: &str) -> Self {
        Self(s.encode_utf16().collect())
    }

    fn new_from_bytes(v: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(v)
            .map_err(|e| Error::new(ErrorKind::InvalidData, format!("Expected UTF-8 key: {e}")))?;
        Ok(Self::new_from_str(s))
    }

    fn as_string(&self) -> Result<String> {
        std::char::decode_utf16(self.0.iter().copied()).try_fold(String::new(), |mut acc, c| {
            let c = c.map_err(|_| Error::new(ErrorKind::InvalidData, "Expected UTF-8 key"))?;
            acc.push(c);
            Ok(acc)
        })
    }

    // Serialize this value as a JSON string
    fn write_to<W: Write>(&self, w: W) -> Result<()> {
        let s = self.as_string()?;
        let val = serde_json::Value::String(s);
        let mut s = Serializer::new(w);
        val.serialize(&mut s).map_err(|e| {
            let kind = e.io_error_kind().unwrap();
            Error::new(kind, "I/O error")
        })
    }
}

/// Internal struct to keep track of an object in progress of being built.
///
/// As keys and values are received by `CanonicalFormatter`, they are written to `next_key` and
/// `next_value` by using the `CanonicalFormatter::writer` convenience method.
///
/// How this struct behaves when `Formatter` methods are called:
///
/// ```plain
/// [other methods]  // values written to the writer received by method
/// begin_object     // create this object
/// /-> begin_object_key    // object.key_done = false;
/// |   [other methods]     // values written to object.next_key, writer received by method ignored
/// |   end_object_key      // object.key_done = true;
/// |   begin_object_value  // [nothing]
/// |   [other methods]     // values written to object.next_value
/// |   end_object_value    // object.next_key and object.next_value are inserted into object.obj
/// \---- // jump back if more values are present
/// end_object       // write the object (sorted by its keys) to the writer received by the method
/// ```
#[derive(Debug, Default)]
struct Object {
    obj: BTreeMap<ObjectKey, Vec<u8>>,
    next_key: Vec<u8>,
    next_value: Vec<u8>,
    key_done: bool,
}

impl CanonicalFormatter {
    /// Create a new `CanonicalFormatter` object.
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience method to return the appropriate writer given the current context.
    ///
    /// If we are currently writing an object (that is, if `!self.object_stack.is_empty()`), we
    /// need to write the value to either the next key or next value depending on that state
    /// machine. See the docstrings for `Object` for more detail.
    ///
    /// If we are not currently writing an object, pass through `writer`.
    fn writer<'a, W: Write + ?Sized>(&'a mut self, writer: &'a mut W) -> Box<dyn Write + 'a> {
        self.writer_or_key(writer, false).0
    }

    /// For string writes, we may be writing into the key. If so, then handle
    /// that specially.
    fn writer_or_key<'a, W: Write + ?Sized>(
        &'a mut self,
        writer: &'a mut W,
        object_key_allowed: bool,
    ) -> (Box<dyn Write + 'a>, bool) {
        self.object_stack
            .last_mut()
            .map_or((Box::new(writer), false), |object| {
                let r = if object.key_done {
                    Box::new(&mut object.next_value)
                } else if !object_key_allowed {
                    panic!("Unhandled write into object key");
                } else {
                    Box::new(&mut object.next_key)
                };
                (r, !object.key_done)
            })
    }

    /// Returns a mutable reference to the top of the object stack.
    fn obj_mut(&mut self) -> Result<&mut Object> {
        self.object_stack.last_mut().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                "serde_json called an object method without calling begin_object first",
            )
        })
    }
}

/// Wraps `serde_json::CompactFormatter` to use the appropriate writer (see
/// `CanonicalFormatter::writer`).
macro_rules! wrapper {
    ($f:ident) => {
        fn $f<W: Write + ?Sized>(&mut self, writer: &mut W) -> Result<()> {
            CompactFormatter.$f(&mut self.writer(writer))
        }
    };

    ($f:ident, $t:ty) => {
        fn $f<W: Write + ?Sized>(&mut self, writer: &mut W, arg: $t) -> Result<()> {
            CompactFormatter.$f(&mut self.writer(writer), arg)
        }
    };
}

impl Formatter for CanonicalFormatter {
    wrapper!(write_null);
    wrapper!(write_bool, bool);
    wrapper!(write_i8, i8);
    wrapper!(write_i16, i16);
    wrapper!(write_i32, i32);
    wrapper!(write_i64, i64);
    wrapper!(write_i128, i128);
    wrapper!(write_u8, u8);
    wrapper!(write_u16, u16);
    wrapper!(write_u32, u32);
    wrapper!(write_u64, u64);
    wrapper!(write_u128, u128);

    fn write_f32<W: Write + ?Sized>(&mut self, writer: &mut W, value: f32) -> Result<()> {
        self.write_f64(writer, value.into())
    }

    fn write_f64<W: Write + ?Sized>(&mut self, writer: &mut W, value: f64) -> Result<()> {
        let v = floatformat::number_to_json(value).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Unhandled floating point value {e}"),
            )
        })?;
        CompactFormatter.write_string_fragment(&mut self.writer(writer), &v)
    }

    // By default this is only used for u128/i128. If serde_json's `arbitrary_precision` feature is
    // enabled, all numbers are internally stored as strings, and this method is always used (even
    // for floating point values).
    fn write_number_str<W: Write + ?Sized>(&mut self, writer: &mut W, value: &str) -> Result<()> {
        CompactFormatter.write_number_str(&mut self.writer(writer), value)
    }

    fn begin_string<W: Write + ?Sized>(&mut self, writer: &mut W) -> Result<()> {
        let Some(v) = self.object_stack.last_mut() else {
            return CompactFormatter.begin_string(writer);
        };
        if !v.key_done {
            return Ok(());
        }
        return CompactFormatter.begin_string(&mut v.next_value);
    }

    fn end_string<W: Write + ?Sized>(&mut self, writer: &mut W) -> Result<()> {
        let Some(v) = self.object_stack.last_mut() else {
            return CompactFormatter.begin_string(writer);
        };
        if !v.key_done {
            return Ok(());
        }
        return CompactFormatter.begin_string(&mut v.next_value);
    }

    fn write_string_fragment<W: Write + ?Sized>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> Result<()> {
        let (mut writer, in_key) = self.writer_or_key(writer, true);
        if in_key {
            writer.write_all(fragment.as_bytes())
        } else {
            CompactFormatter.write_string_fragment(&mut writer, fragment)
        }
    }

    fn write_char_escape<W: Write + ?Sized>(
        &mut self,
        writer: &mut W,
        char_escape: CharEscape,
    ) -> Result<()> {
        let (mut writer, in_key) = self.writer_or_key(writer, true);
        if in_key {
            let v = match char_escape {
                CharEscape::Quote => b"\"",
                CharEscape::ReverseSolidus => b"\\",
                CharEscape::Solidus => b"/",
                CharEscape::Backspace => b"\x08",
                CharEscape::FormFeed => b"\x0C",
                CharEscape::LineFeed => b"\n",
                CharEscape::CarriageReturn => b"\r",
                CharEscape::Tab => b"\t",
                CharEscape::AsciiControl(c) => &[c],
            };
            writer.write_all(v)
        } else {
            return CompactFormatter.write_char_escape(&mut writer, char_escape);
        }
    }

    wrapper!(begin_array);
    wrapper!(end_array);
    wrapper!(begin_array_value, bool); // hack: this passes through the `first` argument
    wrapper!(end_array_value);

    // Here are the object methods. Because keys must be sorted, we serialize the object's keys and
    // values in memory as a `BTreeMap`, then write it all out when `end_object_value` is called.

    fn begin_object<W: Write + ?Sized>(&mut self, writer: &mut W) -> Result<()> {
        CompactFormatter.begin_object(&mut self.writer(writer))?;
        self.object_stack.push(Object::default());
        Ok(())
    }

    fn end_object<W: Write + ?Sized>(&mut self, writer: &mut W) -> Result<()> {
        let object = self.object_stack.pop().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                "serde_json called Formatter::end_object object method
                 without calling begin_object first",
            )
        })?;
        let mut writer = self.writer(writer);
        let mut first = true;

        for (key, value) in object.obj {
            CompactFormatter.begin_object_key(&mut writer, first)?;
            key.write_to(&mut writer)?;
            CompactFormatter.end_object_key(&mut writer)?;

            CompactFormatter.begin_object_value(&mut writer)?;
            writer.write_all(&value)?;
            CompactFormatter.end_object_value(&mut writer)?;

            first = false;
        }

        CompactFormatter.end_object(&mut writer)
    }

    fn begin_object_key<W: Write + ?Sized>(&mut self, _writer: &mut W, _first: bool) -> Result<()> {
        let object = self.obj_mut()?;
        object.key_done = false;
        Ok(())
    }

    fn end_object_key<W: Write + ?Sized>(&mut self, _writer: &mut W) -> Result<()> {
        let object = self.obj_mut()?;
        object.key_done = true;
        Ok(())
    }

    fn begin_object_value<W: Write + ?Sized>(&mut self, _writer: &mut W) -> Result<()> {
        Ok(())
    }

    fn end_object_value<W: Write + ?Sized>(&mut self, _writer: &mut W) -> Result<()> {
        let object = self.obj_mut()?;
        let key = std::mem::take(&mut object.next_key);
        let value = std::mem::take(&mut object.next_value);
        // Canonialize as UTF-16
        object.obj.insert(ObjectKey::new_from_bytes(&key)?, value);
        Ok(())
    }

    // This is for serde_json's `raw_value` feature, which provides a RawValue type that is passed
    // through as-is. That's not good enough for canonical JSON, so we parse it and immediately
    // write it back out... as canonical JSON.
    fn write_raw_fragment<W: Write + ?Sized>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> Result<()> {
        let mut ser = Serializer::with_formatter(self.writer(writer), Self::new());
        serde_json::from_str::<serde_json::Value>(fragment)?.serialize(&mut ser)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{cmp::Ordering, io::Result};

    use proptest::prelude::*;
    use serde::Serialize;
    use serde_json::{Number, Serializer};
    use similar_asserts::assert_eq;

    #[test]
    fn test_object_key() {
        let cases = [("\n", "1"), ("\r", "<script>"), ("ö", "דּ")];
        for case in cases {
            assert_eq!(case.0.cmp(&case.1), Ordering::Less);
        }
        let mut v = cases
            .iter()
            .flat_map(|v| [v.0, v.1])
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter();
        assert_eq!(v.next().unwrap(), "\n");
        assert_eq!(v.next().unwrap(), "\r");
        assert_eq!(v.next().unwrap(), "1");
        assert_eq!(v.next().unwrap(), "<script>");
        assert_eq!(v.next().unwrap(), "ö");
        assert_eq!(v.next().unwrap(), "דּ");

        let mut buf = Vec::new();
        ObjectKey::new_from_str("").write_to(&mut buf).unwrap();
        assert_eq!(&buf, b"\"\"");
    }

    /// Small wrapper around the `serde_json` json! macro to encode the value as canonical JSON.
    macro_rules! encode {
        ($($tt:tt)+) => {
            (|v: serde_json::Value| -> Result<Vec<u8>> {
                let mut buf = Vec::new();
                let mut ser = Serializer::with_formatter(&mut buf, CanonicalFormatter::new());
                v.serialize(&mut ser)?;
                Ok(buf)
            })(serde_json::json!($($tt)+))
        };
    }

    /// These smoke tests come from securesystemslib, the library used by the TUF reference
    /// implementation.
    ///
    /// `<https://github.com/secure-systems-lab/securesystemslib/blob/f466266014aff529510216b8c2f8c8f39de279ec/tests/test_formats.py#L354-L389>`
    #[test]
    fn securesystemslib_asserts() -> Result<()> {
        assert_eq!(encode!([1, 2, 3])?, b"[1,2,3]");
        assert_eq!(encode!([1, 2, 3])?, b"[1,2,3]");
        assert_eq!(encode!([])?, b"[]");
        assert_eq!(encode!({})?, b"{}");
        assert_eq!(encode!({"A": [99]})?, br#"{"A":[99]}"#);
        assert_eq!(encode!({"A": true})?, br#"{"A":true}"#);
        assert_eq!(encode!({"B": false})?, br#"{"B":false}"#);
        assert_eq!(encode!({"x": 3, "y": 2})?, br#"{"x":3,"y":2}"#);
        assert_eq!(encode!({"x": 3, "y": null})?, br#"{"x":3,"y":null}"#);

        Ok(())
    }

    /// A more involved test than any of the above for our core competency: ordering things.
    #[test]
    fn ordered_nested_object() -> Result<()> {
        assert_eq!(
            encode!({
                "nested": {
                    "bad": true,
                    "good": false
                },
                "b": 2,
                "a": 1,
                "c": {
                    "h": {
                        "h": -5,
                        "i": 3
                    },
                    "a": null,
                    "x": {}
                }
            })?,
            br#"{"a":1,"b":2,"c":{"a":null,"h":{"h":-5,"i":3},"x":{}},"nested":{"bad":true,"good":false}}"#.to_vec(),
        );

        Ok(())
    }

    /// This test asserts that the canonical representation of some real-world data always comes
    /// out the same.
    #[allow(clippy::unreadable_literal)]
    #[test]
    fn actual_tuf_signed() {
        let encode_result = encode!(
        {
          "signed": {
            "_type": "timestamp",
            "spec_version": "1.0.0",
            "version": 1604605512,
            "expires": "2020-11-12T19:45:12.613154979Z",
            "meta": {
              "snapshot.json": {
                "length": 1278,
                "hashes": {
                  "sha256": "56c4ecc3b331f6154d9a5005f6e2978e4198cc8c3b79746c25a592043a2d83d4"
                },
                "version": 1604605512
              }
            }
          }
        }
        );

        let encoded = encode_result.unwrap();
        let expected: Vec<u8> = vec![
            123, 34, 115, 105, 103, 110, 101, 100, 34, 58, 123, 34, 95, 116, 121, 112, 101, 34, 58,
            34, 116, 105, 109, 101, 115, 116, 97, 109, 112, 34, 44, 34, 101, 120, 112, 105, 114,
            101, 115, 34, 58, 34, 50, 48, 50, 48, 45, 49, 49, 45, 49, 50, 84, 49, 57, 58, 52, 53,
            58, 49, 50, 46, 54, 49, 51, 49, 53, 52, 57, 55, 57, 90, 34, 44, 34, 109, 101, 116, 97,
            34, 58, 123, 34, 115, 110, 97, 112, 115, 104, 111, 116, 46, 106, 115, 111, 110, 34, 58,
            123, 34, 104, 97, 115, 104, 101, 115, 34, 58, 123, 34, 115, 104, 97, 50, 53, 54, 34,
            58, 34, 53, 54, 99, 52, 101, 99, 99, 51, 98, 51, 51, 49, 102, 54, 49, 53, 52, 100, 57,
            97, 53, 48, 48, 53, 102, 54, 101, 50, 57, 55, 56, 101, 52, 49, 57, 56, 99, 99, 56, 99,
            51, 98, 55, 57, 55, 52, 54, 99, 50, 53, 97, 53, 57, 50, 48, 52, 51, 97, 50, 100, 56,
            51, 100, 52, 34, 125, 44, 34, 108, 101, 110, 103, 116, 104, 34, 58, 49, 50, 55, 56, 44,
            34, 118, 101, 114, 115, 105, 111, 110, 34, 58, 49, 54, 48, 52, 54, 48, 53, 53, 49, 50,
            125, 125, 44, 34, 115, 112, 101, 99, 95, 118, 101, 114, 115, 105, 111, 110, 34, 58, 34,
            49, 46, 48, 46, 48, 34, 44, 34, 118, 101, 114, 115, 105, 111, 110, 34, 58, 49, 54, 48,
            52, 54, 48, 53, 53, 49, 50, 125, 125,
        ];
        assert_eq!(expected, encoded);
    }

    #[test]
    fn encode_u128_i128() {
        #[derive(serde_derive::Serialize)]
        struct Object {
            u128: u128,
            i128: i128,
        }

        let value = Object {
            u128: u128::MAX,
            i128: i128::MIN,
        };

        let mut buf = Vec::new();
        let mut ser = Serializer::with_formatter(&mut buf, CanonicalFormatter::new());
        value.serialize(&mut ser).unwrap();

        let expected = [
            123, 34, 105, 49, 50, 56, 34, 58, 45, 49, 55, 48, 49, 52, 49, 49, 56, 51, 52, 54, 48,
            52, 54, 57, 50, 51, 49, 55, 51, 49, 54, 56, 55, 51, 48, 51, 55, 49, 53, 56, 56, 52, 49,
            48, 53, 55, 50, 56, 44, 34, 117, 49, 50, 56, 34, 58, 51, 52, 48, 50, 56, 50, 51, 54,
            54, 57, 50, 48, 57, 51, 56, 52, 54, 51, 52, 54, 51, 51, 55, 52, 54, 48, 55, 52, 51, 49,
            55, 54, 56, 50, 49, 49, 52, 53, 53, 125,
        ];

        assert_eq!(buf, expected);
    }

    #[test]
    fn test_basic() {
        let v = serde_json::json! { { "foo": "42" } };
        let expected = serde_json::to_string(&v).unwrap();
        let buf = String::from_utf8(encode!(v).unwrap()).unwrap();
        assert_eq!(&buf, &expected);
    }

    fn arbitrary_json() -> impl Strategy<Value = serde_json::Value> {
        use serde_json::Value;
        const S: &str = ".*";
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<u32>().prop_map(|v| Value::Number(Number::from_u128(v.into()).unwrap())),
            any::<bool>().prop_map(Value::Bool),
            S.prop_map(Value::String),
        ];
        leaf.prop_recursive(
            8,   // 8 levels deep
            256, // Shoot for maximum size of 256 nodes
            10,  // We put up to 10 items per collection
            |inner| {
                prop_oneof![
                    // Take the inner strategy and make the two recursive cases.
                    prop::collection::vec(inner.clone(), 0..10).prop_map(Value::Array),
                    prop::collection::hash_map(S, inner, 0..10).prop_map(|v| {
                        v.into_iter().map(|(k, v)| (k, Value::from(v))).collect()
                    }),
                ]
            },
        )
    }

    proptest! {
        #[test]
        fn roundtrip_rfc8785(v in arbitrary_json()) {
            let v: serde_json::Value = v.into();
            let buf = encode!(&v).unwrap();
            let v2: serde_json::Value = serde_json::from_slice(&buf)
                .map_err(|e| format!("Failed to parse {v:?} -> {}: {e}", String::from_utf8_lossy(&buf))).unwrap();
            assert_eq!(&v, &v2);
        }
    }

    fn verify(input: &str, expected: &str) {
        let input: serde_json::Value = serde_json::from_str(input).unwrap();
        let mut buf = Vec::new();
        let mut ser = Serializer::with_formatter(&mut buf, CanonicalFormatter::new());
        input.serialize(&mut ser).unwrap();
        let buf = String::from_utf8(buf).unwrap();
        assert_eq!(expected, &buf);
    }

    #[test]
    fn test_arrays() {
        verify(
            include_str!("../testdata/input/arrays.json"),
            include_str!("../testdata/output/arrays.json"),
        );
    }

    #[test]
    fn test_french() {
        verify(
            include_str!("../testdata/input/french.json"),
            include_str!("../testdata/output/french.json"),
        );
    }

    #[test]
    fn test_structures() {
        verify(
            include_str!("../testdata/input/structures.json"),
            include_str!("../testdata/output/structures.json"),
        );
    }

    #[test]
    fn test_unicode() {
        verify(
            include_str!("../testdata/input/unicode.json"),
            include_str!("../testdata/output/unicode.json"),
        );
    }

    #[test]
    fn test_values() {
        verify(
            include_str!("../testdata/input/values.json"),
            include_str!("../testdata/output/values.json"),
        );
    }

    #[test]
    fn test_weird() {
        verify(
            include_str!("../testdata/input/weird.json"),
            include_str!("../testdata/output/weird.json"),
        );
    }
}
