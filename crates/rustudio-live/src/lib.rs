//! A live Roblox session as rustudio's client DataModel.
//!
//! rove decodes the wire (transport, replication items, a world store); this
//! crate is the **bridge**: it turns what rove decoded into the DataModel the
//! rest of rustudio already renders, runs and replicates against. There is no
//! second world here and no second VM — [`world::World`] wraps one
//! `rustudio_core::doc::Document` and one rove-id → `Ref` map, the same pair
//! `rustudio_engine::replicate::initial_client` produces for a local Play
//! session (docs/M0-UNDERSTANDING.md §3).
//!
//! The offline half is M1: [`snapshot`] parses the world JSON rove's
//! `WorldStore` normalises replication into, and [`world::build`] makes the
//! document from it in one pass.
//!
//! The live half is M2: [`live::Live`] applies replication as it arrives — a
//! create, a set, a parent edge, a remove — to a world that is already being
//! drawn, in rove's own action vocabulary (`create`/`set`/`remove`/`event`).
//! It shares [`snapshot::ValueConverter`] and the id and `Ref` map with the
//! offline half, so a value or an id means the same thing whichever way it
//! arrived.

//!
//! The rules this crate keeps (docs/LIVE-JOIN.md):
//!
//! - values are converted **once**, at the boundary, into `rbx_types::Variant`
//!   — no JSON is held per frame and none reaches the frame path;
//! - a child whose parent has not arrived waits ([`world::World::stats`]
//!   `out_of_order`) instead of being re-parented twice, which is what
//!   `replicate::arrive` does for a local Play session;
//! - a `$ref` to a node the world does not have is dropped and counted, never
//!   left as a `Ref` into nowhere (the scene walk and the physics reader would
//!   follow it);
//! - classes come from `api-dump.json`, not from a list in this crate.

pub mod cframe;
pub mod live;
pub mod snapshot;
pub mod world;

/// Where a rove id comes from: the world snapshot's scope id (`scope:player-1`).
pub type RoveId = String;

/// The id map's own error, used by the builder and the replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// Two nodes carry one id: the world is ambiguous, so it is refused
    /// rather than silently keeping one.
    DuplicateId(String),
    /// A property value the converter cannot make into a `Variant`.
    Value { class: String, property: String, detail: String },
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::DuplicateId(id) => write!(f, "duplicate rove id {id:?}"),
            BridgeError::Value { class, property, detail } => {
                write!(f, "{class}.{property}: {detail}")
            }
        }
    }
}

impl std::error::Error for BridgeError {}
