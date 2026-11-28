//! **Telling a live client's runtime what its own document just did.**
//!
//! Play Solo replicates through `replicate::apply`, which is a `Runtime` call:
//! every instance copied in is parented by `apply_parent_replicated`, so
//! `ChildAdded`, `DescendantAdded` and `AncestryChanged` fire, scripts that
//! arrive where they run are collected, and the world a `LocalScript` walks is
//! the world it was handed.
//!
//! A live join does not have that. `rustudio_live::Live::apply` writes the
//! document *directly* — that is what makes applying a 4096-row patch a memcpy
//! rather than a geometry rebuild — and the client's `Runtime` therefore never
//! heard about any of it. The consequences were exactly the reports:
//!
//! * the player's `Character` arrives after `start_play`, with `Animate` and
//!   `Health` inside it, and no `PlayerModule` was ever told the character
//!   exists, so the character slid around unanimated;
//! * a tool replicated into the `Backpack` never appeared in it;
//! * a `PlayerGui` screen replicated in was drawn (the editor reads the
//!   document for that) but its scripts never started;
//! * every `ChildAdded` in the place — which is how most Roblox code is
//!   written — was never fired.
//!
//! This module replays `Live`'s own record of what it did
//! ([`LiveChange`]) through the client runtime, which is where the signals
//! come from. It is deliberately *not* a second replication path: the document
//! is already right, and nothing here writes a property or a parent a second
//! time. It fires the signals and starts the scripts.

use rbx_dom_weak::types::Ref;
use rustudio_live::live::LiveChange;

use crate::game::Game;

/// Replay one batch's worth of document changes through the live client's
/// runtime.
///
/// A no-op unless a `RunMode::Client` session is playing: before `start_play`
/// there is no client runtime to tell, and in any other mode the join's window
/// is not the thing owning the document.
pub(crate) fn replay(game: &mut Game, changes: &[LiveChange]) {
    if changes.is_empty() {
        return;
    }
    let live = game
        .play
        .as_ref()
        .is_some_and(|s| s.mode == rustudio_engine::runtime::RunMode::Client);
    if !live {
        return;
    }
    game.with_live_client(|rt| {
        for change in changes {
            match *change {
                // **A create is a parent edge, not a new instance.** The
                // instance is already in the document (with all of its
                // properties, which `Live` converted in one pass). What the
                // runtime has to do is what a `Parent` write does: fire
                // `ChildAdded`/`DescendantAdded` on the ancestors, and queue any
                // script in the subtree that now runs here. `notify_parent` is
                // that path for an instance it has never seen — it does not
                // write the parent, it *reports* the one the document has.
                LiveChange::Created { child, parent } => {
                    // Destroyed again later in the same batch: nothing to
                    // report, and `notify_parent` would name a ref the document
                    // no longer has.
                    if rt.has(child) {
                        rt.notify_parent(child, Ref::none(), parent, vec![child]);
                        // A `LocalScript` that arrived with the world starts on
                        // the next dispatch, as it does when Play Solo's server
                        // replicates it in.
                        rt.start_scripts_in(child);
                    }
                }
                LiveChange::Reparented { child, old, new } => {
                    if rt.has(child) {
                        rt.notify_parent(child, old, new, vec![child]);
                        rt.start_scripts_in(child);
                    }
                }
                // The instance is gone from the document already (`Live`
                // destroyed it). The signals still have to fire, with the parent
                // it had, so a script's `ChildRemoved` / `AncestryChanged` runs.
                // `notify_parent` with `new` = none is exactly that.
                LiveChange::Removed { child, old } => {
                    rt.notify_parent(child, old, Ref::none(), Vec::new());
                }
            }
        }
        rt.dispatch();
    });
}
