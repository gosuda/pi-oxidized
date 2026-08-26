//! Serde ↔ CborValue adapter (private). Converts between serde's data model
//! and the [`CborValue`] tree so schema types can use `#[derive(Serialize,
//! Deserialize)]` without a third-party CBOR crate.

use serde::ser::{
    SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
    SerializeTuple, SerializeTupleStruct, SerializeTupleVariant,
};
use serde::{Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// CborValue
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum CborValue {
    Null,
    Bool(bool),
    UInt(u64),
    NInt(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<CborValue>),
    Map(Vec<(String, CborValue)>),
}

// ---------------------------------------------------------------------------
// SerError
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SerError(pub String);

impl SerError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self { Self(msg.to_string()) }
}

impl serde::ser::Error for SerError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self { Self(msg.to_string()) }
}
impl serde::de::Error for SerError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self { Self(msg.to_string()) }
}
impl std::fmt::Display for SerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for SerError {}

// ---------------------------------------------------------------------------
// Serializer: typed → CborValue
// ---------------------------------------------------------------------------

pub struct CborValueSerializer;

pub struct SeqSer { pub items: Vec<CborValue> }
pub struct MapSer { pub entries: Vec<(String, CborValue)>, pub next_key: Option<CborValue> }
pub struct StructSer { pub entries: Vec<(String, CborValue)> }
pub struct TupleVarSer { pub variant: String, pub items: Vec<CborValue> }
pub struct StructVarSer { pub variant: String, pub fields: Vec<(String, CborValue)> }

impl Serializer for CborValueSerializer {
    type Ok = CborValue;
    type Error = SerError;
    type SerializeSeq = SeqSer;
    type SerializeTuple = SeqSer;
    type SerializeTupleStruct = SeqSer;
    type SerializeTupleVariant = TupleVarSer;
    type SerializeMap = MapSer;
    type SerializeStruct = StructSer;
    type SerializeStructVariant = StructVarSer;

    fn serialize_bool(self, v: bool) -> Result<CborValue, SerError> { Ok(CborValue::Bool(v)) }
    fn serialize_i8(self, v: i8) -> Result<CborValue, SerError> { self.serialize_i64(i64::from(v)) }
    fn serialize_i16(self, v: i16) -> Result<CborValue, SerError> { self.serialize_i64(i64::from(v)) }
    fn serialize_i32(self, v: i32) -> Result<CborValue, SerError> { self.serialize_i64(i64::from(v)) }
    fn serialize_i64(self, v: i64) -> Result<CborValue, SerError> {
        if v >= 0 { Ok(CborValue::UInt(v as u64)) } else { Ok(CborValue::NInt(v)) }
    }
    fn serialize_u8(self, v: u8) -> Result<CborValue, SerError> { Ok(CborValue::UInt(u64::from(v))) }
    fn serialize_u16(self, v: u16) -> Result<CborValue, SerError> { Ok(CborValue::UInt(u64::from(v))) }
    fn serialize_u32(self, v: u32) -> Result<CborValue, SerError> { Ok(CborValue::UInt(u64::from(v))) }
    fn serialize_u64(self, v: u64) -> Result<CborValue, SerError> { Ok(CborValue::UInt(v)) }
    fn serialize_f32(self, v: f32) -> Result<CborValue, SerError> { self.serialize_f64(f64::from(v)) }
    fn serialize_f64(self, v: f64) -> Result<CborValue, SerError> {
        let neg_zero = v == 0.0 && v.is_sign_negative();
        if v.is_finite() && v.fract() == 0.0 && !neg_zero && v.abs() < 2f64.powi(53) {
            let i = v as i64;
            return Ok(if i >= 0 { CborValue::UInt(i as u64) } else { CborValue::NInt(i) });
        }
        Ok(CborValue::Float(v))
    }
    fn serialize_char(self, v: char) -> Result<CborValue, SerError> { Ok(CborValue::Text(v.to_string())) }
    fn serialize_str(self, v: &str) -> Result<CborValue, SerError> { Ok(CborValue::Text(v.to_string())) }
    fn serialize_bytes(self, v: &[u8]) -> Result<CborValue, SerError> { Ok(CborValue::Bytes(v.to_vec())) }
    fn serialize_none(self) -> Result<CborValue, SerError> { Ok(CborValue::Null) }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<CborValue, SerError> { value.serialize(self) }
    fn serialize_unit(self) -> Result<CborValue, SerError> { Ok(CborValue::Null) }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<CborValue, SerError> { Ok(CborValue::Null) }
    fn serialize_unit_variant(self, _name: &'static str, _i: u32, variant: &'static str) -> Result<CborValue, SerError> { Ok(CborValue::Text(variant.to_string())) }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _name: &'static str, value: &T) -> Result<CborValue, SerError> { value.serialize(self) }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(self, _name: &'static str, _i: u32, variant: &'static str, value: &T) -> Result<CborValue, SerError> {
        let inner = value.serialize(CborValueSerializer)?;
        Ok(CborValue::Map(vec![(variant.to_string(), inner)]))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqSer, SerError> {
        Ok(SeqSer { items: Vec::with_capacity(len.unwrap_or(0)) })
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqSer, SerError> { self.serialize_seq(Some(len)) }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SeqSer, SerError> { self.serialize_seq(Some(len)) }
    fn serialize_tuple_variant(self, _name: &'static str, _i: u32, variant: &'static str, len: usize) -> Result<TupleVarSer, SerError> {
        Ok(TupleVarSer { variant: variant.to_string(), items: Vec::with_capacity(len) })
    }
    fn serialize_map(self, len: Option<usize>) -> Result<MapSer, SerError> {
        Ok(MapSer { entries: Vec::with_capacity(len.unwrap_or(0)), next_key: None })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<StructSer, SerError> {
        Ok(StructSer { entries: Vec::with_capacity(len) })
    }
    fn serialize_struct_variant(self, _name: &'static str, _i: u32, variant: &'static str, len: usize) -> Result<StructVarSer, SerError> {
        Ok(StructVarSer { variant: variant.to_string(), fields: Vec::with_capacity(len) })
    }
}

impl SerializeSeq for SeqSer {
    type Ok = CborValue; type Error = SerError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
        self.items.push(value.serialize(CborValueSerializer)?); Ok(())
    }
    fn end(self) -> Result<CborValue, SerError> { Ok(CborValue::Array(self.items)) }
}

impl SerializeTuple for SeqSer {
    type Ok = CborValue; type Error = SerError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
        self.items.push(value.serialize(CborValueSerializer)?); Ok(())
    }
    fn end(self) -> Result<CborValue, SerError> { Ok(CborValue::Array(self.items)) }
}

impl SerializeTupleStruct for SeqSer {
    type Ok = CborValue; type Error = SerError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
        self.items.push(value.serialize(CborValueSerializer)?); Ok(())
    }
    fn end(self) -> Result<CborValue, SerError> { Ok(CborValue::Array(self.items)) }
}

impl SerializeTupleVariant for TupleVarSer {
    type Ok = CborValue; type Error = SerError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
        self.items.push(value.serialize(CborValueSerializer)?); Ok(())
    }
    fn end(self) -> Result<CborValue, SerError> { Ok(CborValue::Map(vec![(self.variant, CborValue::Array(self.items))])) }
}

impl SerializeMap for MapSer {
    type Ok = CborValue; type Error = SerError;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), SerError> {
        self.next_key = Some(key.serialize(CborValueSerializer)?); Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerError> {
        let k = self.next_key.take().ok_or_else(|| SerError::custom("value without key"))?;
        let v = value.serialize(CborValueSerializer)?;
        let ks = match k { CborValue::Text(s) => s, _ => return Err(SerError::custom("map keys must be strings")) };
        self.entries.push((ks, v)); Ok(())
    }
    fn end(self) -> Result<CborValue, SerError> { Ok(CborValue::Map(self.entries)) }
}

impl SerializeStruct for StructSer {
    type Ok = CborValue; type Error = SerError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), SerError> {
        self.entries.push((key.to_string(), value.serialize(CborValueSerializer)?)); Ok(())
    }
    fn end(self) -> Result<CborValue, SerError> { Ok(CborValue::Map(self.entries)) }
}

impl SerializeStructVariant for StructVarSer {
    type Ok = CborValue; type Error = SerError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), SerError> {
        self.fields.push((key.to_string(), value.serialize(CborValueSerializer)?)); Ok(())
    }
    fn end(self) -> Result<CborValue, SerError> { Ok(CborValue::Map(vec![(self.variant, CborValue::Map(self.fields))])) }
}

// ---------------------------------------------------------------------------
// Deserializer: CborValue → typed
// ---------------------------------------------------------------------------

pub struct CborValueDeserializer { pub value: CborValue }

struct SeqAccess { iter: std::vec::IntoIter<CborValue> }
struct MapAccess { iter: std::vec::IntoIter<(String, CborValue)>, next_value: Option<CborValue> }
struct EnumAccess { variant: String, value: CborValue }

impl<'de> serde::de::SeqAccess<'de> for SeqAccess {
    type Error = SerError;
    fn next_element_seed<T: serde::de::DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, SerError> {
        match self.iter.next() {
            Some(v) => Ok(Some(seed.deserialize(CborValueDeserializer { value: v })?)),
            None => Ok(None),
        }
    }
    fn size_hint(&self) -> Option<usize> { Some(self.iter.len()) }
}

impl<'de> serde::de::MapAccess<'de> for MapAccess {
    type Error = SerError;
    fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, SerError> {
        match self.iter.next() {
            Some((k, v)) => { self.next_value = Some(v); Ok(Some(seed.deserialize(CborValueDeserializer { value: CborValue::Text(k) })?)) }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, SerError> {
        let v = self.next_value.take().ok_or_else(|| SerError::custom("value without key"))?;
        seed.deserialize(CborValueDeserializer { value: v })
    }
}

impl<'de> serde::de::EnumAccess<'de> for EnumAccess {
    type Error = SerError;
    type Variant = CborValueDeserializer;
    fn variant_seed<T: serde::de::DeserializeSeed<'de>>(self, seed: T) -> Result<(T::Value, Self::Variant), SerError> {
        let de = serde::de::value::StrDeserializer::new(&self.variant);
        Ok((seed.deserialize(de)?, CborValueDeserializer { value: self.value }))
    }
}

impl<'de> Deserializer<'de> for CborValueDeserializer {
    type Error = SerError;

    fn deserialize_any<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerError> {
        match self.value {
            CborValue::Null => visitor.visit_unit(),
            CborValue::Bool(b) => visitor.visit_bool(b),
            CborValue::UInt(n) => if n <= i64::MAX as u64 { visitor.visit_i64(n as i64) } else { visitor.visit_u64(n) },
            CborValue::NInt(n) => visitor.visit_i64(n),
            CborValue::Float(f) => visitor.visit_f64(f),
            CborValue::Text(s) => visitor.visit_string(s),
            CborValue::Bytes(b) => visitor.visit_byte_buf(b),
            CborValue::Array(a) => { let mut sa = SeqAccess { iter: a.into_iter() }; visitor.visit_seq(&mut sa) }
            CborValue::Map(e) => { let mut ma = MapAccess { iter: e.into_iter(), next_value: None }; visitor.visit_map(&mut ma) }
        }
    }

    fn deserialize_bool<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Bool(b) => v.visit_bool(b), _ => Err(SerError::custom("expected bool")) }
    }
    fn deserialize_i8<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_i64(v) }
    fn deserialize_i16<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_i64(v) }
    fn deserialize_i32<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_i64(v) }
    fn deserialize_i64<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::UInt(n) if n <= i64::MAX as u64 => v.visit_i64(n as i64), CborValue::NInt(n) => v.visit_i64(n), _ => Err(SerError::custom("expected i64")) }
    }
    fn deserialize_u8<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_u64(v) }
    fn deserialize_u16<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_u64(v) }
    fn deserialize_u32<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_u64(v) }
    fn deserialize_u64<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::UInt(n) => v.visit_u64(n), _ => Err(SerError::custom("expected u64")) }
    }
    fn deserialize_f32<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_f64(v) }
    fn deserialize_f64<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Float(f) => v.visit_f64(f), CborValue::UInt(n) => v.visit_f64(n as f64), CborValue::NInt(n) => v.visit_f64(n as f64), _ => Err(SerError::custom("expected f64")) }
    }
    fn deserialize_char<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_str(v) }
    fn deserialize_str<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_string(v) }
    fn deserialize_string<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Text(s) => v.visit_string(s), _ => Err(SerError::custom("expected string")) }
    }
    fn deserialize_bytes<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_byte_buf(v) }
    fn deserialize_byte_buf<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Bytes(b) => v.visit_byte_buf(b), _ => Err(SerError::custom("expected bytes")) }
    }
    fn deserialize_option<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Null => v.visit_none(), _ => v.visit_some(self) }
    }
    fn deserialize_unit<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Null => v.visit_unit(), _ => Err(SerError::custom("expected unit")) }
    }
    fn deserialize_unit_struct<V: serde::de::Visitor<'de>>(self, _: &'static str, v: V) -> Result<V::Value, SerError> { self.deserialize_unit(v) }
    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(self, _: &'static str, v: V) -> Result<V::Value, SerError> { v.visit_newtype_struct(self) }
    fn deserialize_seq<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Array(a) => { let mut sa = SeqAccess { iter: a.into_iter() }; v.visit_seq(&mut sa) }, _ => Err(SerError::custom("expected array")) }
    }
    fn deserialize_tuple<V: serde::de::Visitor<'de>>(self, _: usize, v: V) -> Result<V::Value, SerError> { self.deserialize_seq(v) }
    fn deserialize_tuple_struct<V: serde::de::Visitor<'de>>(self, _: &'static str, _: usize, v: V) -> Result<V::Value, SerError> { self.deserialize_seq(v) }
    fn deserialize_map<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> {
        match self.value { CborValue::Map(e) => { let mut ma = MapAccess { iter: e.into_iter(), next_value: None }; v.visit_map(&mut ma) }, _ => Err(SerError::custom("expected map")) }
    }
    fn deserialize_struct<V: serde::de::Visitor<'de>>(self, _: &'static str, _: &'static [&'static str], v: V) -> Result<V::Value, SerError> { self.deserialize_map(v) }
    fn deserialize_enum<V: serde::de::Visitor<'de>>(self, _: &'static str, _: &'static [&'static str], v: V) -> Result<V::Value, SerError> {
        match self.value {
            CborValue::Text(s) => v.visit_enum(serde::de::value::StrDeserializer::new(&s)),
            CborValue::Map(e) => {
                if e.len() == 1 { let (var, val) = e.into_iter().next().ok_or_else(|| SerError::custom("empty"))?; v.visit_enum(EnumAccess { variant: var, value: val }) }
                else { Err(SerError::custom("externally tagged enum must have one entry")) }
            }
            _ => Err(SerError::custom("expected enum")),
        }
    }
    fn deserialize_identifier<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_str(v) }
    fn deserialize_ignored_any<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, SerError> { self.deserialize_any(v) }
}

impl<'de> serde::de::VariantAccess<'de> for CborValueDeserializer {
    type Error = SerError;
    fn unit_variant(self) -> Result<(), SerError> { match self.value { CborValue::Null => Ok(()), _ => Err(SerError::custom("expected unit variant")) } }
    fn newtype_variant_seed<T: serde::de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, SerError> { seed.deserialize(self) }
    fn tuple_variant<V: serde::de::Visitor<'de>>(self, _: usize, v: V) -> Result<V::Value, SerError> { self.deserialize_seq(v) }
    fn struct_variant<V: serde::de::Visitor<'de>>(self, _: &'static [&'static str], v: V) -> Result<V::Value, SerError> { self.deserialize_map(v) }
}
