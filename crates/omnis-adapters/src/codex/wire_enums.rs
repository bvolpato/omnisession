//! Decode enum fields with the enclosing event's native Serde representation.

use serde::{Deserialize, de::value::MapDeserializer};
use serde_json::Value;

/// Internally tagged events buffer their fields before Serde decodes nested unit variants.
/// Project just the required fields so an unrelated payload never enters this buffer.
pub(super) fn accepts<'a, T: Deserialize<'a>, const N: usize>(
    item: &'a Value,
    keys: [&'static str; N],
) -> bool {
    let fields = keys
        .into_iter()
        .filter_map(|key| item.get(key).map(|value| (key, value)));
    T::deserialize(MapDeserializer::<_, serde_json::Error>::new(fields)).is_ok()
}

/// Once Serde accepts the enum, expose its variant name to the history reader.
pub(super) fn name_unit(value: &mut Value) {
    if let Value::Object(tag) = value
        && let Some((name, _)) = std::mem::take(tag).into_iter().next()
    {
        *value = name.into();
    }
}
