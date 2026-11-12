use std::collections::BTreeMap;

use uuid::Uuid;

/// Maps exact native row receipts to receipts that tolerate known protobuf field reordering.
pub type DatabaseAliases = BTreeMap<Uuid, DatabaseStepAlias>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseStepAlias {
    pub canonical: Uuid,
}

const MAX_FIELDS: usize = 1024;

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
            let value = if let FieldType::Message(child) = child
                && let Some(nested) = Self::parse(body, child, remaining)
            {
                ordered &= nested.ordered;
                Value::Message(nested)
            } else {
                // Keep unsupported child messages byte-for-byte when their enclosing fields move.
                Value::Bytes(body)
            };
            fields.push(Field { tag, prefix, value });
        }
        if !ordered {
            fields.sort_by_key(|field| field.tag);
        }
        Some(Self { fields, ordered })
    }

    fn write(&self, out: &mut Vec<u8>) {
        for field in &self.fields {
            out.extend_from_slice(field.prefix);
            match &field.value {
                Value::Bytes(body) => out.extend_from_slice(body),
                Value::Message(node) => node.write(out),
            }
        }
    }
}

impl DatabaseStepAlias {
    #[must_use]
    pub fn from_wire(namespace: Uuid, raw: Uuid, wire: &[u8]) -> Option<Self> {
        let mut fields = MAX_FIELDS;
        let node = Node::parse(wire, Schema::Step, &mut fields)?;
        let canonical = if node.ordered {
            raw
        } else {
            let mut canonical = Vec::with_capacity(wire.len());
            node.write(&mut canonical);
            Uuid::new_v5(&namespace, &canonical)
        };
        Some(Self { canonical })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(wire: &[u8]) -> Option<DatabaseStepAlias> {
        let namespace = Uuid::nil();
        DatabaseStepAlias::from_wire(namespace, Uuid::new_v5(&namespace, wire), wire)
    }

    #[test]
    fn nested_order_matches_an_existing_raw_receipt() {
        let canonical = [8, 15, 162, 1, 6, 10, 1, b'a', 66, 1, b'b'];
        let reversed = [162, 1, 6, 66, 1, b'b', 10, 1, b'a', 8, 15];
        let first = receipt(&canonical).unwrap();
        let second = receipt(&reversed).unwrap();
        assert_eq!(first.canonical, second.canonical);
        assert_eq!(first.canonical, Uuid::new_v5(&Uuid::nil(), &canonical));
        for row in [
            &[8, 15, 8, 15, 162, 1, 0][..],
            &[8, 15, 154, 1, 0, 162, 1, 0],
        ] {
            assert!(receipt(row).is_none());
        }
    }

    #[test]
    fn outer_fields_can_move_around_an_exact_unknown_child() {
        let first = [8, 15, 32, 1, 162, 1, 3, 18, 1, b'a'];
        let second = [32, 1, 8, 15, 162, 1, 3, 18, 1, b'a'];
        assert_eq!(
            receipt(&first).unwrap().canonical,
            receipt(&second).unwrap().canonical
        );
        let mut changed = first;
        changed[9] = b'b';
        assert_ne!(
            receipt(&changed).unwrap().canonical,
            receipt(&second).unwrap().canonical
        );
    }

    #[test]
    fn metadata_field_order_has_a_stable_receipt() {
        let first = [
            8, 15, 32, 1, 42, 6, 10, 2, 8, 1, 24, 2, 162, 1, 3, 10, 1, b'x',
        ];
        let second = [
            8, 15, 32, 1, 42, 6, 24, 2, 10, 2, 8, 1, 162, 1, 3, 10, 1, b'x',
        ];
        assert_eq!(receipt(&first), receipt(&second));
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
    }
}
