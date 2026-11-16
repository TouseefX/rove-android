//! keep one shared world instead of copying the whole thing for every actor.
//! `WorldStore` holds the latest received state. a `WorldView` shares that base
//! and keeps local edits in a small overlay, so forking a view stays cheap.
//! ids, names, keys, metadata and node records are shared internally.
//! json is just for the public boundary.

use crate::packed_properties::{
    packed_value_layout_bytes, CompactSharedProperty, CompactCreatedProperties, PackedProperties, SharedPayloadResolver,
    SharedPropertyValue,
};
use crate::shared_payload::SharedPayload;
use serde_json::{Map, Number, Value};
use std::{
    cell::RefCell,
    collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    io,
    mem::size_of,
    sync::{Arc, Mutex, Weak},
};

// Smaller shards stay in the allocator's small pages and copy less data
// when a received update changes just one immutable world-index bucket.
const SHARD_COUNT: usize = 1024;
const REQUIRED_VERSION: u64 = 1;
const MAX_PENDING_EVENTS: usize = 4096;
// real captures can have around 40k nodes. keep a limit, but do not cut
// valid worlds off at the old tiny-fixture cap.
const MAX_NODES: usize = 100_000;

// The immutable record already owns its ID. Keep one index entry with its
// receipt-order position instead of two hash maps carrying duplicate ID keys.
#[derive(Clone, Default)]
struct NodeMap {
    entries: hashbrown::HashTable<(Arc<NodeRecord>, usize)>,
    hashes: std::collections::hash_map::RandomState,
}

impl NodeMap {
    fn hash(&self, id: &str) -> u64 {
        std::hash::BuildHasher::hash_one(&self.hashes, id)
    }
    fn get(&self, id: &str) -> Option<&Arc<NodeRecord>> {
        self.entries.find(self.hash(id), |(node, _)| node.id.as_ref() == id).map(|(node, _)| node)
    }
    fn position(&self, id: &str) -> Option<usize> {
        self.entries.find(self.hash(id), |(node, _)| node.id.as_ref() == id).map(|(_, position)| *position)
    }
    fn insert(&mut self, record: Arc<NodeRecord>, position: usize) {
        let hash = self.hash(&record.id);
        if let Some(entry) = self.entries.find_mut(hash, |(node, _)| node.id == record.id) {
            *entry = (record, position);
        } else {
            let hashes = &self.hashes;
            self.entries.insert_unique(hash, (record, position), |(node, _)| {
                std::hash::BuildHasher::hash_one(hashes, node.id.as_ref())
            });
        }
    }
    fn remove(&mut self, id: &str) -> Option<(Arc<NodeRecord>, usize)> {
        let hash = self.hash(id);
        self.entries.find_entry(hash, |(node, _)| node.id.as_ref() == id).ok().map(|entry| entry.remove().0)
    }
    fn values(&self) -> impl Iterator<Item = &Arc<NodeRecord>> {
        self.entries.iter().map(|(node, _)| node)
    }
    fn keys(&self) -> impl Iterator<Item = &Arc<str>> {
        self.values().map(|node| &node.id)
    }
    fn capacity(&self) -> usize { self.entries.capacity() }
}

const MAX_INTERNED_METADATA_BYTES: usize = 512;
const MAX_METADATA_INTERN_BUCKETS: usize = 4096;
const MAX_METADATA_INTERN_COLLISIONS: usize = 8;
const METADATA_PRUNE_INTERVAL: usize = 256;
const INTERN_PRUNE_INTERVAL: usize = 8;
const INTERN_PRUNE_BUCKETS_PER_TICK: usize = 16;
const INTERN_PRUNE_ENTRIES_PER_BUCKET: usize = 4;
const INTERN_PRUNE_BYTES_THRESHOLD: usize = 32 * 1024;
const INTERN_SHRINK_MIN_CAPACITY: usize = 64;
const INTERN_SHRINK_MAX_LEN: usize = 16;

#[derive(Default)]
struct MetadataInterner {
    buckets: HashMap<u64, Vec<Weak<MetadataBacking>>>,
    skipped_new_keys: usize,
}

thread_local! {
    // Weak bounded buckets do not pin metadata after its nodes/views are gone.
    // The key is only a hash; equality is checked against canonical JSON bytes.
    static PACKED_METADATA_INTERN: RefCell<MetadataInterner> =
        RefCell::new(MetadataInterner::default());
}

struct BoundedJsonBytes {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl io::Write for BoundedJsonBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_INTERNED_METADATA_BYTES {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "metadata interning bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_metadata_bytes(map: &Map<String, Value>) -> Option<Vec<u8>> {
    let mut writer = BoundedJsonBytes {
        bytes: Vec::new(),
        exceeded: false,
    };
    match serde_json::to_writer(&mut writer, map) {
        Ok(()) => Some(writer.bytes),
        Err(_) if writer.exceeded => None,
        Err(_) => None,
    }
}

fn intern_packed_metadata(map: &Map<String, Value>) -> Result<Arc<MetadataBacking>, String> {
    PackedProperties::validate_map(map)?;
    let Some(canonical) = bounded_metadata_bytes(map) else {
        return Ok(Arc::new(MetadataBacking::Packed(PackedProperties::new_validated(map)?)));
    };
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = hasher.finish();
    if let Some(shared) =
        PACKED_METADATA_INTERN.with(|cell| cell.borrow_mut().find(hash, &canonical))
    {
        return Ok(shared);
    }
    let packed = Arc::new(MetadataBacking::Packed(PackedProperties::new_validated(map)?));
    PACKED_METADATA_INTERN.with(|cell| cell.borrow_mut().insert(hash, &packed));
    Ok(packed)
}

impl MetadataInterner {
    fn find(&mut self, hash: u64, canonical: &[u8]) -> Option<Arc<MetadataBacking>> {
        let entries = self.buckets.get_mut(&hash)?;
        entries.retain(|entry| entry.strong_count() > 0);
        entries.iter().filter_map(Weak::upgrade).find(|existing| {
            serde_json::to_vec(&existing.unpack()).expect("packed metadata map serializes")
                == canonical
        })
    }

    fn insert(&mut self, hash: u64, packed: &Arc<MetadataBacking>) {
        let has_bucket = self.buckets.contains_key(&hash);
        if !has_bucket && self.buckets.len() >= MAX_METADATA_INTERN_BUCKETS {
            self.skipped_new_keys += 1;
            if self.skipped_new_keys >= METADATA_PRUNE_INTERVAL {
                self.prune_dead();
            }
            if self.buckets.len() >= MAX_METADATA_INTERN_BUCKETS {
                return;
            }
        }
        let entries = self.buckets.entry(hash).or_default();
        entries.retain(|entry| entry.strong_count() > 0);
        if entries.len() < MAX_METADATA_INTERN_COLLISIONS {
            entries.push(Arc::downgrade(packed));
        }
    }

    fn prune_dead(&mut self) {
        self.buckets.retain(|_, entries| {
            entries.retain(|entry| entry.strong_count() > 0);
            !entries.is_empty()
        });
        self.skipped_new_keys = 0;
    }

    #[cfg(test)]
    fn intern(
        &mut self,
        hash: u64,
        canonical: &[u8],
        packed: PackedProperties,
    ) -> Arc<MetadataBacking> {
        if let Some(existing) = self.find(hash, canonical) {
            return existing;
        }
        let result = Arc::new(MetadataBacking::Packed(packed));
        self.insert(hash, &result);
        result
    }
}

/// Compact metadata for ordinary replicated nodes. PackedProperties applies a
/// depth bound that is useful for mutable engine properties but did not apply
/// to the old opaque metadata map. Preserve that edge of the old snapshot API
/// by retaining only over-depth metadata maps in a shared fallback.
#[derive(Clone, Debug)]
enum MetadataBacking {
    Packed(PackedProperties),
    Deep(Map<String, Value>),
}

#[derive(Clone, Debug)]
struct PackedMetadata(Arc<MetadataBacking>);

impl PackedMetadata {
    fn new(map: &Map<String, Value>) -> Result<Self, String> {
        match intern_packed_metadata(map) {
            Ok(value) => Ok(Self(value)),
            Err(error) if error == "property nesting exceeds storage limit" =>
                Ok(Self(Arc::new(MetadataBacking::Deep(map.clone())))),
            Err(error) => Err(error),
        }
    }
    fn unpack(&self) -> Map<String, Value> { self.0.unpack() }
    fn get(&self, key: &str) -> Option<Value> { self.0.get(key) }
    fn get_path(&self, path: &[&str]) -> Option<Value> { self.0.get_path(path) }
    fn stored_bytes(&self) -> usize { self.0.stored_bytes() }
    fn packed_backing(&self) -> Option<&Arc<MetadataBacking>> {
        matches!(self.0.as_ref(), MetadataBacking::Packed(_)).then_some(&self.0)
    }
}

impl MetadataBacking {
    fn unpack(&self) -> Map<String, Value> {
        match self {
            Self::Packed(value) => value.unpack(),
            Self::Deep(value) => value.clone(),
        }
    }

    fn get(&self, key: &str) -> Option<Value> {
        match self {
            Self::Packed(value) => value.get(key),
            Self::Deep(value) => value.get(key).cloned(),
        }
    }

    fn get_path(&self, path: &[&str]) -> Option<Value> {
        let (section, rest) = path.split_first()?;
        match self {
            Self::Packed(value) => value.get_path(path),
            Self::Deep(value) => {
                let mut selected = value.get(*section)?;
                for key in rest {
                    selected = selected.as_object()?.get(*key)?;
                }
                Some(selected.clone())
            }
        }
    }

    fn stored_bytes(&self) -> usize {
        // Keep this as the additive per-node estimate used by the legacy
        // profile field. Arc-backed Packed metadata changes the physical enum
        // tag/reference layout; Deep payload bytes remain fully included.
        match self {
            Self::Packed(value) => value.stored_bytes(),
            Self::Deep(value) => {
                size_of::<Self>()
                    + value
                        .iter()
                        .map(|(key, value)| {
                            key.len() + size_of::<(String, Value)>() + estimate_value(value)
                        })
                        .sum::<usize>()
            }
        }
    }


}

/// The three JSON states needed by snapshot fields: omitted, explicit null,
/// and a value. In particular, absent and null engine properties stay distinct.
#[derive(Clone, Debug)]
enum Slot<T> {
    Missing,
    Null,
    Value(T),
}

#[derive(Clone, Debug)]
struct NodeRecord {
    id: Arc<str>,
    class_name: Arc<str>,
    name: Option<Arc<str>>,
    parent: Option<Arc<str>>,
    flags: u8,
    properties: PackedProperties,
    // Encodes the received metadata object into the same compact COW backing
    // used for properties. Large per-node origin and attribute maps otherwise
    // allocate a separate JSON tree for every replicated instance.
    metadata: PackedMetadata,
    // Internal provenance for runtime projection; intentionally not serialized.
    created_sequence: u64,
}

const NAME_PRESENT: u8 = 1;
const PARENT_PRESENT: u8 = 2;
const PROPERTIES_PRESENT: u8 = 4;
const PROPERTIES_NULL: u8 = 8;
const CREATED_SEQUENCE_PRESENT: u8 = 16;

impl<T> Slot<T> {
    fn into_value(self) -> Option<T> { if let Self::Value(value) = self { Some(value) } else { None } }
}

impl NodeRecord {
    fn name_slot(&self) -> Slot<&Arc<str>> {
        match self.name.as_ref() {
            Some(value) => Slot::Value(value),
            None if self.flags & NAME_PRESENT != 0 => Slot::Null,
            None => Slot::Missing,
        }
    }
    fn parent_slot(&self) -> Slot<&Arc<str>> {
        match self.parent.as_ref() {
            Some(value) => Slot::Value(value),
            None if self.flags & PARENT_PRESENT != 0 => Slot::Null,
            None => Slot::Missing,
        }
    }
    fn set_name(&mut self, name: Slot<Arc<str>>) {
        if matches!(name, Slot::Missing) { self.flags &= !NAME_PRESENT; }
        else { self.flags |= NAME_PRESENT; }
        self.name = name.into_value();
    }

    fn parse(value: &Value, pool: &mut InternPool) -> Result<Self, String> {
        Self::parse_with_shared(value, pool, None)
    }

    fn parse_with_shared(
        value: &Value,
        pool: &mut InternPool,
        shared: Option<&SharedPayloadResolver<'_>>,
    ) -> Result<Self, String> {
        let object = value.as_object().ok_or("world node must be an object")?;
        let id = required_string(object.get("id"), "node id")?;
        let class_name = required_string(object.get("class_name"), "node class_name")?;
        let name = object
            .get("name")
            .map(|v| parse_name_slot(v, pool))
            .transpose()?
            .unwrap_or(Slot::Missing);
        let parent = object
            .get("parent")
            .map(|v| parse_parent_slot(v, pool))
            .transpose()?
            .unwrap_or(Slot::Missing);

        let properties_present = object.contains_key("properties");
        let properties_value = object.get("properties");
        let properties_null = matches!(properties_value, Some(Value::Null));
        let properties = match properties_value {
            Some(Value::Object(values)) if crate::lightweight::enabled() =>
                PackedProperties::new_with_shared(&crate::lightweight::filter_map(values), shared)?,
            Some(Value::Object(values)) => PackedProperties::new_with_shared(values, shared)?,
            None | Some(Value::Null) => PackedProperties::new(&Map::new())?,
            Some(_) => return Err(format!("node {id:?} properties must be an object or null")),
        };

        let mut metadata = Map::new();
        for (key, value) in object {
            if matches!(
                key.as_str(),
                "id" | "class_name" | "name" | "parent" | "properties"
            ) {
                continue;
            }
            let value = if crate::lightweight::enabled() && matches!(key.as_str(),"origins"|"property_revisions") {
                value.as_object().map(|values|Value::Object(crate::lightweight::filter_map(values)))
                    .unwrap_or_else(||value.clone())
            } else { value.clone() };
            metadata.insert(key.clone(), value);
        }
        Ok(Self {
            // Node IDs are unique. Sharing the record/order Arc is enough;
            // a weak interner bucket per leaf costs more than it can save.
            // Parent strings, class names and names still use the interner.
            id: Arc::from(id.as_str()),
            class_name: pool.get(&class_name),
            flags: u8::from(!matches!(name, Slot::Missing)) * NAME_PRESENT
                | u8::from(!matches!(parent, Slot::Missing)) * PARENT_PRESENT
                | u8::from(properties_present) * PROPERTIES_PRESENT
                | u8::from(properties_null) * PROPERTIES_NULL,
            name: name.into_value(),
            parent: parent.into_value(),
            properties,
            metadata: PackedMetadata::new(&metadata)?,
            created_sequence: 0,
        })
    }

    fn property(&self, key: &str) -> Option<Value> {
        self.properties.get(key)
    }

    fn metadata_field(&self, section: &str, key: &str) -> Option<Value> {
        self.metadata.get_path(&[section, key])
    }

    fn parent_id(&self) -> Option<&str> {
        self.parent.as_deref()
    }

    fn set_property(&mut self, key: &str, value: Value) -> Result<(), String> {
        self.set_property_with_shared(key, value, None)
    }

    fn set_property_with_shared(
        &mut self,
        key: &str,
        value: Value,
        shared: Option<&SharedPayloadResolver<'_>>,
    ) -> Result<(), String> {
        self.properties.set_with_shared(key, value, shared)?;
        self.flags = (self.flags | PROPERTIES_PRESENT) & !PROPERTIES_NULL;
        Ok(())
    }

    fn set_property_ref_with_shared(
        &mut self,
        key: &str,
        value: &Value,
        shared: Option<&SharedPayloadResolver<'_>>,
    ) -> Result<(), String> {
        self.properties.set_ref_with_shared(key, value, shared)?;
        self.flags = (self.flags | PROPERTIES_PRESENT) & !PROPERTIES_NULL;
        Ok(())
    }

    fn set_shared_property(
        &mut self,
        key: &str,
        payload: Arc<SharedPayload>,
        force_bytes: bool,
    ) -> Result<(), String> {
        self.properties
            .set_shared_payload(key, payload, force_bytes)?;
        self.flags = (self.flags | PROPERTIES_PRESENT) & !PROPERTIES_NULL;
        Ok(())
    }

    fn set_shared_properties(&mut self, values: Vec<SharedPropertyValue>) -> Result<(), String> {
        if values.is_empty() {
            return Ok(());
        }
        self.properties.set_shared_payloads(values)?;
        self.flags = (self.flags | PROPERTIES_PRESENT) & !PROPERTIES_NULL;
        Ok(())
    }

    fn set_parent(&mut self, parent: Slot<Arc<str>>) {
        if matches!(parent, Slot::Missing) { self.flags &= !PARENT_PRESENT; }
        else { self.flags |= PARENT_PRESENT; }
        self.parent = parent.into_value();
    }

    fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("id".into(), Value::String(self.id.to_string()));
        object.insert(
            "class_name".into(),
            Value::String(self.class_name.to_string()),
        );
        match self.name_slot() {
            Slot::Missing => {}
            Slot::Null => {
                object.insert("name".into(), Value::Null);
            }
            Slot::Value(name) => {
                object.insert("name".into(), Value::String(name.to_string()));
            }
        }
        match self.parent_slot() {
            Slot::Missing => {}
            Slot::Null => {
                object.insert("parent".into(), Value::Null);
            }
            Slot::Value(parent) => {
                object.insert("parent".into(), Value::String(parent.to_string()));
            }
        }
        if self.flags & PROPERTIES_PRESENT != 0 {
            if self.flags & PROPERTIES_NULL != 0 {
                object.insert("properties".into(), Value::Null);
            } else {
                let values = self.properties.unpack();
                object.insert("properties".into(), Value::Object(values));
            }
        }
        for (key, value) in self.metadata.unpack() {
            object.insert(key, value);
        }
        Value::Object(object)
    }

    fn estimated_bytes(&self, seen_strings: &mut HashSet<*const ()>) -> usize {
        let properties = self.properties.stored_bytes();
        let metadata = self.metadata.stored_bytes();
        size_of::<Self>()
            + interned_string_bytes(&self.id, seen_strings)
            + interned_string_bytes(&self.class_name, seen_strings)
            + slot_string_bytes(&self.name, seen_strings)
            + slot_string_bytes(&self.parent, seen_strings)
            + properties
            + metadata
            + 2 * size_of::<usize>()
    }
}

fn interned_string_bytes(value: &Arc<str>, seen: &mut HashSet<*const ()>) -> usize {
    if seen.insert(Arc::as_ptr(value) as *const ()) {
        value.len()
    } else {
        0
    }
}

fn slot_string_bytes(value: &Option<Arc<str>>, seen: &mut HashSet<*const ()>) -> usize {
    match value {
        Some(value) => interned_string_bytes(value, seen),
        _ => 0,
    }
}

/// A weak interner avoids duplicate strings. Expired weak allocations are
/// reclaimed by bounded maintenance during later lookups. Hash collisions are
/// resolved by comparing live strings.
#[derive(Clone, Default)]
struct InternPool {
    // Clones share weak allocation bookkeeping only. WorldData remains a
    // separate COW value, so transactional updates cannot mutate cloned worlds.
    state: Arc<Mutex<InternPoolState>>,
}

#[derive(Default)]
struct InternPoolState {
    buckets: HashMap<u64, InternBucket>,
    // Round-robin hash queue contains each current bucket once. This avoids a
    // full HashMap sweep during ordinary lookups.
    prune_order: VecDeque<u64>,
    lookups_since_prune: usize,
    bytes_since_prune: usize,
}

struct InternedString {
    weak: Weak<str>,
    // Logical payload length lets the scheduler react to a few large transient
    // strings without dereferencing an expired weak pointer.
    payload_bytes: usize,
}

// A hash normally has one string. Keep that weak entry inline; allocate a
// collision queue only when there are actually two different strings.
#[derive(Default)]
struct InternBucket {
    first: Option<InternedString>,
    rest: Option<Box<VecDeque<InternedString>>>,
}
impl InternBucket {
    fn len(&self) -> usize { usize::from(self.first.is_some()) + self.rest.as_ref().map_or(0, |v| v.len()) }
    fn is_empty(&self) -> bool { self.first.is_none() }
    fn capacity(&self) -> usize { usize::from(self.first.is_some()) + self.rest.as_ref().map_or(0, |v| v.capacity()) }
    fn iter(&self) -> impl Iterator<Item = &InternedString> { self.first.iter().chain(self.rest.iter().flat_map(|v| v.iter())) }
    fn push_back(&mut self, entry: InternedString) {
        if self.first.is_none() { self.first = Some(entry); }
        else { self.rest.get_or_insert_with(|| Box::new(VecDeque::new())).push_back(entry); }
    }
    fn pop_front(&mut self) -> Option<InternedString> {
        let result = self.first.take();
        if let Some(rest) = self.rest.as_mut() { self.first = rest.pop_front(); }
        self.drop_empty_tail();
        result
    }
    fn retain(&mut self, mut keep: impl FnMut(&InternedString) -> bool) {
        if self.first.as_ref().is_some_and(|entry| !keep(entry)) { self.first = None; }
        if let Some(rest) = self.rest.as_mut() {
            rest.retain(&mut keep);
            if self.first.is_none() { self.first = rest.pop_front(); }
        }
        self.drop_empty_tail();
    }
    fn drop_empty_tail(&mut self) {
        if self.rest.as_ref().is_some_and(|rest| rest.is_empty()) { self.rest = None; }
    }
    fn shrink_to_fit(&mut self) {
        if let Some(rest) = self.rest.as_mut() { rest.shrink_to_fit(); }
        self.drop_empty_tail();
    }
    fn heap_bytes(&self) -> usize {
        self.rest.as_ref().map_or(0, |rest| size_of::<VecDeque<InternedString>>() + rest.capacity() * size_of::<InternedString>())
    }
}

impl InternPool {
    fn get(&mut self, value: &str) -> Arc<str> {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        self.get_hashed(value, hasher.finish())
    }

    fn get_hashed(&mut self, value: &str, hash: u64) -> Arc<str> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let found = state.buckets.get_mut(&hash).and_then(|bucket| {
            let mut found = None;
            bucket.retain(|entry| {
                if let Some(existing) = entry.weak.upgrade() {
                    if existing.as_ref() == value {
                        found = Some(existing);
                    }
                    true
                } else {
                    false
                }
            });
            found
        });
        if let Some(found) = found {
            state.maintenance_tick(0);
            return found;
        }

        let is_new_bucket = !state.buckets.contains_key(&hash);
        let value: Arc<str> = Arc::from(value);
        state
            .buckets
            .entry(hash)
            .or_default()
            .push_back(InternedString {
                weak: Arc::downgrade(&value),
                payload_bytes: value.len(),
            });
        if is_new_bucket {
            state.prune_order.push_back(hash);
        }
        state.maintenance_tick(value.len());
        value
    }
}

impl InternPoolState {
    fn maintenance_tick(&mut self, inserted_bytes: usize) {
        self.lookups_since_prune += 1;
        if inserted_bytes > 0 {
            self.bytes_since_prune = self.bytes_since_prune.saturating_add(inserted_bytes);
        }
        if self.lookups_since_prune < INTERN_PRUNE_INTERVAL
            && self.bytes_since_prune < INTERN_PRUNE_BYTES_THRESHOLD
        {
            return;
        }
        self.lookups_since_prune = 0;
        self.bytes_since_prune = 0;

        self.prune_some();
    }

    fn prune_some(&mut self) {
        for _ in 0..INTERN_PRUNE_BUCKETS_PER_TICK {
            let Some(hash) = self.prune_order.pop_front() else {
                break;
            };
            let Some(bucket) = self.buckets.get_mut(&hash) else {
                continue;
            };

            // Rotate through a bounded number of entries so an adversarial or
            // accidental collision bucket cannot turn one lookup into a scan.
            for _ in 0..INTERN_PRUNE_ENTRIES_PER_BUCKET.min(bucket.len()) {
                let entry = bucket.pop_front().expect("bounded by bucket length");
                if entry.weak.strong_count() > 0 {
                    bucket.push_back(entry);
                }
            }
            if bucket.capacity() >= INTERN_SHRINK_MIN_CAPACITY
                && bucket.len() <= INTERN_SHRINK_MAX_LEN
            {
                bucket.shrink_to_fit();
            }
            let remove_bucket = bucket.is_empty();
            if remove_bucket {
                self.buckets.remove(&hash);
            } else {
                self.prune_order.push_back(hash);
            }
        }
        if self.prune_order.capacity() >= INTERN_SHRINK_MIN_CAPACITY
            && self.prune_order.len() <= INTERN_SHRINK_MAX_LEN
        {
            self.prune_order.shrink_to_fit();
        }
        // This runs only when the table is sparse; HashMap::shrink_to_fit may
        // still inspect old capacity, so entry-check limits are not a strict
        // worst-case bound on total maintenance time.
        if self.buckets.capacity() >= INTERN_SHRINK_MIN_CAPACITY
            && self.buckets.len() <= INTERN_SHRINK_MAX_LEN
        {
            self.buckets.shrink_to_fit();
        }
    }

    #[cfg(test)]
    fn weak_stats(&self) -> (usize, usize, usize, usize) {
        let total = self.buckets.values().map(InternBucket::len).sum();
        let expired_entries = self
            .buckets
            .values()
            .flat_map(|bucket| bucket.iter())
            .filter(|entry| entry.weak.strong_count() == 0);
        let mut expired = 0;
        let mut expired_bytes = 0usize;
        for entry in expired_entries {
            expired += 1;
            expired_bytes = expired_bytes.saturating_add(entry.payload_bytes);
        }
        (self.buckets.len(), total, expired, expired_bytes)
    }
}

// Copy-on-write in small chunks: receipt of one node must not copy a full
// world's ordering vector while VM/Actor views retain an earlier revision.
#[derive(Clone, Default)]
struct ReceiptOrder {
    chunks: Vec<Arc<Vec<Option<Arc<str>>>>>,
    len: usize,
}
impl ReceiptOrder {
    const CHUNK: usize = 256;
    fn len(&self) -> usize { self.len }
    fn iter(&self) -> impl Iterator<Item = &Option<Arc<str>>> {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }
    fn push(&mut self, id: Option<Arc<str>>) {
        if self.len % Self::CHUNK == 0 {
            self.chunks.push(Arc::new(Vec::with_capacity(Self::CHUNK)));
        }
        let chunk = Arc::make_mut(self.chunks.last_mut().expect("receipt chunk exists"));
        // Vec::clone preserves length rather than spare capacity. Reserve once
        // so subsequent appends do not repeatedly grow a just-copied chunk.
        if chunk.capacity() < Self::CHUNK { chunk.reserve_exact(Self::CHUNK - chunk.len()); }
        chunk.push(id);
        self.len += 1;
    }
    fn get_mut(&mut self, position: usize) -> Option<&mut Option<Arc<str>>> {
        if position >= self.len { return None; }
        Arc::make_mut(&mut self.chunks[position / Self::CHUNK]).get_mut(position % Self::CHUNK)
    }
    fn storage_bytes(&self) -> usize {
        self.chunks.capacity() * size_of::<Arc<Vec<Option<Arc<str>>>>>()
            + self.chunks.iter().map(|chunk| size_of::<Vec<Option<Arc<str>>>>() + 2 * size_of::<usize>() + chunk.capacity() * size_of::<Option<Arc<str>>>()).sum::<usize>()
    }
}

#[derive(Clone)]
struct WorldData {
    sequence: u64,
    metadata: Arc<BTreeMap<Arc<str>, Arc<Value>>>,
    shards: Vec<Arc<NodeMap>>,
    order: Arc<ReceiptOrder>,
    events: Arc<Vec<Arc<Value>>>,
    node_count: usize,
}

impl WorldData {
    fn new() -> Self {
        Self {
            sequence: 0,
            metadata: Arc::new(BTreeMap::new()),
            shards: (0..SHARD_COUNT).map(|_| Arc::new(NodeMap::default())).collect(),
            order: Arc::new(ReceiptOrder::default()),
            events: Arc::new(Vec::new()),
            node_count: 0,
        }
    }

    fn shard_index(id: &str) -> usize {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        (hasher.finish() as usize) & (SHARD_COUNT - 1)
    }

    fn get(&self, id: &str) -> Option<&Arc<NodeRecord>> {
        self.shards[Self::shard_index(id)].get(id)
    }

    fn insert(&mut self, node: NodeRecord) {
        let index = Self::shard_index(&node.id);
        let existing = self.shards[index].position(&node.id);
        let position = if let Some(position) = existing {
            position
        } else {
            self.node_count += 1;
            let position = self.order.len();
            Arc::make_mut(&mut self.order).push(Some(node.id.clone()));
            position
        };
        Arc::make_mut(&mut self.shards[index]).insert(Arc::new(node), position);
    }

    fn replace_record(&mut self, record: NodeRecord) {
        let index = Self::shard_index(&record.id);
        let position = self.shards[index].position(&record.id).expect("replaced world record exists");
        Arc::make_mut(&mut self.shards[index]).insert(Arc::new(record), position);
    }

    fn remove(&mut self, id: &str) -> bool {
        let index = Self::shard_index(id);
        let removed = Arc::make_mut(&mut self.shards[index]).remove(id);
        let Some((record, position)) = removed else { return false };
        self.node_count = self.node_count.saturating_sub(1);
        if let Some(slot) = Arc::make_mut(&mut self.order).get_mut(position) {
            *slot = None;
        }
        drop(record);
        true
    }

    fn estimated_bytes(&self) -> usize {
        let mut seen_strings: HashSet<*const ()> = HashSet::new();
        let nodes = self
            .shards
            .iter()
            .map(|shard| {
                shard.capacity() * size_of::<(Arc<NodeRecord>, usize)>()
                    + shard
                        .values()
                        .map(|node| {
                            node.estimated_bytes(&mut seen_strings) + size_of::<Arc<NodeRecord>>()
                        })
                        .sum::<usize>()
            })
            .sum::<usize>();
        let metadata = self
            .metadata
            .iter()
            .map(|(key, value)| {
                interned_string_bytes(key, &mut seen_strings)
                    + size_of::<(Arc<str>, Arc<Value>)>()
                    + estimate_value(value)
            })
            .sum::<usize>();
        size_of::<Self>()
            + self.shards.len() * size_of::<Arc<NodeMap>>()
            + self.order.storage_bytes()
            + self.events.capacity() * size_of::<Arc<Value>>()
            + nodes
            + metadata
    }
}

/// Live, receipt-ordered world state with stable node identity.
#[derive(Clone)]
pub struct WorldStore {
    data: Arc<WorldData>,
    pool: InternPool,
}

impl WorldStore {
    /// Read a normalized client-world snapshot, including all top-level metadata.
    pub fn from_snapshot(snapshot: &Value) -> Result<Self, String> {
        let object = snapshot
            .as_object()
            .ok_or("world snapshot must be an object")?;
        if object.get("version").and_then(Value::as_u64) != Some(REQUIRED_VERSION) {
            return Err("unsupported or missing world snapshot version".into());
        }
        let sequence = match object.get("sequence") {
            None => 0,
            Some(value) => value
                .as_u64()
                .ok_or("world snapshot sequence must be an unsigned integer")?,
        };
        let nodes = object
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or("world snapshot nodes must be an array")?;
        if nodes.len() > MAX_NODES {
            return Err("world snapshot exceeds 100000 nodes".into());
        }

        let mut pool = InternPool::default();
        let mut data = WorldData::new();
        data.sequence = sequence;
        let mut metadata = BTreeMap::new();
        for (key, value) in object {
            if key == "nodes" || key == "sequence" {
                continue;
            }
            metadata.insert(pool.get(key), Arc::new(value.clone()));
        }
        data.metadata = Arc::new(metadata);
        for value in nodes {
            let node = NodeRecord::parse(value, &mut pool)?;
            if data.get(&node.id).is_some() {
                return Err(format!("duplicate world node id {:?}", node.id));
            }
            data.insert(node);
        }
        Ok(Self {
            data: Arc::new(data),
            pool,
        })
    }

    /// Apply one normalized receipt batch atomically. Supported actions are
    /// `create`, `set`, and `remove`; unsupported newer action kinds are an
    /// explicit error instead of silently advancing the world cursor.
    pub fn apply_delta(&mut self, delta: &Value) -> Result<(), String> {
        self.apply_delta_with_shared(delta, None)
    }

    /// A session may supply immutable decoder payloads while staging this
    /// receipt. Matching properties retain only their payload Arc; the
    /// resolver itself and its decoder borrow never escape this call.
    pub(crate) fn apply_delta_with_shared(
        &mut self,
        delta: &Value,
        shared: Option<&SharedPayloadResolver<'_>>,
    ) -> Result<(), String> {
        self.apply_delta_internal(delta, shared, &[], &[])
    }

    pub(crate) fn apply_compact_delta(
        &mut self,
        delta: &Value,
        shared: Option<&SharedPayloadResolver<'_>>,
        compact_properties: &[CompactSharedProperty],
    ) -> Result<(), String> {
        self.apply_delta_internal(delta, shared, compact_properties, &[])
    }

    pub(crate) fn apply_compact_delta_with_creates(
        &mut self, delta: &Value, shared: Option<&SharedPayloadResolver<'_>>,
        properties: &[CompactSharedProperty], creates: &[CompactCreatedProperties],
    ) -> Result<(), String> {
        self.apply_delta_internal(delta, shared, properties, creates)
    }

    fn apply_delta_internal(
        &mut self,
        delta: &Value,
        shared: Option<&SharedPayloadResolver<'_>>,
        compact_properties: &[CompactSharedProperty],
        created_properties: &[CompactCreatedProperties],
    ) -> Result<(), String> {
        let envelope = delta.as_object().ok_or("world delta must be an object")?;
        if envelope.get("version").and_then(Value::as_u64) != Some(REQUIRED_VERSION)
            || envelope.get("delta").and_then(Value::as_bool) != Some(true)
        {
            return Err("unsupported world delta envelope".into());
        }
        let target_sequence = envelope
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or("world delta sequence must be an unsigned integer")?;
        let actions = envelope
            .get("actions")
            .and_then(Value::as_array)
            .ok_or("world delta actions must be an array")?;
        if target_sequence < self.data.sequence {
            return Err("world delta sequence moved backwards".into());
        }

        // Stage in a shallow COW copy. No mutation reaches `self` until every
        // action validates, so malformed packets cannot leave partial state.
        let mut next = self.data.as_ref().clone();
        let mut compact_by_action = HashMap::with_capacity(compact_properties.len());
        for binding in compact_properties {
            if compact_by_action
                .insert(
                    (binding.action_index, binding.property.as_str()),
                    (Arc::clone(&binding.payload), binding.force_bytes),
                )
                .is_some()
            {
                return Err("duplicate compact shared-property binding".into());
            }
        }
        let mut creates = HashMap::with_capacity(created_properties.len());
        for binding in created_properties {
            if creates.insert(binding.action_index, binding).is_some() {
                return Err("duplicate compact create binding".into());
            }
        }
        let mut last_action_sequence = self.data.sequence;
        for (action_index, action) in actions.iter().enumerate() {
            let object = action
                .as_object()
                .ok_or("world delta action must be an object")?;
            let sequence = object
                .get("sequence")
                .and_then(Value::as_u64)
                .ok_or("world delta action sequence must be an unsigned integer")?;
            if sequence <= self.data.sequence {
                compact_by_action.retain(|(index, _), _| *index != action_index);
                creates.remove(&action_index);
                continue;
            }
            if sequence <= last_action_sequence || sequence > target_sequence {
                return Err(
                    "world delta action sequence is unordered or beyond its envelope".into(),
                );
            }
            last_action_sequence = sequence;
            let kind = object
                .get("kind")
                .and_then(Value::as_str)
                .ok_or("world delta action kind must be a string")?;
            match kind {
                "create" => {
                    let node_value = object.get("node").ok_or("create action node is missing")?;
                    let mut node =
                        NodeRecord::parse_with_shared(node_value, &mut self.pool, shared)?;
                    if let Some(binding) = creates.remove(&action_index) {
                        let headers = node_value.get("properties").and_then(Value::as_object)
                            .ok_or("compact create properties header must be an object")?;
                        for (key,value) in headers {
                            if binding.properties.get(key).as_ref() != Some(value) {
                                return Err("compact create property header differs from backing".into());
                            }
                        }
                        node.properties = if crate::lightweight::enabled() {
                            PackedProperties::new(&crate::lightweight::filter_map(&binding.properties.unpack()))?
                        } else { binding.properties.as_ref().clone() };
                        node.flags = (node.flags | PROPERTIES_PRESENT) & !PROPERTIES_NULL;
                    }
                    if let Some(properties) =
                        node_value.get("properties").and_then(Value::as_object)
                    {
                        let mut shared_values = Vec::new();
                        for property in properties.keys() {
                            if let Some((payload, force_bytes)) =
                                compact_by_action.remove(&(action_index, property.as_str()))
                            {
                                if crate::lightweight::omitted(property) { continue; }
                                shared_values.push(SharedPropertyValue {
                                    key: property.clone(),
                                    payload,
                                    force_bytes,
                                });
                            }
                        }
                        node.set_shared_properties(shared_values)?;
                    }
                    node.created_sequence = sequence;
                    node.flags |= CREATED_SEQUENCE_PRESENT;
                    let action_id = match object.get("id") {
                        Some(value) => {
                            let id = required_string(Some(value), "create id")?;
                            if node.id.as_ref() != id {
                                return Err("create action id differs from node id".into());
                            }
                            id
                        }
                        None => node.id.to_string(),
                    };
                    if let Some(existing) = next.get(&action_id) {
                        if existing.class_name != node.class_name {
                            return Err(format!("replicated instance {action_id:?} changed class"));
                        }
                    }
                    let attributes = crate::received_attributes::creation(
                        node.property("AttributesReplicate").as_ref(),
                        next.get(&action_id).is_none()
                            && node_value.get("attributes_known").is_none()
                            && node_value["received_creation_complete"] == Value::Bool(true),
                    );
                    let tags = crate::received_tags::creation(
                        node.property("Tags").as_ref(),
                        next.get(&action_id).is_none()
                            && node_value.get("collection_tags_known").is_none()
                            && node_value.get("collection_tags").is_none()
                            && node_value["received_creation_complete"] == Value::Bool(true),
                    );
                    if !attributes.is_empty() || !tags.is_empty() {
                        let mut metadata = node.metadata.unpack();
                        metadata.extend(attributes);
                        metadata.extend(tags);
                        node.metadata = PackedMetadata::new(&metadata)?;
                    }
                    next.insert(node);
                    if next.node_count > MAX_NODES {
                        return Err("world delta exceeds 100000 nodes".into());
                    }
                }
                "set" => {
                    let id = required_string(object.get("id"), "set id")?;
                    let fields = object
                        .get("fields")
                        .and_then(Value::as_object)
                        .ok_or("set action fields must be an object")?;
                    let Some(record) = next.get(&id).cloned() else {
                        compact_by_action.retain(|(index, _), _| *index != action_index);
                        continue;
                    };
                    let mut changed = record.as_ref().clone();
                    for (key, value) in fields {
                        if crate::lightweight::omitted(key) {
                            compact_by_action.remove(&(action_index,key.as_str()));
                            continue;
                        }
                        match key.as_str() {
                            "Parent" => {
                                changed.set_parent(parse_parent_slot(value, &mut self.pool)?)
                            }
                            "Name" => changed.set_name(parse_name_slot(value, &mut self.pool)?),
                            _ => {
                                if let Some((payload, force_bytes)) =
                                    compact_by_action.remove(&(action_index, key.as_str()))
                                {
                                    changed.set_shared_property(key, payload, force_bytes)?;
                                } else {
                                    changed.set_property_ref_with_shared(key, value, shared)?;
                                }
                            }
                        }
                    }
                    // Motion and mass projections need input revisions to
                    // expire old observations. Stamp only these dependencies;
                    // unrelated properties need no new per-field metadata.
                    let revision_keys: Vec<_> = fields.keys().filter(|key| matches!(key.as_str(),
                        "CFrame" | "ReplicationPV" | "Position" | "Orientation" | "Rotation"
                        | "AssemblyLinearVelocity" | "Velocity" | "AssemblyAngularVelocity" | "RotVelocity"
                        | "Mass" | "Size" | "size" | "Shape" | "Material" | "MaterialVariant"
                        | "CustomPhysicalProperties" | "CurrentPhysicalProperties"
                        | "CenterOfMass" | "AssemblyMass" | "AssemblyCenterOfMass"
                        | "Anchored" | "Massless" | "RootPriority" | "Name" | "WalkSpeed" | "Gravity"
                        | "Parent" | "NetworkOwnerV3" | "PhysicsRepRootPart" | "PhysicsRepRootRef" | "AssemblyRootPart"
                    )).collect();
                    if object.contains_key("attribute_snapshot")
                        || fields.contains_key("AttributesReplicate")
                        || fields.contains_key("Tags")
                        || object.contains_key("tag_snapshot")
                        || !revision_keys.is_empty()
                    {
                        let mut metadata = changed.metadata.unpack();
                        if fields.contains_key("AttributesReplicate") {
                            metadata.extend(crate::received_attributes::snapshot(
                                &changed.property("AttributesReplicate").unwrap_or(Value::Null)));
                        }
                        if fields.contains_key("Tags") {
                            metadata.extend(crate::received_tags::snapshot(
                                &changed.property("Tags").unwrap_or(Value::Null)));
                        }
                        if let Some(value) = object.get("attribute_snapshot") {
                            let fields = value
                                .as_object()
                                .ok_or("attribute snapshot must be an object")?;
                            for (key, value) in fields {
                                metadata.insert(key.clone(), value.clone());
                            }
                        }
                        if let Some(value) = object.get("tag_snapshot") {
                            let fields =
                                value.as_object().ok_or("tag snapshot must be an object")?;
                            for (key, value) in fields {
                                metadata.insert(key.clone(), value.clone());
                            }
                        }
                        if !revision_keys.is_empty() {
                            let revisions = metadata.entry("property_revisions")
                                .or_insert_with(|| Value::Object(Map::new()));
                            if revisions.is_null() { *revisions = Value::Object(Map::new()); }
                            let revisions = revisions.as_object_mut()
                                .ok_or("property_revisions must be an object")?;
                            for key in revision_keys {
                                revisions.insert(key.clone(), Value::from(sequence));
                            }
                        }
                        changed.metadata = PackedMetadata::new(&metadata)?;
                    }
                    next.replace_record(changed);
                }
                "remove" => {
                    let id = required_string(object.get("id"), "remove id")?;
                    remove_subtree(&mut next, &id);
                }
                "attribute" => {
                    let id = required_string(object.get("id"), "attribute id")?;
                    let update = object
                        .get("attribute_update")
                        .and_then(Value::as_object)
                        .ok_or("attribute update must be an object")?;
                    let name = required_string(update.get("name"), "attribute name")?;
                    let known = update
                        .get("known")
                        .and_then(Value::as_bool)
                        .ok_or("attribute known flag must be a boolean")?;
                    if let Some(record) = next.get(&id).cloned() {
                        let mut changed = record.as_ref().clone();
                        let mut metadata = changed.metadata.unpack();
                        let mut attributes = metadata
                            .get("attributes")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        let was_known = metadata
                            .get("attributes_known")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if known {
                            let value = update.get("value").ok_or("known attribute lacks value")?;
                            if value.is_null() && was_known {
                                attributes.remove(&name);
                            } else {
                                attributes.insert(name, value.clone());
                            }
                        } else {
                            attributes.remove(&name);
                        }
                        metadata.insert("attributes".into(), Value::Object(attributes));
                        metadata.insert("attributes_known".into(), Value::Bool(was_known && known));
                        changed.metadata = PackedMetadata::new(&metadata)?;
                        next.replace_record(changed);
                    }
                }
                "event" => {
                    if next.events.len() >= MAX_PENDING_EVENTS {
                        return Err("pending world event queue is full".into());
                    }
                    Arc::make_mut(&mut next.events).push(Arc::new(action.clone()));
                }
                _ => return Err(format!("unsupported world delta action kind {kind:?}")),
            }
        }
        if !compact_by_action.is_empty() {
            return Err("compact shared-property binding did not match an action field".into());
        }
        if !creates.is_empty() {
            return Err("compact create binding did not match an action".into());
        }
        next.sequence = target_sequence;
        self.data = Arc::new(next);
        self.prune_interns();
        Ok(())
    }

    /// Serialize a normalized snapshot. This is an allocation-heavy boundary
    /// operation; Actor simulation should use `view()` instead.
    pub fn to_snapshot(&self) -> Value {
        data_to_snapshot(&self.data)
    }

    /// Create an Actor-local view in O(1), sharing the immutable base world.
    pub fn view(&self) -> WorldView {
        WorldView {
            base: self.data.clone(),
            overlay: HashMap::new(),
            removed: HashSet::new(),
            new_order: Vec::new(),
            sequence: self.data.sequence,
        }
    }

    pub fn sequence(&self) -> u64 {
        self.data.sequence
    }
    pub fn len(&self) -> usize {
        self.data.node_count
    }
    pub fn is_empty(&self) -> bool {
        self.data.node_count == 0
    }
    pub fn contains(&self, id: &str) -> bool {
        self.data.get(id).is_some()
    }
    pub fn get_property(&self, id: &str, key: &str) -> Option<Value> {
        self.data.get(id)?.property(key)
    }
    /// Events received in this or earlier batches, in receipt order. Call
    /// `acknowledge_events` only after the engine has queued them for dispatch.
    pub fn pending_events(&self) -> &[Arc<Value>] {
        &self.data.events
    }

    pub fn acknowledge_events(&mut self, through_sequence: u64) {
        if self.data.events.iter().any(|event| {
            event
                .get("sequence")
                .and_then(Value::as_u64)
                .is_some_and(|sequence| sequence <= through_sequence)
        }) {
            let data = Arc::make_mut(&mut self.data);
            Arc::make_mut(&mut data.events).retain(|event| {
                event
                    .get("sequence")
                    .and_then(Value::as_u64)
                    .is_none_or(|sequence| sequence > through_sequence)
            });
        }
    }

    /// Estimated bytes owned by the canonical store. This is not RSS and
    /// excludes allocator bookkeeping/capacity; see `shared_memory_usage` for
    /// accounting across Actor views.
    pub fn retained_bytes(&self) -> usize {
        self.data.estimated_bytes() + self.pool.estimated_bytes()
    }

    /// Allocation estimates for profiling; field names only, never values.
    pub fn storage_profile(&self) -> Value {
        use blake2::{Blake2b512, Digest};
        let mut digest = Blake2b512::new();
        digest.update(self.data.sequence.to_le_bytes());
        for id in self.data.order.iter().flatten() {
            if let Some(node) = self.data.get(id) {
                let bytes = serde_json::to_vec(&node.to_json()).expect("JSON node serializes");
                digest.update((bytes.len() as u64).to_le_bytes());
                digest.update(&bytes);
            }
        }
        let mut properties: BTreeMap<String, usize> = BTreeMap::new();
        let mut metadata_bytes = 0usize;
        let mut shared_property_count = 0usize;
        let mut shared_property_bytes = 0usize;
        let mut shared_property_stored_bytes = 0usize;
        let mut shared_payloads = HashSet::new();
        let mut packed_metadata_reference_count = 0usize;
        let mut metadata_backings = HashSet::new();
        let mut metadata_backing_reference_count = 0usize;
        let mut metadata_unique_backing_bytes = 0usize;
        let mut deep_metadata_node_count = 0usize;
        for shard in &self.data.shards {
            for node in shard.values() {
                for (name, value) in node.properties.unpack() {
                    *properties.entry(name).or_default() += estimate_value(&value);
                }
                node.properties.for_each_shared_payload(|payload| {
                    shared_property_count += 1;
                    if shared_payloads.insert(Arc::as_ptr(payload)) {
                        shared_property_bytes += payload.len();
                        shared_property_stored_bytes += payload.stored_bytes();
                    }
                });
                metadata_bytes += node.metadata.stored_bytes();
                if let Some(backing) = node.metadata.packed_backing() {
                    packed_metadata_reference_count += 1;
                    if Arc::strong_count(backing) > 1 {
                        metadata_backing_reference_count += 1;
                    }
                    if metadata_backings.insert(Arc::as_ptr(backing) as usize) {
                        metadata_unique_backing_bytes += backing.stored_bytes();
                    }
                } else {
                    deep_metadata_node_count += 1;
                }
            }
        }
        serde_json::json!({"estimated_world_bytes":self.retained_bytes(),
            "world_state_blake2b512":format!("{:x}",digest.finalize()),
            "estimated_property_bytes_by_name":properties,"estimated_node_metadata_bytes":metadata_bytes,
            "shared_payload_backed_property_count":shared_property_count,
            "shared_property_unique_payload_bytes":shared_property_bytes,
            "shared_property_unique_stored_bytes":shared_property_stored_bytes,
            "packed_metadata_reference_count":packed_metadata_reference_count,
            "metadata_backing_reference_count":metadata_backing_reference_count,
            "metadata_unique_backing_count":metadata_backings.len(),
            "metadata_duplicate_backing_references":packed_metadata_reference_count.saturating_sub(metadata_backings.len()),
            "metadata_unique_backing_stored_bytes":metadata_unique_backing_bytes,
            "deep_metadata_node_count":deep_metadata_node_count,
            "layout_packed_properties_bytes":size_of::<PackedProperties>(),
            "layout_packed_value_bytes":packed_value_layout_bytes(),
            "layout_packed_metadata_bytes":size_of::<PackedMetadata>(),
            "layout_node_record_bytes":size_of::<NodeRecord>(),
            "layout_metadata_backing_arc_bytes":size_of::<Arc<MetadataBacking>>(),
            "metadata_intern_max_canonical_bytes":MAX_INTERNED_METADATA_BYTES})
    }

    /// Report approximate shared-base and per-view overlay bytes. The shared
    /// immutable base is counted once for this family of views.
    pub fn shared_memory_usage(&self, views: &[WorldView]) -> MemoryUsage {
        let mut revisions = HashSet::new();
        revisions.insert(Arc::as_ptr(&self.data));
        let mut base_bytes = self.data.estimated_bytes();
        for view in views {
            if revisions.insert(Arc::as_ptr(&view.base)) {
                // Distinct versions may share individual shards/records. Count
                // each whole estimate to avoid undercounting; this is a
                // conservative upper bound across retained revisions.
                base_bytes += view.base.estimated_bytes();
            }
        }
        let intern_bytes = self.pool.estimated_bytes();
        let overlay_bytes = views.iter().map(WorldView::overlay_bytes).sum();
        MemoryUsage {
            shared_base_bytes: base_bytes + intern_bytes,
            overlay_bytes,
            view_count: views.len(),
            total_estimated_bytes: base_bytes + intern_bytes + overlay_bytes,
        }
    }

    fn prune_interns(&mut self) {
        // The successful-commit hook also advances bounded cleanup when a
        // delta did not intern any strings. This preserves progress after
        // earlier rejected staging without sweeping the whole interner.
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // `prune_some` owns queue/map coordination and caps weak-entry
        // checks; never mutate buckets directly here.
        state.prune_some();
    }
}

/// each actor gets its own edits on top of a shared base.
/// cloning a view just copies Arc handles; an edit copies the node it touches.
#[derive(Clone)]
pub struct WorldView {
    base: Arc<WorldData>,
    overlay: HashMap<Arc<str>, Arc<NodeRecord>>,
    removed: HashSet<Arc<str>>,
    new_order: Vec<Arc<str>>,
    sequence: u64,
}

impl WorldView {
    fn node(&self, id: &str) -> Option<&NodeRecord> {
        if self.removed.contains(id) {
            return None;
        }
        self.overlay
            .get(id)
            .map(Arc::as_ref)
            .or_else(|| self.base.get(id).map(Arc::as_ref))
    }

    pub fn contains(&self, id: &str) -> bool {
        self.node(id).is_some()
    }

    /// Return an immutable handle to one node record. The handle owns an Arc
    /// to that record only, so a consumer can keep it across a world update
    /// without pinning the complete old world revision.
    pub fn read_only_node(&self, id: &str) -> Option<ReadOnlyNode> {
        if self.removed.contains(id) {
            return None;
        }
        self.overlay
            .get(id)
            .cloned()
            .or_else(|| self.base.get(id).cloned())
            .map(ReadOnlyNode)
    }

    /// Visit nodes in canonical snapshot order without allocating an ID or
    /// JSON array. Handles pin one node record each and do not retain this view.
    pub fn for_each_node(&self, mut visit: impl FnMut(ReadOnlyNode)) {
        for id in self.base.order.iter().flatten() {
            if !self.removed.contains(id.as_ref()) {
                if let Some(node) = self
                    .overlay
                    .get(id)
                    .cloned()
                    .or_else(|| self.base.get(id).cloned())
                {
                    visit(ReadOnlyNode(node));
                }
            }
        }
        for id in &self.new_order {
            if !self.removed.contains(id.as_ref()) && self.base.get(id).is_none() {
                if let Some(node) = self.overlay.get(id).cloned() {
                    visit(ReadOnlyNode(node));
                }
            }
        }
    }
    /// Return direct children in canonical world order, including view overlays.
    pub fn children_of(&self, parent: &str) -> Vec<ReadOnlyNode> {
        let mut children = Vec::new();
        self.for_each_node(|node| {
            if self.parent(node.id()) == Some(parent) {
                children.push(node);
            }
        });
        children
    }

    /// Class name from the received node record, if the instance is present.
    pub fn class_name(&self, id: &str) -> Option<&str> {
        self.node(id).map(|node| node.class_name.as_ref())
    }
    pub fn get_property(&self, id: &str, key: &str) -> Option<Value> {
        self.node(id)?.property(key)
    }
    pub fn name(&self, id: &str) -> Option<&str> {
        self.node(id)?.name.as_deref()
    }
    pub fn parent(&self, id: &str) -> Option<&str> {
        self.node(id)?.parent_id()
    }
    pub fn node_count(&self) -> usize {
        self.base.node_count
            + self
                .new_order
                .iter()
                .filter(|id| !self.removed.contains(id.as_ref()))
                .count()
            - self
                .removed
                .iter()
                .filter(|id| self.base.get(id.as_ref()).is_some())
                .count()
    }

    pub fn set_property(&mut self, id: &str, key: &str, value: Value) -> Result<(), String> {
        if key == "Parent" {
            return self.reparent_value(id, &value);
        }
        if key == "Name" {
            let name = parse_name_slot(&value, &mut InternPool::default())?;
            let mut record = self.node(id).ok_or("view node not found")?.clone();
            record.set_name(name);
            self.overlay.insert(record.id.clone(), Arc::new(record));
            return Ok(());
        }
        let mut record = self.node(id).ok_or("view node not found")?.clone();
        record.set_property(key, value)?;
        self.overlay.insert(record.id.clone(), Arc::new(record));
        Ok(())
    }

    fn reparent_value(&mut self, id: &str, value: &Value) -> Result<(), String> {
        let parent = parse_parent_slot(value, &mut InternPool::default())?;
        let mut record = self.node(id).ok_or("view node not found")?.clone();
        record.set_parent(parent);
        self.overlay.insert(record.id.clone(), Arc::new(record));
        Ok(())
    }

    pub fn reparent(&mut self, id: &str, parent: Option<&str>) -> Result<(), String> {
        let parent = parent.map_or(Slot::Null, |value| Slot::Value(Arc::from(value)));
        let mut record = self.node(id).ok_or("view node not found")?.clone();
        record.set_parent(parent);
        self.overlay.insert(record.id.clone(), Arc::new(record));
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        if self.node(id).is_none() {
            return Ok(());
        }
        let mut children: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();
        for candidate in self.ids() {
            if let Some(parent) = self
                .node(candidate.as_ref())
                .and_then(NodeRecord::parent_id)
            {
                children
                    .entry(Arc::from(parent))
                    .or_default()
                    .push(candidate);
            }
        }
        let mut pending = vec![Arc::<str>::from(id)];
        let mut seen = HashSet::new();
        let mut index = 0;
        while index < pending.len() {
            let current = pending[index].clone();
            if seen.insert(current.clone()) {
                if let Some(descendants) = children.get(&current) {
                    pending.extend(descendants.iter().cloned());
                }
            }
            index += 1;
        }
        for id in seen {
            self.overlay.remove(id.as_ref());
            self.removed.insert(id);
        }
        Ok(())
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn overlay_len(&self) -> usize {
        self.overlay.len() + self.removed.len()
    }
    pub fn overlay_bytes(&self) -> usize {
        let mut seen_strings: HashSet<*const ()> = HashSet::new();
        for shard in &self.base.shards {
            for node in shard.values() {
                collect_strings(node, &mut seen_strings);
            }
        }
        self.overlay
            .values()
            .map(|node| node.estimated_bytes(&mut seen_strings))
            .sum::<usize>()
            + self
                .removed
                .iter()
                .map(|id| id.len() + size_of::<Arc<str>>())
                .sum::<usize>()
            + self.new_order.len() * size_of::<Arc<str>>()
    }

    pub fn to_snapshot(&self) -> Value {
        let mut result = data_to_snapshot(&self.base);
        let Some(object) = result.as_object_mut() else {
            return result;
        };
        if let Some(nodes) = object.get_mut("nodes").and_then(Value::as_array_mut) {
            nodes.retain(|node| {
                node.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !self.removed.contains(id))
            });
            for node in nodes.iter_mut() {
                if let Some(id) = node.get("id").and_then(Value::as_str) {
                    if let Some(overlay) = self.overlay.get(id) {
                        *node = overlay.to_json();
                    }
                }
            }
            for id in &self.new_order {
                if !self.removed.contains(id.as_ref()) && self.base.get(id).is_none() {
                    if let Some(node) = self.overlay.get(id) {
                        nodes.push(node.to_json());
                    }
                }
            }
        }
        if let Some(sequence) = object.get_mut("sequence") {
            *sequence = Value::Number(Number::from(self.sequence));
        }
        result
    }

    fn ids(&self) -> Vec<Arc<str>> {
        let mut result = Vec::with_capacity(self.base.node_count + self.overlay.len());
        for shard in &self.base.shards {
            result.extend(shard.keys().cloned());
        }
        result.extend(self.new_order.iter().cloned());
        result
    }
}

/// Distinguishes a missing field from an explicit JSON null in a received
/// immutable node record.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadOnlyField<T> {
    Missing,
    Null,
    Value(T),
}

/// a read-only handle to one packed node. cloning it keeps that record,
/// its strings and its property buffers alive, without pinning the whole world.
#[derive(Clone, Debug)]
pub struct ReadOnlyNode(Arc<NodeRecord>);

impl ReadOnlyNode {
    /// Share the immutable identity without allocating another string.
    pub fn shared_id(&self) -> Arc<str> { self.0.id.clone() }

    pub fn id(&self) -> &str {
        &self.0.id
    }

    pub fn class_name(&self) -> &str {
        &self.0.class_name
    }

    /// Receipt sequence of the create action, or None for initial snapshot nodes.
    pub fn created_sequence(&self) -> Option<u64> {
        (self.0.flags & CREATED_SEQUENCE_PRESENT != 0).then_some(self.0.created_sequence)
    }

    pub fn name(&self) -> ReadOnlyField<&str> {
        match self.0.name_slot() {
            Slot::Missing => ReadOnlyField::Missing,
            Slot::Null => ReadOnlyField::Null,
            Slot::Value(value) => ReadOnlyField::Value(value),
        }
    }

    pub fn parent(&self) -> ReadOnlyField<&str> {
        match self.0.parent_slot() {
            Slot::Missing => ReadOnlyField::Missing,
            Slot::Null => ReadOnlyField::Null,
            Slot::Value(value) => ReadOnlyField::Value(value),
        }
    }

    pub fn properties(&self) -> ReadOnlyField<Map<String, Value>> {
        if self.0.flags & PROPERTIES_PRESENT == 0 {
            ReadOnlyField::Missing
        } else if self.0.flags & PROPERTIES_NULL != 0 {
            ReadOnlyField::Null
        } else {
            ReadOnlyField::Value(self.0.properties.unpack())
        }
    }

    pub fn properties_len(&self) -> ReadOnlyField<usize> {
        if self.0.flags & PROPERTIES_PRESENT == 0 {
            ReadOnlyField::Missing
        } else if self.0.flags & PROPERTIES_NULL != 0 {
            ReadOnlyField::Null
        } else {
            ReadOnlyField::Value(self.0.properties.len())
        }
    }

    /// A present property containing JSON null returns `Some(Value::Null)`;
    /// an absent property returns `None`.
    pub fn property(&self, key: &str) -> Option<Value> {
        self.0.property(key)
    }

    /// Return a node metadata object such as `origins`, `attributes`, or
    /// `property_revisions`, preserving a missing field versus JSON null.
    pub fn metadata_section(&self, section: &str) -> ReadOnlyField<Value> {
        match self.0.metadata.get(section) {
            None => ReadOnlyField::Missing,
            Some(Value::Null) => ReadOnlyField::Null,
            Some(value) => ReadOnlyField::Value(value),
        }
    }

    /// Read one entry from an object-valued metadata section. Explicit null
    /// entries stay `Some(Value::Null)` while missing entries stay `None`.
    pub fn metadata_field(&self, section: &str, key: &str) -> Option<Value> {
        self.0.metadata_field(section, key)
    }

    /// Read a top-level opaque metadata value without materializing other fields.
    pub fn record_field(&self, key: &str) -> Option<Value> {
        self.0.metadata.get(key)
    }
}

/// Approximate storage accounting for a canonical store and its views.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    pub shared_base_bytes: usize,
    pub overlay_bytes: usize,
    pub view_count: usize,
    pub total_estimated_bytes: usize,
}

fn parse_name_slot(value: &Value, pool: &mut InternPool) -> Result<Slot<Arc<str>>, String> {
    match value {
        Value::Null => Ok(Slot::Null),
        Value::String(name) => Ok(Slot::Value(pool.get(name))),
        _ => Err("Name must be a string or null".into()),
    }
}

fn parse_parent_slot(value: &Value, pool: &mut InternPool) -> Result<Slot<Arc<str>>, String> {
    match value {
        Value::Null => Ok(Slot::Null),
        Value::String(id) => Ok(Slot::Value(pool.get(id))),
        Value::Object(object) if object.len() == 1 => object
            .get("$ref")
            .and_then(Value::as_str)
            .map(|id| Slot::Value(pool.get(id)))
            .ok_or("Parent reference must contain a string $ref".into()),
        _ => Err("Parent must be a string, a $ref object, or null".into()),
    }
}

fn required_string(value: Option<&Value>, label: &str) -> Result<String, String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{label} must be a nonempty string"))
}

fn remove_subtree(data: &mut WorldData, root: &str) {
    if data.get(root).is_none() {
        return;
    }
    let mut children: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();
    for shard in &data.shards {
        for node in shard.values() {
            if let Some(parent) = node.parent_id() {
                children
                    .entry(Arc::from(parent))
                    .or_default()
                    .push(node.id.clone());
            }
        }
    }
    let mut pending = vec![root.to_owned()];
    let mut seen = HashSet::new();
    let mut cursor = 0;
    while cursor < pending.len() {
        let parent = pending[cursor].clone();
        cursor += 1;
        if !seen.insert(parent.clone()) {
            continue;
        }
        if let Some(descendants) = children.get(parent.as_str()) {
            pending.extend(
                descendants
                    .iter()
                    .filter(|id| !seen.contains(id.as_ref()))
                    .map(|id| id.to_string()),
            );
        }
    }
    // Reverse order so children are detached before their ancestors.
    pending.reverse();
    for id in seen {
        data.remove(&id);
    }
}

fn data_to_snapshot(data: &WorldData) -> Value {
    let mut object = Map::new();
    for (key, value) in data.metadata.iter() {
        object.insert(key.to_string(), value.as_ref().clone());
    }
    let mut nodes = Vec::with_capacity(data.node_count);
    for id in data.order.iter().flatten() {
        if let Some(node) = data.get(id) {
            nodes.push(node.to_json());
        }
    }
    object.insert("nodes".into(), Value::Array(nodes));
    object.insert(
        "sequence".into(),
        Value::Number(Number::from(data.sequence)),
    );
    Value::Object(object)
}

fn estimate_value(value: &Value) -> usize {
    match value {
        Value::Null => size_of::<Value>(),
        Value::Bool(_) => size_of::<Value>(),
        Value::Number(_) => size_of::<Value>() + 8,
        Value::String(value) => size_of::<Value>() + value.len(),
        Value::Array(values) => {
            size_of::<Value>() + values.iter().map(estimate_value).sum::<usize>()
        }
        Value::Object(values) => {
            size_of::<Value>()
                + values
                    .iter()
                    .map(|(key, value)| {
                        key.len() + size_of::<(String, Value)>() + estimate_value(value)
                    })
                    .sum::<usize>()
        }
    }
}

fn collect_strings(node: &NodeRecord, seen: &mut HashSet<*const ()>) {
    seen.insert(Arc::as_ptr(&node.id) as *const ());
    seen.insert(Arc::as_ptr(&node.class_name) as *const ());
    if let Some(value) = &node.name {
        seen.insert(Arc::as_ptr(value) as *const ());
    }
    if let Some(value) = &node.parent {
        seen.insert(Arc::as_ptr(value) as *const ());
    }
    // Metadata keys now live in the compact per-node packed buffer, not the
    // world-wide string interner.
}

impl InternPool {
    fn estimated_bytes(&self) -> usize {
        // Weak entries retain their Arc allocation until pruned, so include
        // expired string payload bytes. Live string bytes are counted by nodes.
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.buckets.values().map(|bucket| {
            bucket.heap_bytes() + bucket.iter().filter(|entry| entry.weak.strong_count() == 0)
                .map(|entry| entry.payload_bytes).sum::<usize>()
        }).sum::<usize>()
            + state.buckets.capacity() * size_of::<(u64, InternBucket)>()
            + state.prune_order.capacity() * size_of::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/world_snapshot.json")).unwrap()
    }

    #[test]
    fn receipt_order_keeps_old_views_stable_across_chunks_and_recreation() {
        let nodes: Vec<Value> = (0..1025).map(|i| serde_json::json!({"id":format!("item-{i}"),"class_name":"Folder","name":format!("Item {i}"),"parent":null,"properties":{}})).collect();
        let mut store = WorldStore::from_snapshot(&serde_json::json!({"version":1,"sequence":0,"nodes":nodes})).unwrap();
        let old = store.view();
        let original = old.to_snapshot();
        store.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":4,"actions":[
            {"kind":"remove","sequence":1,"id":"item-255"},
            {"kind":"remove","sequence":2,"id":"item-256"},
            {"kind":"create","sequence":3,"node":{"id":"item-255","class_name":"Folder","name":"recreated","parent":null,"properties":{}}},
            {"kind":"set","sequence":4,"id":"item-1024","fields":{"Name":"last"}}
        ]})).unwrap();
        let current = store.to_snapshot();
        let ids: Vec<&str> = current["nodes"].as_array().unwrap().iter().map(|n|n["id"].as_str().unwrap()).collect();
        let mut expected: Vec<String> = (0..1025).filter(|i|*i!=255 && *i!=256).map(|i|format!("item-{i}")).collect();
        expected.push("item-255".into());
        assert_eq!(ids, expected.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(old.to_snapshot(), original);
        assert_eq!(store.view().name("item-1024"), Some("last"));
        assert_eq!(store.view().name("item-255"), Some("recreated"));
        // A staged edit followed by an invalid action must not leak into either revision.
        let before = store.to_snapshot();
        assert!(store.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":6,"actions":[
            {"kind":"remove","sequence":5,"id":"item-512"},
            {"kind":"unsupported","sequence":6}
        ]})).is_err());
        assert_eq!(store.to_snapshot(), before);
        assert_eq!(old.to_snapshot(), original);
    }

    #[test]
    fn lightweight_mode_filters_only_received_asset_fields_in_snapshots_and_deltas() {
        use serde_json::json;
        let light = crate::lightweight::enabled();
        let asset = SharedPayload::from_raw("asset-data-".repeat(100).into_bytes());
        let mut store = WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[{
            "id":"part","class_name":"MeshPart","name":"Coin","parent":null,
            "properties":{"MeshId":"mesh-before","Anchored":true,"Source":"return 1"},
            "origins":{"MeshId":"received","Anchored":"received"},
            "property_revisions":{"MeshId":0,"Anchored":0},
            "attributes":{"MeshId":"user-attribute"}
        }]})).unwrap();
        let before = store.view();
        let part = before.read_only_node("part").unwrap();
        assert_eq!(part.property("MeshId"), (!light).then(||json!("mesh-before")));
        assert_eq!(part.property("Anchored"), Some(json!(true)));
        assert_eq!(part.metadata_field("attributes","MeshId"), Some(json!("user-attribute")));
        assert_eq!(part.metadata_field("origins","MeshId"), (!light).then(||json!("received")));
        let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"create","sequence":1,"node":{"id":"sound","class_name":"Sound",
                "name":"Ambient","parent":"part","properties":{"SoundId":null,"Volume":0.5}}},
            {"kind":"set","sequence":2,"id":"part","fields":{"MeshId":null,"Anchored":false}}
        ]});
        let bindings = [
            CompactSharedProperty { action_index:0,property:"SoundId".into(),payload:asset.clone(),force_bytes:false },
            CompactSharedProperty { action_index:1,property:"MeshId".into(),payload:asset.clone(),force_bytes:false },
        ];
        store.apply_compact_delta(&delta,None,&bindings).unwrap();
        let after = store.view();
        let part = after.read_only_node("part").unwrap();
        let sound = after.read_only_node("sound").unwrap();
        let expected = (!light).then(||json!("asset-data-".repeat(100)));
        assert_eq!(part.property("MeshId"),expected);
        assert_eq!(sound.property("SoundId"),expected);
        assert_eq!(part.property("Anchored"),Some(json!(false)));
        assert_eq!(part.property("Source"),Some(json!("return 1")));
        assert_eq!(sound.property("Volume"),Some(json!(0.5)));
        assert_eq!(sound.parent(),ReadOnlyField::Value("part"));
        assert_eq!(before.read_only_node("part").unwrap().property("Anchored"),Some(json!(true)));
    }

    #[test]
    fn string_interner_periodically_reclaims_unique_transient_values() {
        let mut pool = InternPool::default();
        for index in 0..2048 {
            drop(pool.get(&format!("transient-{index}")));
        }
        let state = pool.state.lock().unwrap();
        let (buckets, entries, expired, expired_bytes) = state.weak_stats();
        assert!(
            expired <= 1,
            "periodic cleanup left {expired} expired entries"
        );
        assert!(entries <= 1, "periodic cleanup left {entries} weak entries");
        assert!(buckets <= 1, "periodic cleanup left {buckets} weak buckets");
        assert!(
            expired_bytes <= 32,
            "final transient entry retained {expired_bytes} bytes"
        );
    }

    #[test]
    fn repeated_rejected_creates_do_not_accumulate_expired_interned_strings() {
        use serde_json::json;

        let snapshot = json!({"version":1,"sequence":0,"nodes":[]});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        for index in 0..256 {
            let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
                {"kind":"create","sequence":1,"node":{
                    "id":format!("rejected-id-{index}"),
                    "class_name":"TransientClass",
                    "name":format!("transient-name-{index}"),
                    "properties":{}
                }},
                {"kind":"unsupported","sequence":2}
            ]});
            assert!(store.apply_delta(&delta).is_err());
        }
        assert_eq!(store.to_snapshot(), snapshot);
        let (buckets, entries, expired, expired_bytes) =
            store.pool.state.lock().unwrap().weak_stats();
        assert!(
            expired <= 16,
            "rejected creates left {expired} expired strings"
        );
        assert!(
            entries <= 16,
            "rejected creates left {entries} weak entries"
        );
        assert!(
            buckets <= 16,
            "rejected creates left {buckets} weak buckets"
        );
        assert!(
            expired_bytes <= 16 * 32,
            "rejected creates retained {expired_bytes} logical string bytes"
        );
    }

    #[test]
    fn successful_commit_advances_cleanup_without_interning_strings() {
        use serde_json::json;

        let mut store = WorldStore::from_snapshot(&json!({
            "version": 1,
            "sequence": 0,
            "nodes": []
        }))
        .unwrap();
        let live_before = store.pool.state.lock().unwrap().weak_stats();
        drop(store.pool.get("expired-before-successful-event"));
        assert_eq!(store.pool.state.lock().unwrap().weak_stats().2, 1);

        // This event action does not call the string interner. The successful
        // commit hook must nevertheless advance a bounded cleanup pass.
        store
            .apply_delta(&json!({"version":1,"delta":true,"sequence":1,"actions":[
                {"kind":"event","sequence":1,"name":"Tick","arguments":[]}
            ]}))
            .unwrap();
        assert_eq!(store.sequence(), 1);
        assert_eq!(store.pool.state.lock().unwrap().weak_stats(), live_before);
    }

    #[test]
    fn string_interner_byte_pressure_reclaims_large_transient_backing() {
        let mut pool = InternPool::default();
        let large = "z".repeat(256 * 1024);
        drop(pool.get(&large));
        let before = pool.state.lock().unwrap().weak_stats();
        assert!(before.3 >= large.len());

        // Lookup-count maintenance continues after the bytes-triggered sweep
        // saw the newly returned Arc still alive.
        for index in 0..INTERN_PRUNE_INTERVAL {
            drop(pool.get(&format!("maintenance-{index}")));
        }
        let after = pool.state.lock().unwrap().weak_stats();
        assert!(
            after.3 < large.len(),
            "large expired string was not reclaimed"
        );
    }

    #[test]
    fn string_interner_bounds_unique_churn_while_many_live_buckets_remain() {
        let mut pool = InternPool::default();
        let live = (0..128)
            .map(|index| pool.get(&format!("persistent-live-{index}")))
            .collect::<Vec<_>>();
        for cycle in 0..12 {
            for index in 0..512 {
                drop(pool.get(&format!("cycle-{cycle}-transient-{index}")));
            }
            let (_, _, expired, _) = pool.state.lock().unwrap().weak_stats();
            assert!(
                expired <= 128,
                "live-bucket churn left {expired} expired weak entries after cycle {cycle}"
            );
        }
        for (index, original) in live.iter().enumerate() {
            assert!(Arc::ptr_eq(
                original,
                &pool.get(&format!("persistent-live-{index}"))
            ));
        }
    }

    #[test]
    fn string_interner_prunes_hot_collision_bucket_during_equality_lookup() {
        let mut pool = InternPool::default();
        let live_unrelated = (0..96)
            .map(|index| pool.get(&format!("unrelated-live-{index}")))
            .collect::<Vec<_>>();
        let hash = 0x8765_4321;
        for index in 0..1024 {
            drop(pool.get_hashed(&format!("same-hash-transient-{index}"), hash));
            let state = pool.state.lock().unwrap();
            let entries = state.buckets.get(&hash).unwrap().len();
            assert!(
                entries <= 1,
                "hot collision lookup retained {entries} entries"
            );
        }
        let survivor = pool.get_hashed("same-hash-live", hash);
        assert!(Arc::ptr_eq(
            &survivor,
            &pool.get_hashed("same-hash-live", hash)
        ));
        for (index, original) in live_unrelated.iter().enumerate() {
            assert!(Arc::ptr_eq(
                original,
                &pool.get(&format!("unrelated-live-{index}"))
            ));
        }
    }

    #[test]
    fn string_interner_keeps_live_identity_and_cleans_collision_buckets() {
        let mut pool = InternPool::default();
        let mut clone = pool.clone();
        let live = pool.get("live-content");
        let repeated = clone.get("live-content");
        assert!(Arc::ptr_eq(&live, &repeated));
        for index in 0..256 {
            drop(pool.get(&format!("transient-live-check-{index}")));
        }
        assert!(Arc::ptr_eq(&live, &clone.get("live-content")));

        let mut collision_pool = InternPool::default();
        let collision_hash = 0x1234_5678;
        let mut collision_owners = (0..128)
            .map(|index| collision_pool.get_hashed(&format!("collision-{index}"), collision_hash))
            .collect::<Vec<_>>();
        let surviving = Arc::clone(&collision_owners[0]);
        collision_owners.clear();
        {
            let state = collision_pool.state.lock().unwrap();
            assert_eq!(state.buckets.get(&collision_hash).unwrap().len(), 128);
        }

        for _ in 0..40 {
            collision_pool.state.lock().unwrap().prune_some();
        }
        {
            let state = collision_pool.state.lock().unwrap();
            let bucket = state.buckets.get(&collision_hash).unwrap();
            assert_eq!(bucket.len(), 1);
            assert!(bucket.capacity() <= INTERN_SHRINK_MAX_LEN);
        }
        assert!(Arc::ptr_eq(
            &surviving,
            &collision_pool.get_hashed("collision-0", collision_hash)
        ));
        drop(surviving);
        for _ in 0..2 {
            collision_pool.state.lock().unwrap().prune_some();
        }
        assert!(!collision_pool
            .state
            .lock()
            .unwrap()
            .buckets
            .contains_key(&collision_hash));
        assert!(
            collision_pool.state.lock().unwrap().buckets.capacity() < INTERN_SHRINK_MIN_CAPACITY
        );
    }

    #[test]
    fn fixture_roundtrips_and_preserves_top_level_metadata() {
        let source = fixture();
        let store = WorldStore::from_snapshot(&source).unwrap();
        assert_eq!(store.len(), source["nodes"].as_array().unwrap().len());
        assert_eq!(store.to_snapshot(), source);
    }

    #[test]
    fn borrowed_set_delta_shares_text_and_binary_and_keeps_old_views_atomic() {
        use serde_json::json;

        let original_blob = json!({"$bytes_hex":"00"});
        let snapshot = json!({"version":1,"sequence":0,"nodes":[{
            "id":"payload","class_name":"Folder","name":"payload",
            "properties":{"Text":"old","Blob":original_blob}
        }]});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        let old_view = store.view();
        let old_node = old_view.read_only_node("payload").unwrap();

        let text = "delta text that remains borrowed ".repeat(64);
        let text_payload = crate::shared_payload::SharedPayload::from_raw(text.as_bytes().to_vec());
        let raw = (0..256).map(|i| (i ^ (i >> 3)) as u8).collect::<Vec<_>>();
        let binary_payload = crate::shared_payload::SharedPayload::from_raw(raw.clone());
        let mut hex = String::with_capacity(raw.len() * 2);
        for byte in &raw {
            hex.push_str(&format!("{byte:02x}"));
        }
        let expected_binary = json!({"$bytes_hex":hex});
        let mut delta = json!({"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","id":"payload","sequence":1,"fields":{
                "Text":text,"Blob":expected_binary
            }}
        ]});
        {
            let resolve = |bytes: &[u8]| {
                if text_payload.matches_exact(bytes) {
                    Some(Arc::clone(&text_payload))
                } else if binary_payload.matches_exact(bytes) {
                    Some(Arc::clone(&binary_payload))
                } else {
                    None
                }
            };
            store
                .apply_delta_with_shared(&delta, Some(&resolve))
                .unwrap();
        }
        delta["actions"][0]["fields"]["Text"] = json!("mutated after apply");
        delta["actions"][0]["fields"]["Blob"] = Value::Null;
        drop(delta);

        let new_node = store.view().read_only_node("payload").unwrap();
        assert_eq!(new_node.property("Text"), Some(json!(text)));
        assert_eq!(new_node.property("Blob"), Some(expected_binary));
        assert_eq!(old_node.property("Text"), Some(json!("old")));
        assert_eq!(old_node.property("Blob"), Some(json!({"$bytes_hex":"00"})));
        assert_eq!(
            store.storage_profile()["shared_payload_backed_property_count"],
            2
        );

        let before = store.to_snapshot();
        let rejected_payload = crate::shared_payload::SharedPayload::from_raw(vec![b'z'; 512]);
        {
            let resolve = |bytes: &[u8]| {
                rejected_payload
                    .matches_exact(bytes)
                    .then(|| Arc::clone(&rejected_payload))
            };
            let rejected = json!({"version":1,"delta":true,"sequence":3,"actions":[
                {"kind":"set","id":"payload","sequence":2,"fields":{
                    "Text":"z".repeat(512)
                }},
                {"kind":"unsupported","id":"payload","sequence":3}
            ]});
            assert!(store
                .apply_delta_with_shared(&rejected, Some(&resolve))
                .is_err());
        }
        assert_eq!(store.to_snapshot(), before);
        assert_eq!(Arc::strong_count(&rejected_payload), 1);
    }

    #[test]
    fn compact_shared_create_set_bindings_retain_arcs_and_rollback() {
        use crate::packed_properties::CompactSharedProperty;
        use crate::shared_payload::SharedPayload;
        use serde_json::json;

        let text_first = SharedPayload::from_raw("first shared text ".repeat(32).into_bytes());
        let text_final = SharedPayload::from_raw("final shared text ".repeat(32).into_bytes());
        let binary_bytes = (0..256)
            .map(|value| (value ^ 0xa5) as u8)
            .collect::<Vec<_>>();
        let binary = SharedPayload::from_raw(binary_bytes.clone());
        let mut store = WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[{
            "id":"payload","class_name":"Folder","properties":{"Text":"preexisting","Blob":{"$bytes_hex":"00"}}
        }]})).unwrap();
        let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
            {"sequence":1,"kind":"create","id":"payload","node":{
                "id":"payload","class_name":"Folder","properties":{"Text":null,"Blob":null}}},
            {"sequence":2,"kind":"set","id":"payload","fields":{"Text":null}}
        ]});
        let bindings = [
            CompactSharedProperty {
                action_index: 0,
                property: "Text".into(),
                payload: Arc::clone(&text_first),
                force_bytes: false,
            },
            CompactSharedProperty {
                action_index: 0,
                property: "Blob".into(),
                payload: Arc::clone(&binary),
                force_bytes: false,
            },
            CompactSharedProperty {
                action_index: 1,
                property: "Text".into(),
                payload: Arc::clone(&text_final),
                force_bytes: false,
            },
        ];
        let old_view = store.view();
        store.apply_compact_delta(&delta, None, &bindings).unwrap();
        let current = store.data.get("payload").unwrap();
        let mut found_text = false;
        let mut found_binary = false;
        current.properties.for_each_shared_payload(|payload| {
            found_text |= Arc::ptr_eq(payload, &text_final);
            found_binary |= Arc::ptr_eq(payload, &binary);
        });
        assert!(found_text && found_binary);
        assert_eq!(
            store
                .view()
                .read_only_node("payload")
                .unwrap()
                .property("Text"),
            Some(json!("final shared text ".repeat(32)))
        );
        assert_eq!(
            old_view.read_only_node("payload").unwrap().property("Text"),
            Some(json!("preexisting"))
        );

        let before = store.to_snapshot();
        let rejected = SharedPayload::from_raw("rejected payload ".repeat(40).into_bytes());
        let bad = json!({"version":1,"delta":true,"sequence":4,"actions":[
            {"sequence":3,"kind":"set","id":"payload","fields":{"Text":null}},
            {"sequence":4,"kind":"unsupported","id":"payload"}
        ]});
        let rejected_binding = [CompactSharedProperty {
            action_index: 0,
            property: "Text".into(),
            payload: Arc::clone(&rejected),
            force_bytes: false,
        }];
        assert!(store
            .apply_compact_delta(&bad, None, &rejected_binding)
            .is_err());
        assert_eq!(store.to_snapshot(), before);
        drop(rejected_binding);
        assert_eq!(Arc::strong_count(&rejected), 1);
    }

    #[test]
    fn shared_received_payloads_preserve_json_rollback_and_pinned_lifetimes() {
        use serde_json::json;

        let payload = crate::shared_payload::SharedPayload::from_raw(vec![0xab; 512]);
        let weak = Arc::downgrade(&payload);
        let original_value = json!({"$bytes_hex":"ab".repeat(512)});
        let create = json!({"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"create","id":"payload","sequence":1,"node":{
                "id":"payload","class_name":"Folder","name":"payload",
                "properties":{"Blob":original_value.clone()}}}
        ]});
        let empty = json!({"version":1,"nodes":[]});
        let mut shared = WorldStore::from_snapshot(&empty).unwrap();
        let mut ordinary = WorldStore::from_snapshot(&empty).unwrap();
        {
            let resolve = |bytes: &[u8]| payload.matches_exact(bytes).then(|| Arc::clone(&payload));
            shared
                .apply_delta_with_shared(&create, Some(&resolve))
                .unwrap();
        }
        ordinary.apply_delta(&create).unwrap();
        assert_eq!(shared.to_snapshot(), ordinary.to_snapshot());
        assert_eq!(
            shared.storage_profile()["shared_payload_backed_property_count"],
            1
        );
        assert_eq!(
            shared.storage_profile()["shared_property_unique_payload_bytes"],
            512
        );
        assert_eq!(
            shared.storage_profile()["shared_property_unique_stored_bytes"],
            payload.stored_bytes()
        );
        assert!(payload.stored_bytes() < payload.len());
        assert_eq!(
            shared.storage_profile()["world_state_blake2b512"],
            ordinary.storage_profile()["world_state_blake2b512"]
        );
        drop(payload);
        let pinned = shared.view().read_only_node("payload").unwrap();

        let rejected = crate::shared_payload::SharedPayload::from_raw(vec![0xcd; 512]);
        let before = shared.to_snapshot();
        {
            let resolve =
                |bytes: &[u8]| rejected.matches_exact(bytes).then(|| Arc::clone(&rejected));
            let bad = json!({"version":1,"delta":true,"sequence":3,"actions":[
                {"kind":"set","id":"payload","sequence":2,
                 "fields":{"Blob":{"$bytes_hex":"cd".repeat(512)}}},
                {"kind":"unsupported","id":"payload","sequence":3}
            ]});
            assert!(shared
                .apply_delta_with_shared(&bad, Some(&resolve))
                .is_err());
        }
        assert_eq!(shared.to_snapshot(), before);
        assert_eq!(Arc::strong_count(&rejected), 1);

        shared
            .apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[
                {"kind":"set","id":"payload","sequence":2,"fields":{"Blob":null}}
            ]}))
            .unwrap();
        assert_eq!(
            shared.storage_profile()["shared_payload_backed_property_count"],
            0
        );
        assert_eq!(
            shared
                .view()
                .read_only_node("payload")
                .unwrap()
                .property("Blob"),
            Some(Value::Null)
        );
        drop(shared);
        assert_eq!(pinned.property("Blob"), Some(original_value));
        assert!(weak.upgrade().is_some());
        drop(pinned);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn snapshot_rejects_unsupported_version_and_malformed_sequence() {
        let mut source = fixture();
        source["version"] = Value::from(2);
        assert!(WorldStore::from_snapshot(&source).is_err());

        let mut source = fixture();
        source["sequence"] = Value::String("7".into());
        assert!(WorldStore::from_snapshot(&source).is_err());
    }

    #[test]
    fn views_share_the_base_and_keep_private_overlays() {
        let store = WorldStore::from_snapshot(&fixture()).unwrap();
        let first = store.view();
        let mut second = store.view();
        assert!(Arc::ptr_eq(&first.base, &second.base));
        assert_eq!(first.overlay_bytes(), 0);
        second
            .set_property("scope:workspace", "Gravity", Value::from(80))
            .unwrap();
        assert_eq!(
            first.get_property("scope:workspace", "Gravity"),
            Some(Value::from(196.2))
        );
        assert_eq!(
            second.get_property("scope:workspace", "Gravity"),
            Some(Value::from(80))
        );
        let usage = store.shared_memory_usage(&[first, second]);
        assert_eq!(usage.view_count, 2);
        assert!(usage.overlay_bytes > 0);
    }

    #[test]
    fn children_query_preserves_order_and_view_local_representation() {
        let store = WorldStore::from_snapshot(&serde_json::json!({"version":1,"nodes":[
            {"id":"root","class_name":"Folder","parent":null,"properties":{}},
            {"id":"a","class_name":"Folder","parent":"root","properties":{}},
            {"id":"b","class_name":"Folder","parent":"root","properties":{}},
            {"id":"other","class_name":"Folder","parent":null,"properties":{}}
        ]}))
        .unwrap();
        let base = store.view();
        let mut overlay = store.view();
        assert_eq!(
            base.children_of("root")
                .iter()
                .map(|node| node.id())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        overlay
            .set_property("a", "Parent", serde_json::json!("other"))
            .unwrap();
        overlay.remove("b").unwrap();
        assert!(overlay.children_of("root").is_empty());
        assert_eq!(
            overlay
                .children_of("other")
                .iter()
                .map(|node| node.id())
                .collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!(base.children_of("root").len(), 2);
    }

    #[test]
    fn memory_report_counts_a_retained_old_revision_after_update() {
        let mut store = WorldStore::from_snapshot(&fixture()).unwrap();
        let old = store.view();
        let next_sequence = store.sequence() + 1;
        store
            .apply_delta(
                &serde_json::json!({"version":1,"delta":true,"sequence":next_sequence,"actions":[
                    {"sequence":next_sequence,"kind":"set","id":"scope:workspace","fields":{"Gravity":80}}
                ]}),
            )
            .unwrap();
        let current = store.view();
        let usage = store.shared_memory_usage(&[old, current]);
        assert_eq!(usage.view_count, 2);
        assert!(usage.shared_base_bytes >= store.data.estimated_bytes() * 2);
        assert!(usage.total_estimated_bytes >= usage.shared_base_bytes);
    }

    #[test]
    fn absent_and_null_properties_and_parent_fields_stay_distinct() {
        let snapshot = serde_json::json!({"version":1,"sequence":0,"services":{},"nodes":[
            {"id":"a","class_name":"Folder","name":"A","parent":null,"properties":{"PresentNull":null}},
            {"id":"b","class_name":"Folder","properties":{}}
        ]});
        let store = WorldStore::from_snapshot(&snapshot).unwrap();
        assert_eq!(store.to_snapshot(), snapshot);
        let view = store.view();
        assert_eq!(view.get_property("a", "PresentNull"), Some(Value::Null));
        assert_eq!(view.get_property("a", "Absent"), None);
        assert_eq!(view.name("b"), None);
    }

    #[test]
    fn metadata_field_lookup_handles_large_sections_and_deep_fallback() {
        let packed_map = Map::from_iter([(
            "origins".to_owned(),
            serde_json::json!({"Null":null,"Large":"x".repeat(512)}),
        )]);
        let packed = PackedMetadata::new(&packed_map).unwrap();
        assert!(matches!(packed.0.as_ref(), MetadataBacking::Packed(_)));
        assert_eq!(packed.get_path(&["origins", "Null"]), Some(Value::Null));
        assert_eq!(
            packed.get_path(&["origins", "Large"]),
            Some(Value::String("x".repeat(512)))
        );
        assert_eq!(packed.get_path(&["origins", "Missing"]), None);
        assert_eq!(packed.get_path(&["Large", "child"]), None);

        let mut deep_value = Value::Null;
        for _ in 0..100 {
            deep_value = Value::Array(vec![deep_value]);
        }
        let deep_map = Map::from_iter([
            ("deep".to_owned(), deep_value),
            (
                "origins".to_owned(),
                serde_json::json!({"Null":null,"Count":3}),
            ),
        ]);
        let deep = PackedMetadata::new(&deep_map).unwrap();
        assert!(matches!(deep.0.as_ref(), MetadataBacking::Deep(_)));
        assert_eq!(deep.get_path(&["origins", "Null"]), Some(Value::Null));
        assert_eq!(
            deep.get_path(&["origins", "Count"]),
            Some(serde_json::json!(3))
        );
        assert_eq!(deep.get_path(&["deep", "child"]), None);
        assert_eq!(deep.unpack(), deep_map);
    }

    #[test]
    fn compact_metadata_interning_is_exact_bounded_weak_and_cow_safe() {
        let negative_zero =
            serde_json::json!({"origins":{"Value":-0.0},"attributes":{"null":null}});
        let mut duplicate_map = negative_zero.as_object().unwrap().clone();
        let first = PackedMetadata::new(negative_zero.as_object().unwrap()).unwrap();
        let second = PackedMetadata::new(&duplicate_map).unwrap();
        let (first_backing, second_backing) = (first.packed_backing().unwrap(), second.packed_backing().unwrap());
        assert!(Arc::ptr_eq(first_backing, second_backing));
        assert_eq!(
            first.get_path(&["origins", "Value"]),
            Some(serde_json::json!(-0.0))
        );
        let negative_bytes = serde_json::to_vec(&first.unpack()).unwrap();
        duplicate_map.insert("origins".into(), serde_json::json!({"Value":"mutated"}));
        drop(negative_zero);
        drop(duplicate_map);
        assert_eq!(serde_json::to_vec(&first.unpack()).unwrap(), negative_bytes);

        let positive_zero = serde_json::json!({"origins":{"Value":0.0},"attributes":{"null":null}});
        let distinct = PackedMetadata::new(positive_zero.as_object().unwrap()).unwrap();
        let distinct_backing = distinct.packed_backing().unwrap();
        assert!(
            !Arc::ptr_eq(first_backing, distinct_backing),
            "canonical JSON interning must distinguish negative and positive zero"
        );
        assert_ne!(
            serde_json::to_vec(&first.unpack()).unwrap(),
            serde_json::to_vec(&distinct.unpack()).unwrap()
        );

        let too_large = Map::from_iter([("opaque".to_owned(), Value::String("x".repeat(600)))]);
        let large_first = PackedMetadata::new(&too_large).unwrap();
        let large_second = PackedMetadata::new(&too_large).unwrap();
        assert!(
            !Arc::ptr_eq(
                large_first.packed_backing().unwrap(),
                large_second.packed_backing().unwrap()
            ),
            "metadata above the bounded canonical serialization limit must not be interned"
        );

        let weak = Arc::downgrade(first_backing);
        drop(first);
        drop(second);
        assert!(
            weak.upgrade().is_none(),
            "the weak interner must not keep unused metadata alive"
        );
        drop(positive_zero);
    }

    #[test]
    fn metadata_interner_collision_and_capacity_limits_are_bounded() {
        let mut collisions = MetadataInterner::default();
        let make = |value: i64| {
            let map = Map::from_iter([("value".to_owned(), Value::from(value))]);
            let canonical = serde_json::to_vec(&map).unwrap();
            (map, canonical)
        };
        let (first_map, first_bytes) = make(0);
        let first = collisions.intern(17, &first_bytes, PackedProperties::new(&first_map).unwrap());
        let (other_map, other_bytes) = make(1);
        let other = collisions.intern(17, &other_bytes, PackedProperties::new(&other_map).unwrap());
        assert!(
            !Arc::ptr_eq(&first, &other),
            "hash collisions must be checked by exact content"
        );
        let (repeat_map, repeat_bytes) = make(0);
        let repeated = collisions.intern(
            17,
            &repeat_bytes,
            PackedProperties::new(&repeat_map).unwrap(),
        );
        assert!(Arc::ptr_eq(&first, &repeated));
        let mut collision_owners = vec![first, other, repeated];
        for value in 2..MAX_METADATA_INTERN_COLLISIONS as i64 {
            let (map, bytes) = make(value);
            collision_owners.push(collisions.intern(
                17,
                &bytes,
                PackedProperties::new(&map).unwrap(),
            ));
        }
        let (overflow_map, overflow_bytes) = make(99);
        let overflow_a = collisions.intern(
            17,
            &overflow_bytes,
            PackedProperties::new(&overflow_map).unwrap(),
        );
        let overflow_b = collisions.intern(
            17,
            &overflow_bytes,
            PackedProperties::new(&overflow_map).unwrap(),
        );
        assert!(
            !Arc::ptr_eq(&overflow_a, &overflow_b),
            "collision chains must stop at their cap"
        );
        assert_eq!(
            collisions.buckets.get(&17).unwrap().len(),
            MAX_METADATA_INTERN_COLLISIONS
        );

        let mut full = MetadataInterner::default();
        let mut live = Vec::with_capacity(MAX_METADATA_INTERN_BUCKETS);
        for key in 0..MAX_METADATA_INTERN_BUCKETS {
            let (map, bytes) = make(key as i64);
            live.push(full.intern(key as u64, &bytes, PackedProperties::new(&map).unwrap()));
        }
        assert_eq!(full.buckets.len(), MAX_METADATA_INTERN_BUCKETS);
        live.clear();
        for offset in 0..METADATA_PRUNE_INTERVAL {
            let (map, bytes) = make((MAX_METADATA_INTERN_BUCKETS + offset) as i64);
            drop(full.intern(
                (MAX_METADATA_INTERN_BUCKETS + offset) as u64,
                &bytes,
                PackedProperties::new(&map).unwrap(),
            ));
        }
        assert_eq!(
            full.buckets.len(),
            1,
            "capacity cleanup should reclaim expired buckets after the amortized interval"
        );
    }

    #[test]
    fn metadata_interner_keeps_missing_and_explicit_null_distinct() {
        let present_null = serde_json::json!({"origins":{"Value":null}});
        let absent = serde_json::json!({"origins":{}});
        let left = PackedMetadata::new(present_null.as_object().unwrap()).unwrap();
        let right = PackedMetadata::new(absent.as_object().unwrap()).unwrap();
        assert_eq!(left.get_path(&["origins", "Value"]), Some(Value::Null));
        assert_eq!(right.get_path(&["origins", "Value"]), None);
        assert!(!Arc::ptr_eq(
            left.packed_backing().unwrap(),
            right.packed_backing().unwrap()
        ));
    }

    #[test]
    fn pinned_world_view_keeps_old_metadata_after_update_and_store_drop() {
        let snapshot = serde_json::json!({"version":1,"sequence":0,"services":{},"nodes":[
            {"id":"n","class_name":"Folder","properties":{},
             "origins":{"Name":"received"},"attributes":{"phase":1},"attributes_known":true}
        ]});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        let pinned = store.view();
        let pinned_node = pinned.read_only_node("n").unwrap();
        store.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":1,"actions":[
            {"sequence":1,"kind":"attribute","id":"n","attribute_update":{"name":"phase","known":true,"value":2}}
        ]})).unwrap();
        assert_eq!(
            store
                .view()
                .read_only_node("n")
                .unwrap()
                .metadata_field("attributes", "phase"),
            Some(Value::from(2))
        );
        drop(store);
        assert_eq!(
            pinned_node.metadata_field("attributes", "phase"),
            Some(Value::from(1))
        );
        assert_eq!(
            pinned_node.metadata_field("origins", "Name"),
            Some(Value::from("received"))
        );
        assert_eq!(pinned.to_snapshot(), snapshot);
    }

    #[test]
    fn metadata_profile_reports_shared_backings_and_authoritative_layouts() {
        let repeated = serde_json::json!({"version":1,"sequence":0,"services":{},"nodes":[
            {"id":"a","class_name":"Folder","properties":{},"origins":{"Name":"received"}},
            {"id":"b","class_name":"Folder","properties":{},"origins":{"Name":"received"}},
            {"id":"c","class_name":"Folder","properties":{},"origins":{"Name":"local"}}
        ]});
        let store = WorldStore::from_snapshot(&repeated).unwrap();
        let profile = store.storage_profile();
        assert_eq!(profile["packed_metadata_reference_count"], 3);
        assert_eq!(profile["metadata_unique_backing_count"], 2);
        assert_eq!(profile["metadata_backing_reference_count"], 2);
        assert_eq!(
            profile["layout_packed_properties_bytes"],
            size_of::<PackedProperties>()
        );
        assert_eq!(
            profile["layout_packed_value_bytes"],
            packed_value_layout_bytes()
        );
        assert_eq!(
            profile["layout_packed_metadata_bytes"],
            size_of::<PackedMetadata>()
        );
        assert_eq!(profile["layout_node_record_bytes"], size_of::<NodeRecord>());
        assert_eq!(
            profile["layout_metadata_backing_arc_bytes"],
            size_of::<Arc<MetadataBacking>>()
        );
        assert_eq!(profile["metadata_duplicate_backing_references"], 1);
        assert!(profile["metadata_unique_backing_stored_bytes"]
            .as_u64()
            .is_some());
        assert_eq!(
            profile["estimated_node_metadata_bytes"].as_u64().is_some(),
            true,
            "legacy metadata estimate remains available"
        );
    }

    #[test]
    fn read_only_node_handles_pin_only_the_record_and_preserve_field_states() {
        let snapshot = serde_json::json!({"version":1,"sequence":0,"services":{},"nodes":[
            {"id":"a","class_name":"Part","name":null,"parent":null,
             "properties":{"PresentNull":null,"Count":1},
             "origins":{"PresentNull":null,"Count":"received"},
             "attributes":{"phase":2},"attributes_known":true,
             "property_revisions":{"Count":7}},
            {"id":"b","class_name":"Folder","properties":{}}
        ]});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        let old_view = store.view();
        let old_node = old_view.read_only_node("a").unwrap();
        assert_eq!(old_node.id(), "a");
        assert_eq!(old_node.class_name(), "Part");
        assert_eq!(old_node.name(), ReadOnlyField::Null);
        assert_eq!(old_node.parent(), ReadOnlyField::Null);
        assert_eq!(old_node.property("PresentNull"), Some(Value::Null));
        assert_eq!(old_node.property("Absent"), None);
        assert_eq!(
            old_node.metadata_field("origins", "PresentNull"),
            Some(Value::Null)
        );
        assert_eq!(old_node.metadata_field("origins", "Absent"), None);
        assert_eq!(
            old_node.metadata_section("attributes"),
            ReadOnlyField::Value(serde_json::json!({"phase":2}))
        );
        assert_eq!(
            old_view.read_only_node("b").unwrap().name(),
            ReadOnlyField::Missing
        );
        assert_eq!(
            old_view.read_only_node("b").unwrap().parent(),
            ReadOnlyField::Missing
        );
        assert_eq!(
            old_view
                .read_only_node("b")
                .unwrap()
                .metadata_section("attributes"),
            ReadOnlyField::Missing
        );

        let old_record = Arc::clone(&old_node.0);
        let next_sequence = store.sequence() + 1;
        store
            .apply_delta(&serde_json::json!({"version":1,"delta":true,
            "sequence":next_sequence,"actions":[{"sequence":next_sequence,"kind":"set",
            "id":"a","fields":{"Count":3}}]}))
            .unwrap();
        let new_node = store.view().read_only_node("a").unwrap();
        assert!(!Arc::ptr_eq(&old_record, &new_node.0));
        assert_eq!(old_node.property("Count"), Some(Value::from(1)));
        assert_eq!(new_node.property("Count"), Some(Value::from(3)));

        drop(store);
        drop(old_view);
        assert_eq!(old_node.property("PresentNull"), Some(Value::Null));
        assert_eq!(
            old_node.metadata_field("property_revisions", "Count"),
            Some(Value::from(7))
        );
    }

    #[test]
    fn packed_node_metadata_preserves_absence_null_values_and_cow_updates() {
        let source = serde_json::json!({"version":1,"sequence":0,"services":{},"nodes":[
            {"id":"a","class_name":"Part","properties":{},
                "origins":{"Color":"received","Transparency":"derived_received"},
                "attributes":{"active":true},"attributes_known":true,
                "property_revisions":{"Color":9},"opaque":null},
            {"id":"b","class_name":"Folder","properties":{}}
        ]});
        let original = WorldStore::from_snapshot(&source).unwrap();
        assert_eq!(original.to_snapshot(), source);
        let profile = original.storage_profile();
        let packed = profile["estimated_node_metadata_bytes"].as_u64().unwrap() as usize;
        let expanded = source["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| {
                node.as_object()
                    .unwrap()
                    .iter()
                    .filter(|(key, _)| {
                        !matches!(
                            key.as_str(),
                            "id" | "class_name" | "name" | "parent" | "properties"
                        )
                    })
                    .map(|(key, value)| {
                        key.len() + size_of::<(String, Value)>() + estimate_value(value)
                    })
                    .sum::<usize>()
            })
            .sum::<usize>();
        assert!(
            packed < expanded,
            "packed metadata {packed} bytes must beat its expanded maps {expanded}"
        );
        let old_view = original.view();
        let mut changed = original.clone();
        let sequence = changed.sequence() + 1;
        changed.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":sequence,"actions":[
            {"sequence":sequence,"kind":"attribute","id":"a","attribute_update":{"name":"phase","known":true,"value":4}}
        ]})).unwrap();
        assert_eq!(old_view.to_snapshot(), source);
        let updated = changed.to_snapshot();
        assert_eq!(updated["nodes"][0]["attributes"]["phase"], 4);
        assert_eq!(
            updated["nodes"][0]["origins"],
            source["nodes"][0]["origins"]
        );
        assert_eq!(
            updated["nodes"][0]["property_revisions"],
            source["nodes"][0]["property_revisions"]
        );
        assert_eq!(updated["nodes"][0]["opaque"], Value::Null);
        assert!(updated["nodes"][0].get("not_present").is_none());
        assert_eq!(old_view.get_property("a", "Transparency"), None);
        assert_eq!(changed.storage_profile()["world_state_blake2b512"], {
            use blake2::{Blake2b512, Digest};
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&sequence.to_le_bytes());
            for node in updated["nodes"].as_array().unwrap() {
                let bytes_node = serde_json::to_vec(node).unwrap();
                bytes.extend_from_slice(&(bytes_node.len() as u64).to_le_bytes());
                bytes.extend(bytes_node);
            }
            format!("{:x}", Blake2b512::digest(bytes))
        });
    }

    #[test]
    fn packed_metadata_snapshots_merge_existing_fields_and_reject_invalid_maps_atomically() {
        let snapshot = serde_json::json!({"version":1,"sequence":2,"nodes":[
            {"id":"p","class_name":"Part","properties":{"Anchored":false},
                "origins":{"Anchored":"received","Color":"local"},
                "property_revisions":{"Anchored":2,"Color":1},
                "attributes":{"old":true},"attributes_known":true,
                "tags":{"old_tag":true},"custom":null}
        ]});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        let before = store.to_snapshot();
        let invalid = serde_json::json!({"version":1,"delta":true,"sequence":3,"actions":[
            {"sequence":3,"kind":"set","id":"p","fields":{"Anchored":true},"attribute_snapshot":null}
        ]});
        assert!(store.apply_delta(&invalid).is_err());
        assert_eq!(store.to_snapshot(), before);
        let good = serde_json::json!({"version":1,"delta":true,"sequence":4,"actions":[
            {"sequence":3,"kind":"set","id":"p","fields":{"Anchored":true},
                "attribute_snapshot":{"attributes":{"new":7},"attributes_known":false,"custom":null},
                "tag_snapshot":{"tags":{"new_tag":false}}}
        ]});
        store.apply_delta(&good).unwrap();
        let updated = store.to_snapshot();
        assert_eq!(
            updated["nodes"][0]["origins"],
            before["nodes"][0]["origins"]
        );
        let mut revisions = before["nodes"][0]["property_revisions"].clone();
        revisions["Anchored"] = serde_json::json!(3);
        assert_eq!(updated["nodes"][0]["property_revisions"], revisions);
        assert_eq!(
            updated["nodes"][0]["attributes"],
            serde_json::json!({"new":7})
        );
        assert_eq!(updated["nodes"][0]["attributes_known"], false);
        assert_eq!(
            updated["nodes"][0]["tags"],
            serde_json::json!({"new_tag":false})
        );
        assert_eq!(updated["nodes"][0]["custom"], Value::Null);
        assert!(updated["nodes"][0].get("not_present").is_none());
    }

    #[test]
    fn deep_opaque_metadata_keeps_legacy_acceptance_without_loosening_properties() {
        let mut deep = Value::Null;
        for _ in 0..100 {
            deep = Value::Array(vec![deep]);
        }
        let snapshot = serde_json::json!({"version":1,"sequence":0,"nodes":[
            {"id":"a","class_name":"Folder","properties":{},"custom":{"deep":deep}}
        ]});
        let store = WorldStore::from_snapshot(&snapshot).unwrap();
        assert_eq!(store.to_snapshot(), snapshot);
        let mut invalid = snapshot;
        invalid["nodes"][0]["properties"]["deep"] = invalid["nodes"][0]["custom"]["deep"].clone();
        assert!(WorldStore::from_snapshot(&invalid).is_err());
    }

    #[test]
    fn deltas_are_ordered_transactional_and_preserve_extra_fields() {
        let mut store = WorldStore::from_snapshot(&fixture()).unwrap();
        let original = store.to_snapshot();
        let base_sequence = store.sequence();
        let bad = serde_json::json!({"version":1,"delta":true,"sequence":base_sequence + 2,"actions":[
            {"sequence":base_sequence + 1,"kind":"set","id":"scope:workspace","fields":{"Gravity":80}},
            {"sequence":base_sequence + 2,"kind":"made_up","id":"scope:workspace"}
        ]});
        assert!(store.apply_delta(&bad).is_err());
        assert_eq!(store.to_snapshot(), original);

        let good = serde_json::json!({"version":1,"delta":true,"sequence":base_sequence + 4,"actions":[
            {"sequence":base_sequence + 3,"kind":"set","id":"scope:workspace","fields":{"Gravity":80,"ExplicitNull":null}},
            {"sequence":base_sequence + 4,"kind":"create","id":"child","node":{"id":"child","class_name":"Part","name":"Child","parent":"scope:workspace","properties":{"Anchored":false},"origins":{"Anchored":"received"},"attributes":{"tag":true},"custom":17}}
        ]});
        store.apply_delta(&good).unwrap();
        assert_eq!(store.sequence(), base_sequence + 4);
        let out = store.to_snapshot();
        assert_eq!(out["nodes"][0]["properties"]["Gravity"], 80);
        assert_eq!(out["nodes"][0]["properties"]["ExplicitNull"], Value::Null);
        assert_eq!(
            out["nodes"][out["nodes"].as_array().unwrap().len() - 1]["origins"]["Anchored"],
            "received"
        );
        assert_eq!(
            out["nodes"][out["nodes"].as_array().unwrap().len() - 1]["custom"],
            17
        );
        assert!(store
            .apply_delta(&serde_json::json!({
                "version":1,"delta":true,"sequence":base_sequence + 3,"actions":[]
            }))
            .is_err());
    }

    #[test]
    fn normalized_create_can_take_identity_from_node() {
        let mut store = WorldStore::from_snapshot(&fixture()).unwrap();
        let sequence = store.sequence() + 1;
        // Normalized replication create records carry the stable ID on the
        // node record; some producers omit the redundant action-level ID.
        let delta = serde_json::json!({"version":1,"delta":true,"sequence":sequence,"actions":[
            {"sequence":sequence,"kind":"create","node":{"id":"created:17","class_name":"Part","name":"Created","parent":"scope:workspace","properties":{"Anchored":false},"origins":{"Anchored":"received"},"attributes":{},"constructor_defaults_known":true}}
        ]});
        store.apply_delta(&delta).unwrap();
        assert!(store.contains("created:17"));

        let mismatch = serde_json::json!({"version":1,"delta":true,"sequence":sequence + 1,"actions":[
            {"sequence":sequence + 1,"kind":"create","id":"wrong:18","node":{"id":"created:18","class_name":"Part","name":"Mismatch"}}
        ]});
        assert!(store.apply_delta(&mismatch).is_err());
        assert!(!store.contains("created:18"));
    }

    #[test]
    fn cloned_store_stays_isolated_when_only_one_clone_applies_delta() {
        let mut updated = WorldStore::from_snapshot(&fixture()).unwrap();
        let untouched = updated.clone();
        let sequence = updated.sequence() + 1;
        let delta = serde_json::json!({"version":1,"delta":true,"sequence":sequence + 1,"actions":[
            {"sequence":sequence,"kind":"set","id":"scope:workspace","fields":{"Gravity":80}},
            {"sequence":sequence + 1,"kind":"create","node":{"id":"clone-only","class_name":"Folder","name":"Clone only","parent":"scope:workspace","properties":{}}}
        ]});
        updated.apply_delta(&delta).unwrap();
        assert_eq!(
            updated.view().get_property("scope:workspace", "Gravity"),
            Some(Value::from(80))
        );
        assert_eq!(
            untouched.view().get_property("scope:workspace", "Gravity"),
            Some(Value::from(196.2))
        );
        assert!(!untouched.view().contains("clone-only"));
        assert!(updated.view().contains("clone-only"));
    }

    #[test]
    fn delta_cannot_grow_world_past_snapshot_node_limit() {
        let nodes: Vec<Value> = (0..MAX_NODES)
            .map(|index| serde_json::json!({"id":format!("n{index}"),"class_name":"Folder","properties":{}}))
            .collect();
        let snapshot = serde_json::json!({"version":1,"sequence":0,"nodes":nodes});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        let delta = serde_json::json!({"version":1,"delta":true,"sequence":1,"actions":[
            {"sequence":1,"kind":"create","node":{"id":"n-extra","class_name":"Folder","properties":{}}}
        ]});
        assert!(store.apply_delta(&delta).is_err());
        assert_eq!(store.len(), MAX_NODES);
        assert_eq!(store.sequence(), 0);
    }

    #[test]
    fn repeated_set_attribute_and_event_actions_keep_receipt_order() {
        let mut store = WorldStore::from_snapshot(&fixture()).unwrap();
        let base_sequence = store.sequence();
        let delta = serde_json::json!({"version":1,"delta":true,"sequence":base_sequence + 4,"actions":[
            {"sequence":base_sequence + 1,"kind":"set","id":"scope:workspace","fields":{"Gravity":90}},
            {"sequence":base_sequence + 2,"kind":"set","id":"scope:workspace","fields":{"Gravity":80}},
            {"sequence":base_sequence + 3,"kind":"attribute","id":"scope:workspace","attribute_update":{"name":"phase","known":true,"value":2}},
            {"sequence":base_sequence + 4,"kind":"event","id":"scope:workspace","name":"Ping","arguments":[1]}
        ]});
        store.apply_delta(&delta).unwrap();
        assert_eq!(
            store.get_property("scope:workspace", "Gravity"),
            Some(Value::from(80))
        );
        assert_eq!(store.to_snapshot()["nodes"][0]["attributes"]["phase"], 2);
        assert_eq!(store.pending_events().len(), 1);
        assert_eq!(store.pending_events()[0]["name"], "Ping");
        store.acknowledge_events(base_sequence + 4);
        assert!(store.pending_events().is_empty());
    }

    #[test]
    fn remove_cascades_through_children_and_view_mutation_does_not_touch_base() {
        let snapshot = serde_json::json!({"version":1,"sequence":0,"nodes":[
            {"id":"a","class_name":"Folder","parent":null,"properties":{}},
            {"id":"b","class_name":"Folder","parent":"a","properties":{}},
            {"id":"c","class_name":"Part","parent":"b","properties":{}}
        ]});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        let mut view = store.view();
        view.remove("b").unwrap();
        assert!(view.contains("a"));
        assert!(!view.contains("b") && !view.contains("c"));
        assert_eq!(store.len(), 3);
        store
            .apply_delta(
                &serde_json::json!({"version":1,"delta":true,"sequence":1,"actions":[
                    {"sequence":1,"kind":"remove","id":"b"}
                ]}),
            )
            .unwrap();
        assert_eq!(store.len(), 1);
    }
    #[test]
    fn equal_walk_speed_receipts_advance_only_the_speed_revision() {
        let mut store = WorldStore::from_snapshot(&serde_json::json!({"version":1,"sequence":1,"nodes":[
            {"id":"h","class_name":"Humanoid","name":"Humanoid","properties":{"WalkSpeed":16}}
        ]})).unwrap();
        let before = store.view();
        store.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":3,"actions":[
            {"kind":"set","id":"h","sequence":2,"fields":{"WalkSpeed":16}},
            {"kind":"set","id":"h","sequence":3,"fields":{"Health":90}}
        ]})).unwrap();
        let node = store.view().read_only_node("h").unwrap();
        assert_eq!(node.metadata_field("property_revisions","WalkSpeed"), Some(serde_json::json!(2)));
        assert!(node.metadata_field("property_revisions","Health").is_none());
        assert!(before.read_only_node("h").unwrap().metadata_field("property_revisions","WalkSpeed").is_none());
        let committed = store.to_snapshot();
        assert!(store.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":5,"actions":[
            {"kind":"set","id":"h","sequence":4,"fields":{"WalkSpeed":30}},
            {"kind":"invalid","sequence":5}
        ]})).is_err());
        assert_eq!(store.to_snapshot(), committed);
    }

    #[test]
    fn assembly_dependency_revisions_are_sparse_ordered_and_atomic() {
        let mut store=WorldStore::from_snapshot(&serde_json::json!({"version":1,"sequence":0,"nodes":[
            {"id":"p","class_name":"Part","name":"p","properties":{}}
        ]})).unwrap();
        let before=store.view();
        store.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":3,"actions":[
            {"kind":"set","id":"p","sequence":1,"fields":{"Size":{"$vector3":[2,2,2]},"Anchored":false,"Massless":false,"RootPriority":0,"Name":"Body","Transparency":0.5}},
            {"kind":"set","id":"p","sequence":2,"fields":{"Mass":8,"CenterOfMass":{"$vector3":[0,0,0]},"AssemblyMass":8,"AssemblyCenterOfMass":{"$vector3":[0,0,0]}}},
            {"kind":"set","id":"p","sequence":3,"fields":{"Transparency":0.75}}
        ]})).unwrap();
        let snapshot=store.to_snapshot();
        let revisions=&snapshot["nodes"][0]["property_revisions"];
        for key in ["Size","Anchored","Massless","RootPriority","Name"] {assert_eq!(revisions[key],1);}
        for key in ["Mass","CenterOfMass","AssemblyMass","AssemblyCenterOfMass"] {assert_eq!(revisions[key],2);}
        assert!(revisions.get("Transparency").is_none());
        assert!(before.read_only_node("p").unwrap().metadata_field("property_revisions","Size").is_none());
        assert!(store.apply_delta(&serde_json::json!({"version":1,"delta":true,"sequence":5,"actions":[
            {"kind":"set","id":"p","sequence":4,"fields":{"AssemblyMass":9}},
            {"kind":"invalid","sequence":5}
        ]})).is_err());
        assert_eq!(store.to_snapshot(),snapshot);
    }

}
