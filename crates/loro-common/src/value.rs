use std::{collections::HashMap, hash::Hash, ops::Index, sync::Arc};

use arbitrary::Arbitrary;
use enum_as_inner::EnumAsInner;
use fxhash::FxHashMap;
use serde::{de::VariantAccess, Deserialize, Serialize};

use crate::ContainerID;

/// [LoroValue] is used to represents the state of CRDT at a given version.
///
/// This struct is cheap to clone, the time complexity is O(1).
#[derive(Debug, PartialEq, Clone, EnumAsInner, Default)]
pub enum LoroValue {
    #[default]
    Null,
    Bool(bool),
    Double(f64),
    I64(i64),
    // i64?
    Binary(LoroBinaryValue),
    String(LoroStringValue),
    List(LoroListValue),
    // PERF We can use InternalString as key
    Map(LoroMapValue),
    Container(ContainerID),
}

#[derive(Default, Debug, PartialEq, Clone, Arbitrary)]
pub struct LoroBinaryValue(Arc<Vec<u8>>);
#[derive(Default, Debug, PartialEq, Clone, Arbitrary)]
pub struct LoroStringValue(Arc<String>);
#[derive(Default, Debug, PartialEq, Clone, Arbitrary)]
pub struct LoroListValue(Arc<Vec<LoroValue>>);
#[derive(Default, Debug, PartialEq, Clone, Arbitrary)]
pub struct LoroMapValue(Arc<FxHashMap<String, LoroValue>>);

impl From<Vec<u8>> for LoroBinaryValue {
    fn from(value: Vec<u8>) -> Self {
        Self(Arc::new(value))
    }
}

impl From<String> for LoroStringValue {
    fn from(value: String) -> Self {
        Self(Arc::new(value))
    }
}

impl From<&str> for LoroStringValue {
    fn from(value: &str) -> Self {
        Self(Arc::new(value.to_string()))
    }
}

impl From<Vec<LoroValue>> for LoroListValue {
    fn from(value: Vec<LoroValue>) -> Self {
        Self(Arc::new(value))
    }
}

impl From<FxHashMap<String, LoroValue>> for LoroMapValue {
    fn from(value: FxHashMap<String, LoroValue>) -> Self {
        Self(Arc::new(value))
    }
}

impl From<HashMap<String, LoroValue>> for LoroMapValue {
    fn from(value: HashMap<String, LoroValue>) -> Self {
        Self(Arc::new(FxHashMap::from_iter(value)))
    }
}

impl From<Vec<(String, LoroValue)>> for LoroMapValue {
    fn from(value: Vec<(String, LoroValue)>) -> Self {
        Self(Arc::new(FxHashMap::from_iter(value)))
    }
}

impl std::ops::Deref for LoroBinaryValue {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::Deref for LoroStringValue {
    type Target = String;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::Deref for LoroListValue {
    type Target = Vec<LoroValue>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::Deref for LoroMapValue {
    type Target = FxHashMap<String, LoroValue>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl LoroBinaryValue {
    pub fn make_mut(&mut self) -> &mut Vec<u8> {
        Arc::make_mut(&mut self.0)
    }
}

impl LoroStringValue {
    pub fn make_mut(&mut self) -> &mut String {
        Arc::make_mut(&mut self.0)
    }
}

impl LoroListValue {
    pub fn make_mut(&mut self) -> &mut Vec<LoroValue> {
        Arc::make_mut(&mut self.0)
    }
}

impl LoroMapValue {
    pub fn make_mut(&mut self) -> &mut FxHashMap<String, LoroValue> {
        Arc::make_mut(&mut self.0)
    }
}

impl LoroBinaryValue {
    pub fn unwrap(self) -> Vec<u8> {
        match Arc::try_unwrap(self.0) {
            Ok(v) => v,
            Err(arc) => (*arc).clone(),
        }
    }
}

impl LoroStringValue {
    pub fn unwrap(self) -> String {
        match Arc::try_unwrap(self.0) {
            Ok(v) => v,
            Err(arc) => (*arc).clone(),
        }
    }
}

impl LoroListValue {
    pub fn unwrap(self) -> Vec<LoroValue> {
        match Arc::try_unwrap(self.0) {
            Ok(v) => v,
            Err(arc) => (*arc).clone(),
        }
    }
}

impl LoroMapValue {
    pub fn unwrap(self) -> FxHashMap<String, LoroValue> {
        match Arc::try_unwrap(self.0) {
            Ok(v) => v,
            Err(arc) => (*arc).clone(),
        }
    }
}

impl FromIterator<LoroValue> for LoroListValue {
    fn from_iter<T: IntoIterator<Item = LoroValue>>(iter: T) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl FromIterator<(String, LoroValue)> for LoroMapValue {
    fn from_iter<T: IntoIterator<Item = (String, LoroValue)>>(iter: T) -> Self {
        let map: FxHashMap<_, _> = iter.into_iter().collect();
        Self(Arc::new(map))
    }
}

impl AsRef<str> for LoroStringValue {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<[LoroValue]> for LoroListValue {
    fn as_ref(&self) -> &[LoroValue] {
        &self.0
    }
}

impl AsRef<FxHashMap<String, LoroValue>> for LoroMapValue {
    fn as_ref(&self) -> &FxHashMap<String, LoroValue> {
        &self.0
    }
}

const MAX_DEPTH: usize = 128;
impl<'a> arbitrary::Arbitrary<'a> for LoroValue {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let value = match u.int_in_range(0..=7).unwrap() {
            0 => Self::Null,
            1 => Self::Bool(u.arbitrary()?),
            2 => Self::Double(u.arbitrary()?),
            3 => Self::I64(u.arbitrary()?),
            4 => Self::Binary(LoroBinaryValue::arbitrary(u)?),
            5 => Self::String(LoroStringValue::arbitrary(u)?),
            6 => Self::List(LoroListValue::arbitrary(u)?),
            7 => Self::Map(LoroMapValue::arbitrary(u)?),
            _ => unreachable!(),
        };

        if value.get_depth() > MAX_DEPTH {
            Err(arbitrary::Error::IncorrectFormat)
        } else {
            Ok(value)
        }
    }
}

impl LoroValue {
    pub fn get_by_key(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Map(map) => map.get(key),
            _ => None,
        }
    }

    pub fn get_by_index(&self, index: isize) -> Option<&Self> {
        match self {
            Self::List(list) => {
                if index < 0 {
                    list.get(list.len() - (-index) as usize)
                } else {
                    list.get(index as usize)
                }
            }
            _ => None,
        }
    }

    pub fn is_false(&self) -> bool {
        match self {
            Self::Bool(b) => !*b,
            _ => false,
        }
    }

    pub fn get_depth(&self) -> usize {
        let mut max_depth = 0;
        let mut value_depth_pairs = vec![(self, 0)];
        while let Some((value, depth)) = value_depth_pairs.pop() {
            match value {
                Self::List(arr) => {
                    for v in arr.iter() {
                        value_depth_pairs.push((v, depth + 1));
                    }
                    max_depth = max_depth.max(depth + 1);
                }
                Self::Map(map) => {
                    for (_, v) in map.iter() {
                        value_depth_pairs.push((v, depth + 1));
                    }

                    max_depth = max_depth.max(depth + 1);
                }
                _ => {}
            }
        }

        max_depth
    }

    // TODO: add checks for too deep value, and return err if users
    // try to insert such value into a container
    pub fn is_too_deep(&self) -> bool {
        self.get_depth() > MAX_DEPTH
    }

    /// Visit the all list items or map's values
    pub fn visit_children(&self, f: &mut dyn FnMut(&Self)) {
        match self {
            Self::List(list) => {
                for v in list.iter() {
                    f(v);
                }
            }
            Self::Map(m) => {
                for (_k, v) in m.iter() {
                    f(v)
                }
            }
            _ => {}
        }
    }
}

impl Index<&str> for LoroValue {
    type Output = Self;

    fn index(&self, index: &str) -> &Self::Output {
        match self {
            Self::Map(map) => map.get(index).unwrap_or(&Self::Null),
            _ => &Self::Null,
        }
    }
}

impl Index<usize> for LoroValue {
    type Output = Self;

    fn index(&self, index: usize) -> &Self::Output {
        match self {
            Self::List(list) => list.get(index).unwrap_or(&Self::Null),
            _ => &Self::Null,
        }
    }
}

impl TryFrom<LoroValue> for bool {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::Bool(v) => Ok(v),
            _ => Err("not a bool"),
        }
    }
}

impl TryFrom<LoroValue> for f64 {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::Double(v) => Ok(v),
            _ => Err("not a double"),
        }
    }
}

impl TryFrom<LoroValue> for i32 {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::I64(v) => Ok(v as Self),
            _ => Err("not a i32"),
        }
    }
}

impl TryFrom<LoroValue> for Arc<Vec<u8>> {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::Binary(v) => Ok(Self::clone(&v.0)),
            _ => Err("not a binary"),
        }
    }
}

impl TryFrom<LoroValue> for Arc<String> {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::String(v) => Ok(Self::clone(&v.0)),
            _ => Err("not a string"),
        }
    }
}

impl TryFrom<LoroValue> for Arc<Vec<LoroValue>> {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::List(v) => Ok(Self::clone(&v.0)),
            _ => Err("not a list"),
        }
    }
}

impl TryFrom<LoroValue> for Arc<FxHashMap<String, LoroValue>> {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::Map(v) => Ok(Self::clone(&v.0)),
            _ => Err("not a map"),
        }
    }
}

impl TryFrom<LoroValue> for ContainerID {
    type Error = &'static str;

    fn try_from(value: LoroValue) -> Result<Self, Self::Error> {
        match value {
            LoroValue::Container(v) => Ok(v),
            _ => Err("not a container"),
        }
    }
}

impl Hash for LoroValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Null => {}
            Self::Bool(v) => {
                state.write_u8(*v as u8);
            }
            Self::Double(v) => {
                state.write_u64(v.to_bits());
            }
            Self::I64(v) => {
                state.write_i64(*v);
            }
            Self::Binary(v) => {
                v.hash(state);
            }
            Self::String(v) => {
                v.hash(state);
            }
            Self::List(v) => {
                v.hash(state);
            }
            Self::Map(v) => {
                state.write_usize(v.len());
                for (k, v) in v.iter() {
                    k.hash(state);
                    v.hash(state);
                }
            }
            Self::Container(v) => {
                v.hash(state);
            }
        }
    }
}

impl Eq for LoroValue {}

impl<S: Into<String>, M> From<HashMap<S, Self, M>> for LoroValue {
    fn from(map: HashMap<S, Self, M>) -> Self {
        let mut new_map = FxHashMap::default();
        for (k, v) in map {
            new_map.insert(k.into(), v);
        }

        Self::Map(new_map.into())
    }
}

impl From<Vec<u8>> for LoroValue {
    fn from(vec: Vec<u8>) -> Self {
        Self::Binary(vec.into())
    }
}

impl From<&'_ [u8]> for LoroValue {
    fn from(vec: &[u8]) -> Self {
        Self::Binary(vec.to_vec().into())
    }
}

impl<const N: usize> From<&'_ [u8; N]> for LoroValue {
    fn from(vec: &[u8; N]) -> Self {
        Self::Binary(vec.to_vec().into())
    }
}

impl From<i32> for LoroValue {
    fn from(v: i32) -> Self {
        Self::I64(v as i64)
    }
}

impl From<u32> for LoroValue {
    fn from(v: u32) -> Self {
        Self::I64(v as i64)
    }
}

impl From<i64> for LoroValue {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}

impl From<u16> for LoroValue {
    fn from(v: u16) -> Self {
        Self::I64(v as i64)
    }
}

impl From<i16> for LoroValue {
    fn from(v: i16) -> Self {
        Self::I64(v as i64)
    }
}

impl From<f64> for LoroValue {
    fn from(v: f64) -> Self {
        Self::Double(v)
    }
}

impl From<bool> for LoroValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl<T: Into<Self>> From<Vec<T>> for LoroValue {
    fn from(value: Vec<T>) -> Self {
        let vec: Vec<Self> = value.into_iter().map(|x| x.into()).collect();
        Self::List(vec.into())
    }
}

impl From<&str> for LoroValue {
    fn from(v: &str) -> Self {
        Self::String(v.to_string().into())
    }
}

impl From<String> for LoroValue {
    fn from(v: String) -> Self {
        Self::String(v.into())
    }
}

impl<'a> From<&'a [Self]> for LoroValue {
    fn from(v: &'a [Self]) -> Self {
        Self::List(v.to_vec().into())
    }
}

impl From<ContainerID> for LoroValue {
    fn from(v: ContainerID) -> Self {
        Self::Container(v)
    }
}

#[cfg(feature = "wasm")]
pub mod wasm {
    use fxhash::FxHashMap;
    use js_sys::{Array, Object, Uint8Array};
    use wasm_bindgen::{JsCast, JsValue, __rt::IntoJsResult};

    use crate::{ContainerID, LoroError, LoroValue};

    pub fn convert(value: LoroValue) -> JsValue {
        match value {
            LoroValue::Null => JsValue::NULL,
            LoroValue::Bool(b) => JsValue::from_bool(b),
            LoroValue::Double(f) => JsValue::from_f64(f),
            LoroValue::I64(i) => JsValue::from_f64(i as f64),
            LoroValue::String(s) => JsValue::from_str(&s),
            LoroValue::Binary(binary) => {
                let binary = binary.unwrap();
                let arr = Uint8Array::new_with_length(binary.len() as u32);
                for (i, v) in binary.into_iter().enumerate() {
                    arr.set_index(i as u32, v);
                }
                arr.into_js_result().unwrap()
            }
            LoroValue::List(list) => {
                let list = list.unwrap();
                let arr = Array::new_with_length(list.len() as u32);
                for (i, v) in list.into_iter().enumerate() {
                    arr.set(i as u32, convert(v));
                }
                arr.into_js_result().unwrap()
            }
            LoroValue::Map(m) => {
                let m = m.unwrap();
                let map = Object::new();
                for (k, v) in m.into_iter() {
                    let str: &str = &k;
                    js_sys::Reflect::set(&map, &JsValue::from_str(str), &convert(v)).unwrap();
                }

                map.into_js_result().unwrap()
            }
            LoroValue::Container(container_id) => JsValue::from(&container_id),
        }
    }

    impl From<LoroValue> for JsValue {
        fn from(value: LoroValue) -> Self {
            convert(value)
        }
    }

    impl From<JsValue> for LoroValue {
        fn from(js_value: JsValue) -> Self {
            if js_value.is_null() || js_value.is_undefined() {
                Self::Null
            } else if js_value.as_bool().is_some() {
                Self::Bool(js_value.as_bool().unwrap())
            } else if js_value.as_f64().is_some() {
                let num = js_value.as_f64().unwrap();
                if num.fract() == 0.0 && num <= i64::MAX as f64 && num >= i64::MIN as f64 {
                    Self::I64(num as i64)
                } else {
                    Self::Double(num)
                }
            } else if js_value.is_string() {
                Self::String(js_value.as_string().unwrap().into())
            } else if js_value.has_type::<Array>() {
                let array = js_value.unchecked_into::<Array>();
                let mut list = Vec::new();
                for i in 0..array.length() {
                    list.push(Self::from(array.get(i)));
                }

                Self::List(list.into())
            } else if js_value.is_instance_of::<Uint8Array>() {
                let array = js_value.unchecked_into::<Uint8Array>();
                let mut binary = Vec::new();
                for i in 0..array.length() {
                    binary.push(array.get_index(i));
                }

                Self::Binary(binary.into())
            } else if js_value.is_object() {
                let object = js_value.unchecked_into::<Object>();
                let mut map = FxHashMap::default();
                for key in js_sys::Reflect::own_keys(&object).unwrap().iter() {
                    let key = key.as_string().unwrap();
                    map.insert(
                        key.clone(),
                        Self::from(js_sys::Reflect::get(&object, &key.into()).unwrap()),
                    );
                }

                Self::Map(map.into())
            } else {
                panic!("Fail to convert JsValue {:?} to LoroValue ", js_value)
            }
        }
    }

    impl From<&ContainerID> for JsValue {
        fn from(id: &ContainerID) -> Self {
            Self::from_str(id.to_string().as_str())
        }
    }

    impl TryFrom<JsValue> for ContainerID {
        type Error = LoroError;

        fn try_from(value: JsValue) -> Result<Self, Self::Error> {
            if !value.is_string() {
                return Err(LoroError::DecodeError(
                    "Given ContainerId is not string".into(),
                ));
            }

            let s = value.as_string().unwrap();
            Self::try_from(s.as_str()).map_err(|_| {
                LoroError::DecodeError(
                    format!("Given ContainerId is not a valid ContainerID: {}", s).into(),
                )
            })
        }
    }
}

const LORO_CONTAINER_ID_PREFIX: &str = "🦜:";

impl Serialize for LoroValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            // json type
            match self {
                Self::Null => serializer.serialize_unit(),
                Self::Bool(b) => serializer.serialize_bool(*b),
                Self::Double(d) => serializer.serialize_f64(*d),
                Self::I64(i) => serializer.serialize_i64(*i),
                Self::String(s) => serializer.serialize_str(s),
                Self::Binary(b) => serializer.collect_seq(b.iter()),
                Self::List(l) => serializer.collect_seq(l.iter()),
                Self::Map(m) => serializer.collect_map(m.iter()),
                Self::Container(id) => {
                    serializer.serialize_str(&format!("{}{}", LORO_CONTAINER_ID_PREFIX, id))
                }
            }
        } else {
            // binary type
            match self {
                Self::Null => serializer.serialize_unit_variant("LoroValue", 0, "Null"),
                Self::Bool(b) => serializer.serialize_newtype_variant("LoroValue", 1, "Bool", b),
                Self::Double(d) => {
                    serializer.serialize_newtype_variant("LoroValue", 2, "Double", d)
                }
                Self::I64(i) => serializer.serialize_newtype_variant("LoroValue", 3, "I32", i),
                Self::String(s) => {
                    serializer.serialize_newtype_variant("LoroValue", 4, "String", &**s)
                }

                Self::List(l) => serializer.serialize_newtype_variant("LoroValue", 5, "List", &**l),
                Self::Map(m) => serializer.serialize_newtype_variant("LoroValue", 6, "Map", &**m),
                Self::Container(id) => {
                    serializer.serialize_newtype_variant("LoroValue", 7, "Container", id)
                }
                Self::Binary(b) => {
                    serializer.serialize_newtype_variant("LoroValue", 8, "Binary", &**b)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for LoroValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            deserializer.deserialize_any(LoroValueVisitor)
        } else {
            deserializer.deserialize_enum(
                "LoroValue",
                &[
                    "Null",
                    "Bool",
                    "Double",
                    "I32",
                    "String",
                    "List",
                    "Map",
                    "Container",
                    "Binary",
                ],
                LoroValueEnumVisitor,
            )
        }
    }
}

struct LoroValueVisitor;

impl<'de> serde::de::Visitor<'de> for LoroValueVisitor {
    type Value = LoroValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a LoroValue")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(LoroValue::Null)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(LoroValue::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(LoroValue::I64(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(LoroValue::I64(v as i64))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(LoroValue::Double(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if let Some(id) = v.strip_prefix(LORO_CONTAINER_ID_PREFIX) {
            return Ok(LoroValue::Container(
                ContainerID::try_from(id)
                    .map_err(|_| serde::de::Error::custom("Invalid container id"))?,
            ));
        }
        Ok(LoroValue::String(v.to_owned().into()))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if let Some(id) = v.strip_prefix(LORO_CONTAINER_ID_PREFIX) {
            return Ok(LoroValue::Container(
                ContainerID::try_from(id)
                    .map_err(|_| serde::de::Error::custom("Invalid container id"))?,
            ));
        }

        Ok(LoroValue::String(v.into()))
    }

    fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let binary = Vec::from_iter(v.iter().copied());
        Ok(LoroValue::Binary(binary.into()))
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let binary = Vec::from_iter(v.iter().copied());
        Ok(LoroValue::Binary(binary.into()))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut list = Vec::new();
        while let Some(value) = seq.next_element()? {
            list.push(value);
        }
        Ok(LoroValue::List(list.into()))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut ans: FxHashMap<String, _> = FxHashMap::default();
        while let Some((key, value)) = map.next_entry::<String, _>()? {
            ans.insert(key, value);
        }

        Ok(LoroValue::Map(ans.into()))
    }
}

#[derive(Deserialize)]
enum LoroValueFields {
    Null,
    Bool,
    Double,
    I32,
    String,
    List,
    Map,
    Container,
    Binary,
}

struct LoroValueEnumVisitor;
impl<'de> serde::de::Visitor<'de> for LoroValueEnumVisitor {
    type Value = LoroValue;
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a loro value")
    }

    fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::EnumAccess<'de>,
    {
        match data.variant()? {
            (LoroValueFields::Null, v) => {
                v.unit_variant()?;
                Ok(LoroValue::Null)
            }
            (LoroValueFields::Bool, v) => v.newtype_variant().map(LoroValue::Bool),
            (LoroValueFields::Double, v) => v.newtype_variant().map(LoroValue::Double),
            (LoroValueFields::I32, v) => v.newtype_variant().map(LoroValue::I64),
            (LoroValueFields::String, v) => v
                .newtype_variant()
                .map(|x: String| LoroValue::String(x.into())),
            (LoroValueFields::List, v) => v
                .newtype_variant()
                .map(|x: Vec<LoroValue>| LoroValue::List(x.into())),
            (LoroValueFields::Map, v) => v
                .newtype_variant()
                .map(|x: FxHashMap<String, LoroValue>| LoroValue::Map(x.into())),
            (LoroValueFields::Container, v) => v.newtype_variant().map(LoroValue::Container),
            (LoroValueFields::Binary, v) => v
                .newtype_variant()
                .map(|x: Vec<u8>| LoroValue::Binary(x.into())),
        }
    }
}

pub fn to_value<T: Into<LoroValue>>(value: T) -> LoroValue {
    value.into()
}

#[cfg(feature = "serde_json")]
mod serde_json_impl {
    use serde_json::{Number, Value};

    use super::LoroValue;

    impl From<Value> for LoroValue {
        fn from(value: Value) -> Self {
            match value {
                Value::Null => Self::Null,
                Value::Bool(b) => Self::Bool(b),
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Self::I64(i)
                    } else {
                        Self::Double(n.as_f64().unwrap())
                    }
                }
                Value::String(s) => Self::String(s.into()),
                Value::Array(arr) => Self::List(arr.into_iter().map(Self::from).collect()),
                Value::Object(obj) => {
                    Self::Map(obj.into_iter().map(|(k, v)| (k, Self::from(v))).collect())
                }
            }
        }
    }

    use super::LORO_CONTAINER_ID_PREFIX;
    impl From<LoroValue> for Value {
        fn from(value: LoroValue) -> Self {
            match value {
                LoroValue::Null => Self::Null,
                LoroValue::Bool(b) => Self::Bool(b),
                LoroValue::Double(d) => Self::Number(Number::from_f64(d).unwrap()),
                LoroValue::I64(i) => Self::Number(Number::from(i)),
                LoroValue::String(s) => Self::String(s.to_string()),
                LoroValue::List(l) => Self::Array(l.iter().cloned().map(Self::from).collect()),
                LoroValue::Map(m) => Self::Object(
                    m.iter()
                        .map(|(k, v)| (k.clone(), Self::from(v.clone())))
                        .collect(),
                ),
                LoroValue::Container(id) => {
                    Self::String(format!("{}{}", LORO_CONTAINER_ID_PREFIX, id))
                }
                LoroValue::Binary(b) => Self::Array(b.iter().copied().map(Self::from).collect()),
            }
        }
    }
}
