//! JSON export of decoded values.
//!
//! Mapping:
//! - `Struct` -> object: `"_type"` (struct name), then each field keyed by its
//!   name, in schema order. Duplicate keys get a `#2`, `#3` ... suffix.
//! - `Array` -> array; `Vector` -> array of numbers.
//! - `Hash` -> resolved name, or `"0xHASH"` (8 uppercase hex digits).
//! - `Hash64` -> `"0x" + 16 hex digits`.
//! - `Float` -> number using the shortest f32 representation; NaN and
//!   infinities become the strings `"NaN"`, `"inf"`, `"-inf"`.
//! - `Enum` -> the value's name when a schema is given and the name resolves,
//!   `"0xHASH"` of the value name when only the schema knows it, otherwise the
//!   integer.
//! - `Opaque` -> `{"_opaque": "0xTT", "hex": "..."}`.

use serde_json::{Map, Number, Value as Json};
use u_rome_core::Hash32;

use crate::names::NameLookup;
use crate::schema::Schema;
use crate::value::Value;

/// Name of `h`, or `0xHASH`.
pub fn hash_label(h: Hash32, names: &dyn NameLookup) -> String {
    names
        .name(h)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("0x{:08X}", h.0))
}

/// Convert a value to JSON, resolving hashes through `names`.
pub fn to_json(value: &Value, names: &dyn NameLookup) -> Json {
    to_json_with_schema(value, names, None)
}

/// Like [`to_json`], also naming enum values from `schema`.
pub fn to_json_with_schema(value: &Value, names: &dyn NameLookup, schema: Option<&Schema>) -> Json {
    match value {
        Value::Struct { type_hash, fields } => {
            let mut map = Map::with_capacity(fields.len() + 1);
            map.insert("_type".into(), Json::String(hash_label(*type_hash, names)));
            for (h, v) in fields {
                let base = hash_label(*h, names);
                let mut key = base.clone();
                let mut n = 2;
                while map.contains_key(&key) {
                    key = format!("{base}#{n}");
                    n += 1;
                }
                map.insert(key, to_json_with_schema(v, names, schema));
            }
            Json::Object(map)
        }
        Value::Array(items) => Json::Array(
            items
                .iter()
                .map(|v| to_json_with_schema(v, names, schema))
                .collect(),
        ),
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::UInt(u) => Json::from(*u),
        Value::Float(f) => float(*f),
        Value::String(s) => Json::String(s.clone()),
        Value::Hash(h) => Json::String(hash_label(*h, names)),
        Value::Hash64(h) => Json::String(format!("0x{h:016X}")),
        Value::Enum { enum_hash, value } => {
            match schema.and_then(|s| s.enum_value_name(*enum_hash, *value)) {
                Some(n) => Json::String(hash_label(n, names)),
                None => Json::from(*value),
            }
        }
        Value::Vector(v) => Json::Array(v.iter().map(|&f| float(f)).collect()),
        Value::Opaque { type_code, bytes } => {
            let mut map = Map::new();
            map.insert("_opaque".into(), Json::String(format!("0x{type_code:02X}")));
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            map.insert("hex".into(), Json::String(hex));
            Json::Object(map)
        }
    }
}

fn float(f: f32) -> Json {
    if f.is_nan() {
        return Json::String("NaN".into());
    }
    if f.is_infinite() {
        return Json::String(if f > 0.0 { "inf" } else { "-inf" }.into());
    }
    // Round-trip through the shortest f32 text so 0.1f32 prints as 0.1.
    let d: f64 = f.to_string().parse().unwrap_or(f64::from(f));
    Number::from_f64(d).map_or(Json::Null, Json::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::{HashMapNames, NoNames};
    use u_rome_core::joaat;

    #[test]
    fn scalars() {
        let n = NoNames;
        assert_eq!(to_json(&Value::Float(0.1), &n), serde_json::json!(0.1));
        assert_eq!(
            to_json(&Value::Float(f32::NAN), &n),
            serde_json::json!("NaN")
        );
        assert_eq!(
            to_json(&Value::Float(f32::NEG_INFINITY), &n),
            serde_json::json!("-inf")
        );
        assert_eq!(
            to_json(&Value::Hash(Hash32(0xab)), &n),
            serde_json::json!("0x000000AB")
        );
        assert_eq!(
            to_json(&Value::Hash64(1), &n),
            serde_json::json!("0x0000000000000001")
        );
        assert_eq!(
            to_json(
                &Value::Enum {
                    enum_hash: Hash32(1),
                    value: -3
                },
                &n
            ),
            serde_json::json!(-3)
        );
    }

    #[test]
    fn duplicate_keys_are_suffixed() {
        let names: HashMapNames = ["a"].into_iter().collect();
        let v = Value::Struct {
            type_hash: Hash32(7),
            fields: vec![
                (joaat("a"), Value::Bool(true)),
                (joaat("a"), Value::Bool(false)),
            ],
        };
        assert_eq!(
            to_json(&v, &names),
            serde_json::json!({"_type": "0x00000007", "a": true, "a#2": false})
        );
    }
}
