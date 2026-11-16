use super::*;
use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
use rove_client_core::{schema::Schema, world::WorldStore};

#[test]
fn script_eligibility_reads_character_and_execution_flags_from_shared_world() {
    let snapshot = json!({"version":1,"sequence":1,"local_player":"player",
        "services":{"Workspace":"workspace","Players":"players"},"nodes":[
            {"id":"game","class_name":"DataModel","name":"game"},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game"},
            {"id":"players","class_name":"Players","name":"Players","parent":"game"},
            {"id":"player","class_name":"Player","name":"Local","parent":"players",
                "properties":{"Character":{"$ref":"character"}}},
            {"id":"character","class_name":"Model","name":"Character","parent":"workspace"},
            {"id":"animate","class_name":"LocalScript","name":"Animate","parent":"character"},
            {"id":"client","class_name":"Script","name":"Client","parent":"workspace",
                "properties":{"RunContext":{"$enum":{"type":"RunContext","name":"Client","value":2}}}},
            {"id":"disabled","class_name":"Script","name":"Disabled","parent":"workspace",
                "properties":{"RunContext":2,"Enabled":false}},
            {"id":"server","class_name":"Script","name":"Server","parent":"workspace",
                "properties":{"RunContext":1}}
        ]});
    let store = WorldStore::from_snapshot(&snapshot).unwrap();
    let schema = Schema { version:3, flags:0, enums:vec![], classes:vec![], properties:vec![],
        events:vec![], strings:vec![], names:vec![], lr_types:vec![], parent_property_id:0, decoded_bytes:0 };
    let projection = ProjectionContext::new(&schema, HashMap::new()).unwrap();
    let view = SharedWorldView::projected(store.view(), projection);
    for shared in [false, true] {
        let ctx = Rc::new(RefCell::new(world::World::from_snapshot(&snapshot, shared.then_some(&view)).unwrap()));
        if shared {
            let world = ctx.borrow();
            assert!(world.nodes.get("player").unwrap().projected.is_some());
            assert!(world.nodes.get("client").unwrap().properties.is_empty());
        }
        let (player, character) = local_player_and_character(&ctx);
        assert_eq!(player.as_deref(), Some("player"));
        assert_eq!(character.as_deref(), Some("character"));
        for id in ["animate", "client"] {
            assert_eq!(eligible(&ctx, None, id, player.as_deref(), character.as_deref()), (true,"eligible"));
        }
        assert_eq!(eligible(&ctx, None, "disabled", player.as_deref(), character.as_deref()), (false,"disabled"));
        assert_eq!(eligible(&ctx, None, "server", player.as_deref(), character.as_deref()), (false,"script_runcontext_not_client"));
        ctx.borrow_mut().nodes.get_mut("client").unwrap().local_overrides.insert("Disabled".into(), json!(true));
        assert_eq!(eligible(&ctx, None, "client", player.as_deref(), character.as_deref()), (false,"disabled"));
    }
}
