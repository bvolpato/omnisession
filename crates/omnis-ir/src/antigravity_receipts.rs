use std::collections::BTreeMap;

use uuid::Uuid;

/// Maps exact native row receipts to receipts that tolerate known protobuf field reordering.
pub type DatabaseAliases = BTreeMap<Uuid, DatabaseStepAlias>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseStepAlias {
    pub canonical: Uuid,
    namespace: Uuid,
    legacy_wire: Option<Vec<u8>>,
}

// These limits bound parsing per row and compatibility work per snapshot.
const MAX_FIELDS: usize = 1024;
const MAX_LAYOUTS: usize = 24;
pub const LEGACY_WIRE_BYTES: usize = 4 * 1024 * 1024;
pub const LEGACY_LARGE_BYTES: usize = 16 * 1024 * 1024;
pub const LEGACY_HASH_BYTES: usize = 32 * 1024 * 1024;
pub const LEGACY_HASH_LAYOUTS: usize = 8192;

#[derive(Clone, Copy)]
enum Schema {
    Step,
    Metadata,
    Timestamp,
    User,
    Item,
    Reply,
}

enum Value<'a> {
    Bytes(&'a [u8]),
    Message(Node<'a>),
}

struct Field<'a> {
    tag: u32,
    prefix: &'a [u8],
    value: Value<'a>,
}

struct Node<'a> {
    fields: Vec<Field<'a>>,
    groups: Vec<std::ops::Range<usize>>,
    layouts: usize,
    ordered: bool,
}

fn varint(input: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        if shift == 63 && byte > 1 {
            return None;
        }
        *input = rest;
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Some(value);
        }
    }
    None
}

enum FieldType {
    Scalar,
    Message(Schema),
}

impl Schema {
    fn field(self, tag: u32, wire: u64) -> Option<FieldType> {
        match (self, tag, wire) {
            (Self::Step, 5, 2) => Some(FieldType::Message(Self::Metadata)),
            (Self::Step, 19, 2) => Some(FieldType::Message(Self::User)),
            (Self::Step, 20, 2) => Some(FieldType::Message(Self::Reply)),
            (Self::Metadata, 1, 2) => Some(FieldType::Message(Self::Timestamp)),
            (Self::User, 3, 2) => Some(FieldType::Message(Self::Item)),
            (Self::Step, 1 | 4, 0)
            | (Self::Metadata, 3, 0)
            | (Self::Timestamp, 1 | 2, 0)
            | (Self::User, 1 | 2, 2)
            | (Self::Item, 1, 2)
            | (Self::Reply, 1 | 8, 2)
            | (Self::Metadata, 12, 2) => Some(FieldType::Scalar),
            _ => None,
        }
    }
}

impl<'a> Node<'a> {
    fn parse(mut wire: &'a [u8], schema: Schema, remaining: &mut usize) -> Option<Self> {
        let mut fields = Vec::new();
        let mut ordered = true;
        let mut prior_tag = 0;
        let mut seen = 0u32;
        let mut layouts = 1usize;
        while !wire.is_empty() {
            *remaining = remaining.checked_sub(1)?;
            let start = wire;
            let key = varint(&mut wire)?;
            let tag = u32::try_from(key >> 3).ok()?;
            ordered &= tag >= prior_tag;
            prior_tag = tag;
            let kind = key & 7;
            let child = schema.field(tag, kind)?;
            let bit = 1u32 << tag;
            // Singular duplicates and competing message bodies have order-dependent meaning.
            if seen & bit != 0 && !(matches!(schema, Schema::User) && tag == 3) {
                return None;
            }
            if matches!(schema, Schema::Step)
                && matches!(tag, 19 | 20)
                && seen & ((1 << 19) | (1 << 20)) != 0
            {
                return None;
            }
            seen |= bit;
            let (prefix, body) = if kind == 0 {
                let before = wire;
                varint(&mut wire)?;
                (
                    &start[..start.len() - before.len()],
                    &before[..before.len() - wire.len()],
                )
            } else {
                let length = usize::try_from(varint(&mut wire)?).ok()?;
                let prefix = &start[..start.len() - wire.len()];
                let (body, rest) = wire.split_at_checked(length)?;
                wire = rest;
                (prefix, body)
            };
            let value = if let FieldType::Message(child) = child {
                let nested = Self::parse(body, child, remaining)?;
                layouts = layouts.saturating_mul(nested.layouts);
                ordered &= nested.ordered;
                Value::Message(nested)
            } else {
                Value::Bytes(body)
            };
            fields.push(Field { tag, prefix, value });
        }
        if !ordered {
            fields.sort_by_key(|field| field.tag);
        }
        let mut groups = Vec::new();
        let mut from = 0;
        while from < fields.len() {
            let end = from + fields[from..].partition_point(|f| f.tag == fields[from].tag);
            groups.push(from..end);
            layouts = layouts.saturating_mul(groups.len());
            from = end;
        }
        Some(Self {
            fields,
            groups,
            layouts,
            ordered,
        })
    }

    // Rank zero emits sorted fields. Other ranks reproduce possible orders for older raw receipts.
    fn write(&self, mut rank: usize, out: &mut Vec<u8>) {
        let mut order = [0, 1, 2, 3];
        let n = self.groups.len();
        for i in 0..n {
            let at = i + rank % (n - i);
            rank /= n - i;
            order[i..=at].rotate_right(1);
        }
        for &group in &order[..n] {
            for f in &self.fields[self.groups[group].clone()] {
                out.extend_from_slice(f.prefix);
                match &f.value {
                    Value::Bytes(body) => out.extend_from_slice(body),
                    Value::Message(node) => {
                        node.write(rank % node.layouts, out);
                        rank /= node.layouts;
                    }
                }
            }
        }
    }
}

impl DatabaseStepAlias {
    #[must_use]
    pub fn from_wire(
        namespace: Uuid,
        raw: Uuid,
        wire: Vec<u8>,
        remaining: &mut usize,
        large: &mut usize,
    ) -> Option<Self> {
        let mut fields = MAX_FIELDS;
        let node = Node::parse(&wire, Schema::Step, &mut fields)?;
        if node.layouts == 1 {
            return None;
        }
        let budget = if wire.len() > LEGACY_WIRE_BYTES && node.layouts == 2 {
            large
        } else {
            remaining
        };
        let keep = wire.len() <= *budget;
        if keep {
            *budget -= wire.len();
        }
        let (canonical, legacy_wire) = if node.ordered {
            (raw, keep.then_some(wire))
        } else {
            let mut canonical = Vec::with_capacity(wire.len());
            node.write(0, &mut canonical);
            (
                Uuid::new_v5(&namespace, &canonical),
                keep.then_some(canonical),
            )
        };
        Some(Self {
            canonical,
            namespace,
            legacy_wire,
        })
    }

    #[must_use]
    pub fn matches_legacy(&self, old: Uuid, bytes: &mut usize, layouts: &mut usize) -> bool {
        let Some(wire) = &self.legacy_wire else {
            return false;
        };
        if *layouts == 0 || wire.len() > *bytes {
            return false;
        }
        let mut fields = MAX_FIELDS;
        let Some(node) = Node::parse(wire, Schema::Step, &mut fields) else {
            return false;
        };
        let mut encoded = Vec::with_capacity(wire.len());
        for rank in 1..node.layouts.min(MAX_LAYOUTS) {
            if *layouts == 0 || wire.len() > *bytes {
                break;
            }
            *layouts -= 1;
            *bytes -= wire.len();
            encoded.clear();
            node.write(rank, &mut encoded);
            if Uuid::new_v5(&self.namespace, &encoded) == old {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(wire: &[u8]) -> Option<DatabaseStepAlias> {
        let namespace = Uuid::nil();
        DatabaseStepAlias::from_wire(
            namespace,
            Uuid::new_v5(&namespace, wire),
            wire.to_vec(),
            &mut 1024,
            &mut 0,
        )
    }

    #[test]
    fn nested_order_matches_an_existing_raw_receipt() {
        let canonical = [8, 15, 162, 1, 6, 10, 1, b'a', 66, 1, b'b'];
        let reversed = [162, 1, 6, 66, 1, b'b', 10, 1, b'a', 8, 15];
        let first = receipt(&canonical).unwrap();
        let second = receipt(&reversed).unwrap();
        assert_eq!(first.canonical, second.canonical);
        let old = Uuid::new_v5(&Uuid::nil(), &reversed);
        assert!(!first.matches_legacy(old, &mut 0, &mut 10));
        assert!(first.matches_legacy(old, &mut 1024, &mut 10));
        for row in [
            &[8, 15, 162, 1, 3, 18, 1, b'a'][..],
            &[8, 15, 8, 15, 162, 1, 0],
            &[8, 15, 154, 1, 0, 162, 1, 0],
        ] {
            assert!(receipt(row).is_none());
        }
    }

    #[test]
    fn repeated_items_keep_their_original_order() {
        let row = |a, b| [8, 14, 154, 1, 10, 26, 3, 10, 1, a, 26, 3, 10, 1, b];
        let first = row(b'a', b'b');
        let reversed = row(b'b', b'a');
        assert_ne!(
            receipt(&first).unwrap().canonical,
            receipt(&reversed).unwrap().canonical
        );
        assert!(!receipt(&first).unwrap().matches_legacy(
            Uuid::new_v5(&Uuid::nil(), &reversed),
            &mut 1024,
            &mut 10
        ));
    }
}
