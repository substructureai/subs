use super::*;
use crate::testing::{round_trip, World};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "tests/proptest-regressions"
        ))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_command_sequences_hold_every_law(
        ops in crate::testing::ops()
    ) {
        let mut world = World::new();
        for op in ops {
            world.step(op);
            let state = &world.agg.state;
            let commands = world.trace.join("\n");

            if let Err(violation) = state.check_invariants() {
                prop_assert!(false, "{violation}\ncommands:\n{commands}");
            }

            prop_assert_eq!(
                serde_json::to_value(world.replayed()).unwrap(),
                serde_json::to_value(state).unwrap(),
                "replay diverged\ncommands:\n{}", commands
            );

            let reloaded = round_trip(state);
            prop_assert_eq!(
                serde_json::to_value(&reloaded).unwrap(),
                serde_json::to_value(state).unwrap(),
                "snapshot round trip changed the state\ncommands:\n{}", commands
            );

            let live_plan = schedule::plan(state, world.plan_now);
            prop_assert_eq!(
                &live_plan, &schedule::plan(&reloaded, world.plan_now),
                "the plan read differently after a round trip\ncommands:\n{}", commands
            );

            if !world.cancelled {
                let ready: Vec<&ScheduleStep> =
                    live_plan.iter().filter(|s| !s.is_sweep()).collect();
                prop_assert!(
                    ready.is_empty(),
                    "the schedule tail left work ready: {ready:?}\ncommands:\n{commands}"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "tests/proptest-regressions"
        ))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn a_session_reloaded_from_its_log_is_the_session_that_ran(ops in crate::testing::ops()) {
        crate::testing::check(ops, std::sync::Arc::new(crate::MemoryStore::default()))?;
    }
}

#[test]
fn the_random_sequences_reach_every_kind_of_event() {
    use proptest::strategy::ValueTree;
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    for _ in 0..600 {
        let ops = crate::testing::ops()
            .new_tree(&mut runner)
            .unwrap()
            .current();
        let mut world = World::stored(std::sync::Arc::new(crate::MemoryStore::default()));
        for op in ops {
            world.step(op);
        }
        for (_, _, event) in &world.log {
            let kind = serde_json::to_value(event).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string();
            *seen.entry(kind).or_default() += 1;
        }
    }
    let mut kinds: Vec<_> = seen.iter().collect();
    kinds.sort();
    for kind in [
        "message.new",
        "branch.started",
        "branch.closed",
        "branch.moved",
        "context.placed",
        "context.replaced",
        "llm.call.completed",
        "llm.call.errored",
        "tool.call.completed",
        "system1.call.completed",
        "subagent.requested",
        "connector.sync.completed",
        "session.interrupted",
        "session.interrupt_resumed",
        "turn.joined",
        "call.voided",
    ] {
        assert!(
            seen.get(kind).copied().unwrap_or_default() >= 3,
            "{kind}: {kinds:?}"
        );
    }
}
