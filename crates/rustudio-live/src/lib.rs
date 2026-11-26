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
//! M1 is the offline half: [`snapshot`] parses the world JSON rove's
//! `WorldStore` normalises replication into, and [`world::build`] makes the
//! document from it. The live half (M2) feeds the replica a world built from
//! the same shapes, so the parser and the value converter are shared.
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
