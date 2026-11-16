use super::*;

#[test]
fn actor_budget_is_reserved_while_root_frame_keeps_a_share() {
    let total = Duration::from_millis(60);
    let (root, actors) = split_pump_budget(total, true);
    assert_eq!(root + actors, total);
    assert!(root > Duration::ZERO && actors > Duration::ZERO);

    let (root_without_actors, actor_budget_without_actors) = split_pump_budget(total, false);
    assert_eq!(root_without_actors, total);
    assert_eq!(actor_budget_without_actors, Duration::ZERO);
}
