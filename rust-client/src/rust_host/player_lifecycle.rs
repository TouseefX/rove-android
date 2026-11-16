//! Player events derived from committed world changes. State is per VM and
//! contains only player IDs/references, never another copy of the world.
use super::{
    enums, signals, values,
    world::{self, Node, World, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde_json::Value as Json;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};

#[derive(Clone, Default)]
struct Player {
    parent: Option<String>,
    character: Option<String>,
    announced: Option<String>,
}

#[derive(Default)]
struct State {
    players: HashMap<String, Player>,
    // Only explicit events paired with a derived event in this receipt.
    consumed: HashSet<u64>,
}
type StateRef = Rc<RefCell<State>>;
#[derive(Default)]
struct States(HashMap<usize, (Weak<RefCell<World>>, StateRef)>);

fn reference(value: &Json) -> Option<String> {
    value
        .get("$ref")
        .and_then(Json::as_str)
        .or_else(|| value.as_str())
        .map(str::to_owned)
}
fn character(node: &Node) -> Option<String> {
    world::node_property(node, "Character")
        .as_ref()
        .and_then(reference)
}
fn model(ctx: &WorldRef, id: &str) -> bool {
    world::is_a(ctx, id, "Model") && ctx.borrow().nodes.get(id).is_some_and(|n| !n.destroyed)
}
fn state(lua: &Lua, ctx: &WorldRef) -> StateRef {
    let key = Rc::as_ptr(ctx) as usize;
    if let Some(states) = lua.app_data_ref::<States>() {
        if let Some((owner, state)) = states.0.get(&key) {
            if owner.upgrade().is_some_and(|owner| Rc::ptr_eq(&owner, ctx)) {
                return state.clone();
            }
        }
    }
    let players = {
        let world = ctx.borrow();
        world
            .nodes
            .ids()
            .filter(|id| world.nodes.class_name(id) == Some("Player"))
            .filter_map(|id| {
                world.nodes.get(id).filter(|n| !n.destroyed).map(|node| {
                    let character = character(node);
                    let announced = character.as_ref().filter(|id| model(ctx, id)).cloned();
                    (
                        id.to_owned(),
                        Player {
                            parent: node.parent.clone(),
                            character,
                            announced,
                        },
                    )
                })
            })
            .collect()
    };
    let result = Rc::new(RefCell::new(State {
        players,
        consumed: HashSet::new(),
    }));
    if lua.app_data_ref::<States>().is_none() {
        lua.set_app_data(States::default());
    }
    let mut states = lua.app_data_mut::<States>().unwrap();
    states.0.retain(|_, (owner, _)| owner.strong_count() > 0);
    states.0.insert(key, (Rc::downgrade(ctx), result.clone()));
    result
}

pub(super) fn install(lua: &Lua, ctx: &WorldRef) {
    state(lua, ctx);
}

struct Event {
    source: String,
    name: &'static str,
    target: Option<String>,
    explicit_args: Option<Vec<Json>>,
}
impl Event {
    fn new(source: &str, name: &'static str, target: Option<String>) -> Self {
        Self {
            source: source.into(),
            name,
            target,
            explicit_args: None,
        }
    }
    fn publish(self, lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
        if self.name == "property:Character" {
            return world::emit_property_changed_direct(lua, ctx, &self.source, "Character");
        }
        if !signals::has_receivers(lua, ctx, &self.source, self.name)? {
            return Ok(());
        }
        let mut args = MultiValue::new();
        if let Some(explicit) = self.explicit_args {
            for value in explicit {
                args.push_back(values::from_json(lua, &value)?);
            }
            if self.name == "PlayerRemoving" && args.len() == 1 {
                args.push_back(Value::UserData(enums::enum_item(
                    lua,
                    "PlayerExitReason",
                    "Unknown",
                )?));
            }
        } else {
            let Some(target) = self.target else {
                return Ok(());
            };
            let value = world::instance(lua, ctx, &target)?;
            // A streamed reference is not an Instance until its record arrives.
            if matches!(value, Value::Nil) {
                return Ok(());
            }
            args.push_back(value);
            if self.name == "PlayerRemoving" {
                args.push_back(Value::UserData(enums::enum_item(
                    lua,
                    "PlayerExitReason",
                    "Unknown",
                )?));
            }
        }
        signals::fire(lua, ctx, &self.source, self.name, args)
    }
}

fn membership(
    id: &str,
    player: &mut Player,
    new: Option<String>,
    service: Option<&str>,
    events: &mut Vec<Event>,
) {
    if player.parent == new {
        return;
    }
    if let Some(service) = service {
        if player.parent.as_deref() == Some(service) {
            events.push(Event::new(service, "PlayerRemoving", Some(id.into())));
        }
        if new.as_deref() == Some(service) {
            events.push(Event::new(service, "PlayerAdded", Some(id.into())));
        }
    }
    player.parent = new;
}

fn change_character(
    id: &str,
    player: &mut Player,
    new: Option<String>,
    available: bool,
    events: &mut Vec<Event>,
) {
    if player.character == new {
        return;
    }
    if new.is_none() || available {
        events.push(Event::new(id, "property:Character", None));
    }
    if let Some(old) = player.announced.take() {
        events.push(Event::new(id, "CharacterRemoving", Some(old)));
    }
    player.character = new;
    if available {
        player.announced = player.character.clone();
        events.push(Event::new(id, "CharacterAdded", player.character.clone()));
    }
}

fn resolve(players: &mut HashMap<String, Player>, id: &str, events: &mut Vec<Event>) {
    let mut ids = players
        .iter()
        .filter(|(_, p)| p.character.as_deref() == Some(id) && p.announced.is_none())
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    for player_id in ids {
        let player = players.get_mut(&player_id).unwrap();
        player.announced = Some(id.into());
        events.push(Event::new(&player_id, "property:Character", None));
        events.push(Event::new(&player_id, "CharacterAdded", Some(id.into())));
    }
}

/// A narrow event plan prepared against the old world. Nothing is published or
/// retained if validation of the actual world transaction subsequently fails.
pub(super) struct Receipt {
    state: StateRef,
    next: Option<HashMap<String, Player>>,
    events: HashMap<usize, Vec<Event>>,
    consumed: HashSet<u64>,
}
impl Receipt {
    pub(super) fn commit(&mut self) {
        let mut state = self.state.borrow_mut();
        if let Some(next) = self.next.take() {
            state.players = next;
        }
        state.consumed = std::mem::take(&mut self.consumed);
    }
    pub(super) fn publish(&mut self, lua: &Lua, ctx: &WorldRef, index: usize) -> mlua::Result<()> {
        for event in self.events.remove(&index).unwrap_or_default() {
            event.publish(lua, ctx)?;
        }
        Ok(())
    }
}

fn field<'a>(action: &'a Json, key: &str) -> Option<&'a Json> {
    if action
        .get("property")
        .or_else(|| action.get("key"))
        .and_then(Json::as_str)
        == Some(key)
    {
        action.get("value")
    } else {
        action.get("fields").and_then(|fields| fields.get(key))
    }
}
fn inside(
    ctx: &WorldRef,
    parents: &HashMap<String, Option<String>>,
    id: &str,
    roots: &HashSet<&str>,
) -> bool {
    let mut id = Some(id.to_owned());
    let mut seen = HashSet::new();
    while let Some(current) = id {
        if roots.contains(current.as_str()) {
            return true;
        }
        if !seen.insert(current.clone()) {
            break;
        }
        id = parents
            .get(&current)
            .cloned()
            .unwrap_or_else(|| ctx.borrow().nodes.parent(&current).flatten());
    }
    false
}

pub(super) fn prepare(
    lua: &Lua,
    ctx: &WorldRef,
    actions: &[Json],
    previous: Option<u64>,
    shared: Option<&crate::engine_world_view::SharedProjectedWorldView>,
) -> Receipt {
    let state = state(lua, ctx);
    let mut receipt = Receipt {
        state: state.clone(),
        next: None,
        events: HashMap::new(),
        consumed: HashSet::new(),
    };
    let fresh = |row: &Json| {
        previous.is_none_or(|previous| row["sequence"].as_u64().unwrap_or(0) > previous)
    };
    let pending = state
        .borrow()
        .players
        .values()
        .any(|p| p.character.is_some() && p.announced.is_none());
    // Movement-only receipts must not copy the player registry or scan the world.
    let interested = actions
        .iter()
        .filter(|a| fresh(a))
        .any(|a| match a["kind"].as_str() {
            Some("create") => pending || a["node"]["class_name"] == "Player",
            Some("remove" | "destroy") => true,
            Some("set" | "set_property") => {
                (field(a, "Character").is_some() || field(a, "Parent").is_some())
                    && a["id"]
                        .as_str()
                        .is_some_and(|id| state.borrow().players.contains_key(id))
            }
            _ => false,
        });
    if !interested {
        return receipt;
    }
    let mut players = state.borrow().players.clone();
    let service = ctx.borrow().services.get("Players").cloned();
    let mut parents = HashMap::new();
    let create_character = |a: &Json| {
        let node = &a["node"];
        if let Some(properties) = node.get("properties") {
            return properties.get("Character").and_then(reference);
        }
        shared
            .and_then(|view| view.lookup_node(node["id"].as_str()?))
            .and_then(|node| node.property("Character").ok().flatten())
            .as_ref()
            .and_then(reference)
    };
    let mut available = HashMap::new();
    for id in players
        .values()
        .filter_map(|p| p.character.as_ref())
        .cloned()
        .chain(
            actions
                .iter()
                .filter_map(|a| field(a, "Character").and_then(reference)),
        )
        .chain(
            actions
                .iter()
                .filter(|a| a["kind"] == "create")
                .filter_map(create_character),
        )
    {
        available
            .entry(id.clone())
            .or_insert_with(|| model(ctx, &id));
    }
    for (index, action) in actions.iter().enumerate().filter(|(_, a)| fresh(a)) {
        let mut events = Vec::new();
        let id = action["id"]
            .as_str()
            .or_else(|| action["node"]["id"].as_str())
            .unwrap_or_default();
        match action["kind"].as_str() {
            Some("create") => {
                let class = action["node"]["class_name"].as_str().unwrap_or_default();
                let parent = action["node"]["parent"].as_str().map(str::to_owned);
                parents.insert(id.to_owned(), parent.clone());
                if available.contains_key(id) {
                    let is_model = world::class_chain(class).iter().any(|base| base == "Model");
                    available.insert(id.to_owned(), is_model);
                    if is_model {
                        resolve(&mut players, id, &mut events);
                    }
                }
                if class == "Player" {
                    let player = players.entry(id.into()).or_default();
                    membership(id, player, parent, service.as_deref(), &mut events);
                    let character = create_character(action);
                    let present = character
                        .as_ref()
                        .is_some_and(|id| available.get(id) == Some(&true));
                    change_character(id, player, character, present, &mut events);
                } else if let Some(mut player) = players.remove(id) {
                    membership(id, &mut player, None, service.as_deref(), &mut events);
                }
            }
            Some("set" | "set_property") => {
                if let Some(parent) = field(action, "Parent") {
                    parents.insert(id.to_owned(), reference(parent));
                }
                if let Some(player) = players.get_mut(id) {
                    if let Some(parent) = field(action, "Parent") {
                        membership(
                            id,
                            player,
                            reference(parent),
                            service.as_deref(),
                            &mut events,
                        );
                    }
                    if let Some(value) = field(action, "Character") {
                        let character = reference(value);
                        let present = character
                            .as_ref()
                            .is_some_and(|id| available.get(id) == Some(&true));
                        change_character(id, player, character, present, &mut events);
                    }
                }
            }
            Some("remove" | "destroy") => {
                let roots = action["ids"]
                    .as_array()
                    .map(|ids| ids.iter().filter_map(Json::as_str).collect())
                    .unwrap_or_else(|| HashSet::from([id]));
                for (id, present) in &mut available {
                    if *present && inside(ctx, &parents, id, &roots) {
                        *present = false;
                    }
                }
                let mut removed = players
                    .keys()
                    .filter(|id| inside(ctx, &parents, id, &roots))
                    .cloned()
                    .collect::<Vec<_>>();
                removed.sort();
                for id in removed {
                    let mut player = players.remove(&id).unwrap();
                    membership(&id, &mut player, None, service.as_deref(), &mut events);
                }
            }
            _ => {}
        }
        if !events.is_empty() {
            receipt.events.insert(index, events);
        }
    }
    // A reflected lifecycle signal may accompany its state change. Use its
    // genuine arguments (including a supplied exit reason), but deliver once.
    let mut indices = receipt.events.keys().copied().collect::<Vec<_>>();
    indices.sort_unstable();
    for index in indices {
        for event in receipt.events.get_mut(&index).unwrap() {
            if event.target.is_none() {
                continue;
            }
            if let Some(action) = actions.iter().filter(|a| fresh(a)).find(|a| {
                a["kind"] == "event"
                    && a["id"].as_str() == Some(event.source.as_str())
                    && a["name"].as_str() == Some(event.name)
                    && a["arguments"]
                        .as_array()
                        .and_then(|v| v.first())
                        .and_then(reference)
                        == event.target
                    && !receipt
                        .consumed
                        .contains(&a["sequence"].as_u64().unwrap_or(0))
            }) {
                event.explicit_args = action["arguments"].as_array().cloned();
                receipt
                    .consumed
                    .insert(action["sequence"].as_u64().unwrap_or(0));
            }
        }
    }
    receipt.next = Some(players);
    receipt
}

pub(super) fn consumes_event(
    lua: &Lua,
    ctx: &WorldRef,
    sequence: u64,
    id: &str,
    name: &str,
    args: &[Json],
) -> bool {
    if !matches!(
        name,
        "CharacterAdded" | "CharacterRemoving" | "PlayerAdded" | "PlayerRemoving"
    ) {
        return false;
    }
    let state = state(lua, ctx);
    let mut state = state.borrow_mut();
    if state.consumed.remove(&sequence) {
        return true;
    }
    // A reflected spawn cannot wake a waiter with nil while the matching
    // Character reference is still unresolved. The model receipt completes it.
    name == "CharacterAdded"
        && state.players.get(id).is_some_and(|player| {
            player.character.is_some()
                && player.announced.is_none()
                && args.first().and_then(reference) == player.character
        })
}

pub(super) fn is_character(ctx: &WorldRef, id: &str, key: &str) -> bool {
    key == "Character" && ctx.borrow().nodes.class_name(id) == Some("Player")
}

pub(super) fn local_character(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: Option<&Json>,
    new: &Json,
) -> mlua::Result<()> {
    let state = state(lua, ctx);
    let mut events = Vec::new();
    {
        let mut state = state.borrow_mut();
        let player = state.players.entry(id.into()).or_insert_with(|| {
            let character = old.and_then(reference);
            Player {
                parent: ctx.borrow().nodes.parent(id).flatten(),
                announced: character.as_ref().filter(|id| model(ctx, id)).cloned(),
                character,
            }
        });
        let character = reference(new);
        let available = character.as_ref().is_some_and(|id| model(ctx, id));
        change_character(id, player, character, available, &mut events);
    }
    for event in events {
        event.publish(lua, ctx)?;
    }
    Ok(())
}

pub(super) fn local_parent(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: Option<String>,
    new: Option<String>,
) -> mlua::Result<()> {
    if ctx.borrow().nodes.class_name(id) != Some("Player") {
        return Ok(());
    }
    let state = state(lua, ctx);
    let service = ctx.borrow().services.get("Players").cloned();
    let mut events = Vec::new();
    {
        let mut state = state.borrow_mut();
        let player = state.players.entry(id.into()).or_insert_with(|| Player {
            parent: old,
            ..Player::default()
        });
        membership(id, player, new, service.as_deref(), &mut events);
    }
    for event in events {
        event.publish(lua, ctx)?;
    }
    Ok(())
}

pub(super) fn forget(lua: &Lua, ctx: &WorldRef, id: &str) {
    state(lua, ctx).borrow_mut().players.remove(id);
}
