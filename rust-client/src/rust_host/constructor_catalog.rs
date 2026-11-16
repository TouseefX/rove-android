//! Keep the constructor catalog encoded until a class is actually requested.
//! The encoded bytes are static; decoded classes are shared across VM/Actors.
use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use std::{collections::HashMap, sync::OnceLock};

#[derive(Deserialize)]
struct Encoded<'a> {
    #[serde(borrow)]
    classes: HashMap<&'a str, &'a RawValue>,
}
struct Entry {
    raw: &'static RawValue,
    decoded: OnceLock<Value>,
}
struct Catalog {
    entries: HashMap<&'static str, Entry>,
}
impl Catalog {
    fn new(source: &'static str) -> Self {
        let encoded: Encoded<'static> =
            serde_json::from_str(source).expect("pinned constructor catalog");
        Self {
            entries: encoded
                .classes
                .into_iter()
                .map(|(name, raw)| {
                    (
                        name,
                        Entry {
                            raw,
                            decoded: OnceLock::new(),
                        },
                    )
                })
                .collect(),
        }
    }
    fn get(&self, class: &str) -> Option<&Value> {
        let entry = self.entries.get(class)?;
        Some(entry.decoded.get_or_init(|| {
            serde_json::from_str(entry.raw.get()).expect("pinned constructor entry")
        }))
    }
}
static CATALOG: OnceLock<Catalog> = OnceLock::new();
pub(super) fn class(name: &str) -> Option<&'static Value> {
    CATALOG
        .get_or_init(|| Catalog::new(include_str!("../../../runtime/class-constructors-745.json")))
        .get(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lazy_catalog_preserves_every_class_default_without_decoding_other_classes() {
        let source = include_str!("../../../runtime/class-constructors-745.json");
        let catalog = Catalog::new(source);
        assert!(catalog
            .entries
            .values()
            .all(|entry| entry.decoded.get().is_none()));
        assert!(catalog.get("Part").is_some());
        assert_eq!(
            catalog
                .entries
                .values()
                .filter(|entry| entry.decoded.get().is_some())
                .count(),
            1
        );
        let original: Value = serde_json::from_str(source).unwrap();
        for (name, expected) in original["classes"].as_object().unwrap() {
            assert_eq!(catalog.get(name), Some(expected), "{name}");
        }
        assert!(catalog.get("NotARealClass").is_none());
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    assert!(std::ptr::eq(
                        catalog.get("Part").unwrap(),
                        catalog.get("Part").unwrap()
                    ))
                });
            }
        });
    }
}
