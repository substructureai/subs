use std::collections::HashMap;

use chrono::Utc;
use substructure_session::command::TurnTarget;
use substructure_session::command::{CommandPayload, SessionError, SettleError};
use substructure_session::decision::Action;
use substructure_session::events::EventPayload as Logged;
use substructure_session::protocol::{
    AgentConfig, ClientMessage, ClientPayload, Content, DraftMessage, EffectKind, ErrorInfo,
    Issuer, QueueMode, Requester, RetryPolicy, Role, SessionOwner, Subject,
};
use substructure_session::protocol::{ClientAction, ClientAppend, WorkerState};
use substructure_session::{settle, Command, MemoryStore, Session, Settings, Store, Work};

const TENANT: &str = "tenant-a";

fn create(id: &str) -> Command {
    Command::system(
        id,
        TENANT,
        CommandPayload::CreateSession {
            agent_id: "agent-1".into(),
            owner: SessionOwner {
                tenant_id: TENANT.into(),
                requester: Requester::new(
                    Subject::new(Issuer::app(), "user-1".to_string()),
                    Default::default(),
                ),
                metadata: HashMap::new(),
            },
            ancestry: vec![],
            worker_retry: RetryPolicy::no_retry(),
            agent: None,
            worker: None,
            events: None,
            state: None,
        },
    )
}

fn interrupt(id: &str, n: usize) -> Command {
    Command::system(
        id,
        TENANT,
        CommandPayload::Interrupt {
            interrupt_id: format!("i{n}"),
            reason: "test".into(),
            payload: serde_json::Value::Null,
        },
    )
}

fn load(store: &MemoryStore, settings: Settings) -> Session {
    Session::load(store, "s1", TENANT, settings).unwrap()
}

#[test]
fn a_command_is_written_and_its_work_follows_from_the_events() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    let ran = session.run(&store, Utc::now(), create("c1")).unwrap();

    assert_eq!(ran.result.unwrap().seq, session.seq());
    let written = ran.written.unwrap();
    assert_eq!(written.version, 1);
    assert_eq!(store.read(0).unwrap().len(), 1);
    assert!(matches!(
        session.work(&written.events).as_slice(),
        [Work::AskWorker { work, .. }] if work.kind == EffectKind::Decision
    ));
}

#[test]
fn a_resent_command_gets_its_first_answer_and_writes_nothing() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    let first = session.run(&store, Utc::now(), create("c1")).unwrap();
    let again = session.run(&store, Utc::now(), create("c1")).unwrap();

    assert_eq!(first.result, again.result);
    assert!(again.written.is_none());
    assert_eq!(store.read(0).unwrap().len(), 1);
}

#[test]
fn a_rejected_command_is_written_with_its_error() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    session.run(&store, Utc::now(), create("c1")).unwrap();
    let ran = session.run(&store, Utc::now(), create("c2")).unwrap();

    assert_eq!(
        ran.result.unwrap_err().error,
        SessionError::SessionAlreadyCreated
    );
    assert_eq!(store.read(0).unwrap().len(), 2);
}

#[test]
fn a_command_refused_by_a_session_never_created_writes_nothing() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    let ran = session
        .run(
            &store,
            Utc::now(),
            Command::system("c1", TENANT, CommandPayload::CancelSession),
        )
        .unwrap();

    assert_eq!(
        ran.result.unwrap_err().error,
        SessionError::SessionNotCreated
    );
    assert!(ran.written.is_none());
    assert!(store.read(0).unwrap().is_empty());
}

#[test]
fn a_kept_session_catches_up_when_another_writer_got_there_first() {
    let store = MemoryStore::new();
    let mut kept = load(&store, Settings::default());
    kept.run(&store, Utc::now(), create("c1")).unwrap();
    let mut other = load(&store, Settings::default());
    other.run(&store, Utc::now(), interrupt("c2", 1)).unwrap();

    let ran = kept.run(&store, Utc::now(), interrupt("c3", 2)).unwrap();
    assert_eq!(ran.written.unwrap().version, 3);
    assert_eq!(kept.version(), 3);
    assert!(kept.seen(&"c2".into()).is_some());
}

#[test]
fn writers_racing_on_one_store_each_land_exactly_once() {
    let store = MemoryStore::new();
    load(&store, Settings::default())
        .run(&store, Utc::now(), create("c0"))
        .unwrap();
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let store = store.clone();
            std::thread::spawn(move || {
                for n in 0..10 {
                    let mut session = load(&store, Settings::default());
                    session
                        .run(
                            &store,
                            Utc::now(),
                            interrupt(&format!("t{t}-{n}"), t * 10 + n),
                        )
                        .unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }

    let session = load(&store, Settings::default());
    assert_eq!(session.version(), 41);
    for t in 0..4 {
        for n in 0..10 {
            assert!(session.seen(&format!("t{t}-{n}").into()).is_some());
        }
    }
}

#[test]
fn a_snapshot_gives_the_same_session_as_the_log() {
    let store = MemoryStore::new();
    let settings = Settings {
        snapshot_every: 2,
        ..Settings::default()
    };
    let mut session = load(&store, settings.clone());
    session.run(&store, Utc::now(), create("c1")).unwrap();
    session.run(&store, Utc::now(), interrupt("c2", 1)).unwrap();
    session.run(&store, Utc::now(), interrupt("c3", 2)).unwrap();

    assert_eq!(store.latest().unwrap().unwrap().version(), 2);
    let loaded = load(&store, settings);
    assert_eq!(loaded.version(), 3);
    assert_eq!(
        serde_json::to_value(loaded.state()).unwrap(),
        serde_json::to_value(session.state()).unwrap()
    );
    let decoded = Session::from_json(&session.to_json().unwrap()).unwrap();
    assert_eq!(decoded.seq(), session.seq());
}

#[test]
fn an_entry_records_each_command_without_its_payload() {
    let store = MemoryStore::new();
    load(&store, Settings::default())
        .run(&store, Utc::now(), create("c1"))
        .unwrap();

    let entry: serde_json::Value =
        serde_json::from_slice(&store.read(0).unwrap()[0].to_json().unwrap()).unwrap();
    let command = &entry["commands"][0];
    assert_eq!(command["id"], "c1");
    assert_eq!(command["kind"], "create_session");
    assert_eq!(command["caller"], "system");
    assert!(command.get("payload").is_none());
    for event in entry["events"].as_array().unwrap() {
        assert_eq!(event["command_id"], "c1");
    }
}

#[test]
fn the_same_inputs_make_the_same_ids() {
    let now = Utc::now();
    let command = create("c1");
    let (a, b) = (MemoryStore::new(), MemoryStore::new());
    let x = load(&a, Settings::default())
        .run(&a, now, command.clone())
        .unwrap()
        .written
        .unwrap();
    let y = load(&b, Settings::default())
        .run(&b, now, command)
        .unwrap()
        .written
        .unwrap();

    assert_eq!(
        serde_json::to_value(&x.events).unwrap(),
        serde_json::to_value(&y.events).unwrap()
    );
    let ids: std::collections::HashSet<String> =
        x.events.iter().map(|e| e.id.to_string()).collect();
    assert_eq!(ids.len(), x.events.len());
    assert!(x.events.iter().all(|e| match &e.payload {
        substructure_session::events::EventPayload::DecisionQueued(q) => !ids.contains(&q.id),
        _ => true,
    }));
}

#[test]
fn a_command_id_is_remembered_only_within_the_retention_window() {
    let store = MemoryStore::new();
    let settings = Settings {
        retention: chrono::Duration::days(1),
        ..Settings::default()
    };
    let mut session = load(&store, settings);
    let start = Utc::now();
    session.run(&store, start, create("c1")).unwrap();
    assert!(session
        .run(&store, start + chrono::Duration::hours(2), create("c1"))
        .unwrap()
        .written
        .is_none());

    session
        .run(
            &store,
            start + chrono::Duration::days(2),
            interrupt("c2", 1),
        )
        .unwrap();
    let again = session
        .run(&store, start + chrono::Duration::days(2), create("c1"))
        .unwrap();
    assert_eq!(
        again.result.unwrap_err().error,
        SessionError::SessionAlreadyCreated
    );
}

#[test]
fn events_are_read_from_a_seq() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    session.run(&store, Utc::now(), create("c1")).unwrap();
    session.run(&store, Utc::now(), interrupt("c2", 1)).unwrap();

    let all = Session::events(&store, 0, 1000).unwrap();
    assert_eq!(all.len() as u64, session.seq());
    let tail = Session::events(&store, 2, 2).unwrap();
    assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4]);
}

#[test]
fn work_that_is_settled_is_not_sent_again() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    let dispatched = session
        .run(&store, Utc::now(), create("c1"))
        .unwrap()
        .written
        .unwrap()
        .events;
    let Work::AskWorker { work, .. } = session.work(&dispatched).remove(0) else {
        panic!("a decision goes out");
    };
    let settled = session
        .run(
            &store,
            Utc::now(),
            settle(
                &work,
                TENANT,
                SettleError::new(ErrorInfo::internal("worker down"), false).into(),
            ),
        )
        .unwrap();

    assert!(settled.result.is_ok());
    assert!(!session
        .work(&dispatched)
        .iter()
        .any(|w| matches!(w, Work::AskWorker { .. })));
}

#[test]
fn open_work_is_what_a_restart_sends_again() {
    let store = MemoryStore::new();
    load(&store, Settings::default())
        .run(&store, Utc::now(), create("c1"))
        .unwrap();

    assert!(matches!(
        load(&store, Settings::default()).open_work().as_slice(),
        [Work::AskWorker { work, .. }] if work.kind == EffectKind::Decision
    ));
}

fn say(id: &str, turn: &str, queue: QueueMode) -> Command {
    Command::system(
        id,
        TENANT,
        CommandPayload::SubmitClientPayload {
            payload: ClientPayload::Message(ClientMessage {
                message: DraftMessage {
                    id: None,
                    role: Role::User,
                    content: Some(Content::Text(id.to_string())),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    reasoning: None,
                },
                stream: false,
            }),
            turn: TurnTarget::Open(turn.to_string()),
            queue,
            metadata: Default::default(),
        },
    )
}

fn say_with(id: &str, turn: &str, queue: QueueMode, metadata: &[(&str, &str)]) -> Command {
    let mut command = say(id, turn, queue);
    if let CommandPayload::SubmitClientPayload { metadata: m, .. } = &mut command.payload {
        *m = metadata
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    }
    command
}

fn finish_open_decision(store: &MemoryStore, session: &mut Session, id: &str) {
    let decision_id = session
        .open_work()
        .into_iter()
        .find_map(|w| match w {
            Work::AskWorker { work, .. } => Some(work.id),
            _ => None,
        })
        .expect("a decision is open");
    let ran = session
        .run(
            store,
            Utc::now(),
            Command::system(
                id,
                TENANT,
                CommandPayload::SubmitWorkerDecision {
                    decision_id,
                    transcript: vec![],
                    actions: vec![Action::Done {
                        data: serde_json::Value::Null,
                    }],
                    state: None,
                    agent: None,
                },
            ),
        )
        .unwrap();
    assert!(ran.result.is_ok(), "{:?}", ran.result);
}

#[test]
fn a_voided_subagent_call_mails_its_child_a_cancel() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    session.run(&store, Utc::now(), create("c1")).unwrap();
    finish_open_decision(&store, &mut session, "d0");
    session
        .run(
            &store,
            Utc::now(),
            Command::system(
                "spawn",
                TENANT,
                CommandPayload::RequestSubagent {
                    session_id: None,
                    agent_id: "agent-2".into(),
                    tool_call_id: "call-sa".into(),
                    message: None,
                    retry: RetryPolicy::no_retry(),
                    decision_id: "d-spawn".into(),
                    mode: None,
                },
            ),
        )
        .unwrap();
    let child = session
        .state()
        .subagent("call-sa")
        .unwrap()
        .session_id
        .clone();

    let cancelled = session
        .run(
            &store,
            Utc::now(),
            Command::system("cancel", TENANT, CommandPayload::CancelSession),
        )
        .unwrap()
        .written
        .unwrap()
        .events;
    let mail = session.mail(&cancelled);
    assert!(
        mail.iter()
            .any(|m| m.to == child && matches!(m.command.payload, CommandPayload::CancelSession)),
        "got {mail:?}"
    );
}

fn click(id: &str, turn: &str) -> Command {
    Command::system(
        id,
        TENANT,
        CommandPayload::SubmitClientPayload {
            payload: ClientPayload::Action(ClientAction {
                name: "approve".into(),
                args: None,
            }),
            turn: TurnTarget::Open(turn.to_string()),
            queue: QueueMode::Refuse,
            metadata: Default::default(),
        },
    )
}

#[test]
fn an_action_answers_with_the_turn_it_runs_in() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    session.run(&store, Utc::now(), create("c1")).unwrap();
    finish_open_decision(&store, &mut session, "d0");

    let idle = session.run(&store, Utc::now(), click("a1", "ta")).unwrap();
    assert_eq!(idle.result.unwrap().turn_id.as_deref(), Some("ta"));
    finish_open_decision(&store, &mut session, "d1");
    finish_open_decision(&store, &mut session, "d2");

    session
        .run(&store, Utc::now(), say("m1", "t1", QueueMode::Refuse))
        .unwrap();
    let joined = session.run(&store, Utc::now(), click("a2", "tb")).unwrap();
    assert_eq!(
        joined.result.unwrap().turn_id.as_deref(),
        Some("t1"),
        "a click mid-turn joins the running turn"
    );
}

#[test]
fn a_decision_carries_the_metadata_of_its_turn() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    session.run(&store, Utc::now(), create("c1")).unwrap();
    finish_open_decision(&store, &mut session, "d0");

    session
        .run(&store, Utc::now(), say("m1", "t1", QueueMode::Refuse))
        .unwrap();
    session
        .run(
            &store,
            Utc::now(),
            say_with(
                "m2",
                "t2",
                QueueMode::Merge,
                &[("ref", "m2"), ("room", "r1")],
            ),
        )
        .unwrap();
    session
        .run(
            &store,
            Utc::now(),
            say_with("m3", "t3", QueueMode::Merge, &[("ref", "m3")]),
        )
        .unwrap();
    finish_open_decision(&store, &mut session, "d1");
    finish_open_decision(&store, &mut session, "d2");

    let turn = session
        .open_work()
        .into_iter()
        .find_map(|w| match w {
            Work::AskWorker { request, .. } => request.turn,
            _ => None,
        })
        .expect("the merged turn asks the worker");
    assert_eq!(turn.id, "t2");
    assert_eq!(
        turn.metadata,
        [("ref", "m3"), ("room", "r1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<std::collections::BTreeMap<_, _>>(),
        "a later key wins"
    );
}

#[test]
fn a_merged_submit_answers_with_the_turn_it_joined() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    session.run(&store, Utc::now(), create("c1")).unwrap();

    let running = session
        .run(&store, Utc::now(), say("m1", "t1", QueueMode::Refuse))
        .unwrap();
    let first = session
        .run(&store, Utc::now(), say("m2", "t2", QueueMode::Merge))
        .unwrap();
    let second = session
        .run(&store, Utc::now(), say("m3", "t3", QueueMode::Merge))
        .unwrap();

    let running = running.result.unwrap();
    assert_eq!(
        (running.turn_id.as_deref(), running.queued),
        (Some("t1"), false)
    );
    let first = first.result.unwrap();
    assert_eq!((first.turn_id.as_deref(), first.queued), (Some("t2"), true));
    let second = second.result.unwrap();
    assert_eq!(
        (second.turn_id.as_deref(), second.queued),
        (Some("t2"), true)
    );
}

#[test]
fn a_decision_sent_again_after_a_restart_sees_the_config_written_before_it_went_out() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    let created = session
        .run(&store, Utc::now(), create("c1"))
        .unwrap()
        .written
        .unwrap()
        .events;
    let Work::AskWorker { work: start, .. } = session.work(&created).remove(0) else {
        panic!("session.start goes out");
    };
    session
        .run(&store, Utc::now(), say("m1", "t1", QueueMode::Refuse))
        .unwrap();
    let config = AgentConfig {
        llm: Some("claude".into()),
        model: "m".into(),
        ..Default::default()
    };
    session
        .run(
            &store,
            Utc::now(),
            Command::system(
                "start",
                TENANT,
                CommandPayload::SubmitWorkerDecision {
                    decision_id: start.id.clone(),
                    transcript: vec![],
                    actions: vec![],
                    state: None,
                    agent: Some(config),
                },
            ),
        )
        .unwrap();

    let resent = load(&store, Settings::default()).open_work();

    let Some(Work::AskWorker { request, .. }) = resent.into_iter().next() else {
        panic!("the message's decision is open");
    };
    assert_eq!(request.agent.map(|a| a.model), Some("m".to_string()));
    assert!(
        request.proposed.actions.iter().all(|a| !matches!(
            a,
            substructure_session::protocol::DecisionAction::Interrupt { .. }
        )),
        "the proposal is not the unconfigured pause: {:?}",
        request.proposed.actions
    );
}

#[test]
fn a_restart_resends_a_decision_exactly_as_it_first_went_out() {
    use substructure_session::delivery::{Restart, WorkReader};
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    let created = session
        .run(&store, Utc::now(), create("c1"))
        .unwrap()
        .written
        .unwrap()
        .events;
    let original = session.work(&created);
    session
        .run(&store, Utc::now(), say("m1", "t1", QueueMode::Refuse))
        .unwrap();

    let mut restarted = load(&store, Settings::default());
    let reader = WorkReader::new("work", Restart::ResendOpen);
    let resent = restarted
        .next_work(&store, &reader, true)
        .unwrap()
        .batch
        .unwrap()
        .items;

    let request = |work: &[Work]| {
        work.iter()
            .find_map(|w| match w {
                Work::AskWorker { request, .. } => Some(serde_json::to_value(request).unwrap()),
                _ => None,
            })
            .expect("a decision")
    };
    assert_eq!(request(&resent), request(&original));
}

#[test]
fn a_run_says_when_the_wake_time_moved() {
    let store = MemoryStore::new();
    let mut session = load(&store, Settings::default());
    let created = session.run(&store, Utc::now(), create("c1")).unwrap();
    assert_eq!(created.wake_moved, session.wake_at().is_some());

    let resent = session.run(&store, Utc::now(), create("c1")).unwrap();
    assert!(!resent.wake_moved);
}

mod delivery {
    use super::*;
    use chrono::{DateTime, TimeDelta};
    use substructure_session::delivery::{
        AtLimit, Delivered, Failure, Restart, Start, Subscription, Unsent, WorkReader,
    };
    use substructure_session::events::EventPayload;

    fn created() -> (MemoryStore, Session, DateTime<Utc>) {
        let store = MemoryStore::new();
        let mut session = load(&store, Settings::default());
        let now = Utc::now();
        session.run(&store, now, create("c1")).unwrap();
        (store, session, now)
    }

    fn outbox(at_limit: AtLimit, max_attempts: u32) -> Subscription {
        let mut s = Subscription::new("outbox", Start::Beginning, at_limit);
        s.retry.max_attempts = max_attempts;
        s
    }

    fn failed(item: usize) -> Delivered {
        Delivered::Failed(Failure {
            item,
            error: "503".into(),
            permanent: false,
            retry_after: None,
        })
    }

    fn logged(store: &MemoryStore) -> Vec<String> {
        Session::events(store, 0, usize::MAX)
            .unwrap()
            .iter()
            .filter(|e| e.payload.is_delivery() || matches!(e.payload, EventPayload::WorkUnsent(_)))
            .map(|e| e.payload_type())
            .collect()
    }

    fn fail_first(
        store: &MemoryStore,
        session: &mut Session,
        sub: &Subscription,
        now: DateTime<Utc>,
    ) {
        let batch = session.next_events(store, sub, now).unwrap().batch.unwrap();
        session
            .ack_events(store, sub, &batch, failed(0), now)
            .unwrap();
    }

    #[test]
    fn a_failure_is_logged_and_waits_for_its_backoff() {
        let (store, mut session, now) = created();
        let sub = outbox(AtLimit::Skip, 10);
        fail_first(&store, &mut session, &sub, now);

        assert_eq!(logged(&store), vec!["delivery.failed"]);
        let delivery = session.delivery("outbox").unwrap();
        assert_eq!(delivery.attempts, 1);
        assert_eq!(delivery.retry_at, Some(now + TimeDelta::seconds(2)));
        assert_eq!(session.wake_at(), delivery.retry_at);
        assert!(session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .is_none());
        let again = session
            .next_events(&store, &sub, now + TimeDelta::seconds(2))
            .unwrap()
            .batch
            .unwrap();
        assert!(again.items.iter().all(|e| !e.payload.is_delivery()));
        assert_eq!(again.attempts, 1);
    }

    #[test]
    fn a_stale_report_of_a_failure_is_ignored() {
        let (store, mut session, now) = created();
        let sub = outbox(AtLimit::Skip, 10);
        let batch = session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .unwrap();
        let mut other = load(&store, Settings::default());
        session
            .ack_events(&store, &sub, &batch, failed(0), now)
            .unwrap();
        other
            .ack_events(&store, &sub, &batch, failed(0), now)
            .unwrap();

        assert_eq!(logged(&store), vec!["delivery.failed"]);
    }

    #[test]
    fn success_after_failures_is_logged_as_recovered() {
        let (store, mut session, now) = created();
        let sub = outbox(AtLimit::Skip, 10);
        fail_first(&store, &mut session, &sub, now);
        let later = now + TimeDelta::seconds(5);
        let next = session.next_events(&store, &sub, later).unwrap();
        let batch = next.batch.unwrap();
        let written = session
            .ack_events(&store, &sub, &batch, Delivered::All, later)
            .unwrap();

        assert_eq!(written.len(), 1);
        assert_eq!(
            logged(&store),
            vec!["delivery.failed", "delivery.recovered"]
        );
        assert!(!session.delivery("outbox").unwrap().failing());
        let after = session.next_events(&store, &sub, later).unwrap();
        assert!(after.batch.is_none());
        assert_eq!(store.cursors().unwrap().get("outbox"), Some(&session.seq()));
    }

    fn say_more(store: &MemoryStore, session: &mut Session, n: usize, at: DateTime<Utc>) {
        let command = match n % 2 {
            1 => interrupt(&format!("more{n}"), n),
            _ => Command::system(
                format!("more{n}"),
                TENANT,
                CommandPayload::ResumeInterrupt {
                    interrupt_id: format!("i{}", n - 1),
                    payload: serde_json::Value::Null,
                },
            ),
        };
        let before = session.seq();
        session.run(store, at, command).unwrap();
        assert!(session.seq() > before, "step {n} wrote an event");
    }

    #[test]
    fn a_reader_that_stays_down_skips_each_later_event_at_once_until_one_gets_through() {
        let (store, mut session, now) = created();
        let mut sub = Subscription::new("chat", Start::Beginning, AtLimit::Skip);
        sub.retry.total_timeout_secs = Some(60);
        fail_first(&store, &mut session, &sub, now);
        let late = now + TimeDelta::seconds(120);
        fail_first(&store, &mut session, &sub, late);
        while session
            .next_events(&store, &sub, late)
            .unwrap()
            .batch
            .is_some()
        {
            fail_first(&store, &mut session, &sub, late);
        }
        assert_eq!(
            logged(&store).last().map(String::as_str),
            Some("delivery.skipped")
        );

        say_more(&store, &mut session, 1, late);
        fail_first(&store, &mut session, &sub, late);
        assert_eq!(
            logged(&store).last().map(String::as_str),
            Some("delivery.skipped"),
            "the clock kept running, so the next event is written off at once"
        );

        say_more(&store, &mut session, 2, late);
        let batch = session
            .next_events(&store, &sub, late)
            .unwrap()
            .batch
            .unwrap();
        session
            .ack_events(&store, &sub, &batch, Delivered::All, late)
            .unwrap();
        assert_eq!(
            logged(&store).last().map(String::as_str),
            Some("delivery.recovered")
        );

        say_more(&store, &mut session, 3, late);
        fail_first(&store, &mut session, &sub, late);
        assert_eq!(
            logged(&store).last().map(String::as_str),
            Some("delivery.failed"),
            "a success starts the clock again"
        );
    }

    #[test]
    fn a_failure_points_at_the_item_that_failed() {
        let (store, mut session, now) = created();
        let sub = outbox(AtLimit::Skip, 1);
        let batch = session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .unwrap();
        let second = batch.items[1].seq;
        session
            .ack_events(&store, &sub, &batch, failed(1), now)
            .unwrap();

        assert_eq!(logged(&store), vec!["delivery.skipped"]);
        assert_eq!(session.delivery("outbox").unwrap().floor, second);
        let next = session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .unwrap();
        assert!(next.items[0].seq > second);
    }

    #[test]
    fn a_permanent_failure_gives_up_at_once() {
        let (store, mut session, now) = created();
        let sub = outbox(AtLimit::Skip, 10);
        let batch = session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .unwrap();
        let permanent = Delivered::Failed(Failure {
            item: 0,
            error: "channel_not_found".into(),
            permanent: true,
            retry_after: None,
        });
        session
            .ack_events(&store, &sub, &batch, permanent, now)
            .unwrap();

        assert_eq!(logged(&store), vec!["delivery.skipped"]);
    }

    #[test]
    fn a_parked_reader_waits_until_it_is_resumed() {
        let (store, mut session, now) = created();
        let sub = outbox(AtLimit::Park, 1);
        fail_first(&store, &mut session, &sub, now);
        let later = now + TimeDelta::days(1);
        assert!(session
            .next_events(&store, &sub, later)
            .unwrap()
            .batch
            .is_none());
        assert_eq!(session.wake_at(), None);

        session.resume_delivery(&store, "outbox", later).unwrap();
        assert_eq!(logged(&store), vec!["delivery.parked", "delivery.resumed"]);
        assert!(session
            .next_events(&store, &sub, later)
            .unwrap()
            .batch
            .is_some());
    }

    #[test]
    fn retries_end_when_the_retry_window_closes() {
        let (store, mut session, now) = created();
        let mut sub = outbox(AtLimit::Skip, 100);
        sub.retry.total_timeout_secs = Some(60);
        fail_first(&store, &mut session, &sub, now);
        fail_first(&store, &mut session, &sub, now + TimeDelta::seconds(61));

        assert_eq!(logged(&store), vec!["delivery.failed", "delivery.skipped"]);
    }

    #[test]
    fn a_failure_wakes_a_finished_session() {
        let (store, mut session, now) = created();
        let sub = outbox(AtLimit::Skip, 10);
        session
            .run(
                &store,
                now,
                Command::system("cancel", TENANT, CommandPayload::CancelSession),
            )
            .unwrap();
        fail_first(&store, &mut session, &sub, now);

        assert_eq!(session.wake_at(), Some(now + TimeDelta::seconds(2)));
    }

    #[test]
    fn batches_are_limited_in_size() {
        let (store, mut session, now) = created();
        let mut sub = outbox(AtLimit::Skip, 10);
        sub.batch = 2;
        let first = session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .unwrap();
        assert_eq!(first.items.len(), 2);
        session
            .ack_events(&store, &sub, &first, Delivered::All, now)
            .unwrap();
        let second = session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .unwrap();
        assert_eq!(second.items[0].seq, first.through + 1);
    }

    #[test]
    fn a_joined_reader_follows_only_sessions_it_joined() {
        let (store, mut session, now) = created();
        let sub = Subscription::new("outbox", Start::Joined, AtLimit::Skip);
        assert!(session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .is_none());

        session.join(&store, "outbox").unwrap();
        assert!(session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .is_none());
        session.run(&store, now, interrupt("c2", 1)).unwrap();
        let batch = session
            .next_events(&store, &sub, now)
            .unwrap()
            .batch
            .unwrap();
        assert!(batch
            .items
            .iter()
            .all(|e| e.command_id.as_deref() == Some("c2")));
    }

    #[test]
    fn joining_an_empty_session_sees_it_from_the_start() {
        let store = MemoryStore::new();
        let mut session = load(&store, Settings::default());
        let sub = Subscription::new("outbox", Start::Joined, AtLimit::Skip);
        session.join(&store, "outbox").unwrap();
        session.run(&store, Utc::now(), create("c1")).unwrap();

        let batch = session
            .next_events(&store, &sub, Utc::now())
            .unwrap()
            .batch
            .unwrap();
        assert_eq!(batch.items[0].seq, 1);
    }

    #[test]
    fn work_comes_in_batches_and_moves_the_cursor() {
        let (store, mut session, now) = created();
        let reader = WorkReader::new("work", Restart::ResendOpen);
        let batch = session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .unwrap();
        assert!(matches!(batch.items.as_slice(), [Work::AskWorker { .. }]));

        session
            .ack_work(&store, &reader, &batch, Vec::new(), now)
            .unwrap();
        assert!(session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .is_none());
        assert_eq!(store.cursors().unwrap().get("work"), Some(&session.seq()));
    }

    #[test]
    fn work_not_sent_is_sent_again_as_the_same_attempt() {
        let (store, mut session, now) = created();
        let reader = WorkReader::new("work", Restart::FromCursor);
        let batch = session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .unwrap();
        let id = batch.items[0].id().clone();
        session
            .ack_work(
                &store,
                &reader,
                &batch,
                vec![Unsent {
                    work: id.clone(),
                    error: "queue down".into(),
                    permanent: false,
                }],
                now,
            )
            .unwrap();

        assert_eq!(logged(&store), vec!["work.unsent"]);
        assert!(session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .is_none());
        let due = session.wake_at().unwrap();
        assert!(due > now);

        session.wake(&store, due).unwrap();
        let again = session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .unwrap();
        assert_eq!(again.items[0].id(), &id);
    }

    #[test]
    fn work_that_can_never_be_sent_fails_at_once() {
        let (store, mut session, now) = created();
        let reader = WorkReader::new("work", Restart::FromCursor);
        let batch = session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .unwrap();
        let id = batch.items[0].id().clone();
        session
            .ack_work(
                &store,
                &reader,
                &batch,
                vec![Unsent {
                    work: id.clone(),
                    error: "a blob that cannot be read".into(),
                    permanent: true,
                }],
                now,
            )
            .unwrap();

        assert!(!session.is_open(&id));
        assert!(logged(&store).is_empty());
        let errored = Session::events(&store, 0, usize::MAX)
            .unwrap()
            .into_iter()
            .any(|e| matches!(e.payload, EventPayload::DecisionErrored(ref d) if !d.retryable));
        assert!(errored);
    }

    #[test]
    fn a_restart_sends_all_open_work_again() {
        let (store, mut session, now) = created();
        let reader = WorkReader::new("work", Restart::ResendOpen);
        let batch = session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .unwrap();
        session
            .ack_work(&store, &reader, &batch, Vec::new(), now)
            .unwrap();

        let resent = session
            .next_work(&store, &reader, true)
            .unwrap()
            .batch
            .unwrap();
        assert_eq!(resent.items[0].id(), batch.items[0].id());
        let kept = WorkReader::new("work", Restart::FromCursor);
        assert!(session
            .next_work(&store, &kept, true)
            .unwrap()
            .batch
            .is_none());
    }

    #[test]
    fn a_decision_names_its_attempt() {
        let (store, mut session, _) = created();
        let reader = WorkReader::new("work", Restart::FromCursor);
        let batch = session
            .next_work(&store, &reader, false)
            .unwrap()
            .batch
            .unwrap();
        assert_eq!(batch.items[0].id().attempt, Some(0));
    }
}

mod branches {
    use super::*;

    fn draft(id: &str, role: Role, text: &str) -> DraftMessage {
        DraftMessage {
            id: Some(id.to_string()),
            role,
            content: Some(Content::Text(text.to_string())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn open_decision(session: &Session) -> String {
        session
            .open_work()
            .into_iter()
            .find_map(|w| match w {
                Work::AskWorker { work, .. } => Some(work.id),
                _ => None,
            })
            .expect("a decision is open")
    }

    fn decide(
        store: &MemoryStore,
        session: &mut Session,
        n: usize,
        transcript: Vec<DraftMessage>,
        state: Option<serde_json::Value>,
    ) -> Vec<Logged> {
        let decision_id = open_decision(session);
        let ran = session
            .run(
                store,
                Utc::now(),
                Command::system(
                    format!("decide-{n}"),
                    TENANT,
                    CommandPayload::SubmitWorkerDecision {
                        decision_id,
                        transcript,
                        actions: vec![Action::Done {
                            data: serde_json::Value::Null,
                        }],
                        state: state.map(WorkerState::from),
                        agent: None,
                    },
                ),
            )
            .unwrap();
        assert!(ran.result.is_ok(), "{:?}", ran.result);
        ran.written
            .map(|e| e.events.into_iter().map(|e| e.payload).collect())
            .unwrap_or_default()
    }

    fn turn(store: &MemoryStore, session: &mut Session, n: usize) {
        session
            .run(
                store,
                Utc::now(),
                say(&format!("say-{n}"), &format!("t{n}"), QueueMode::Refuse),
            )
            .unwrap();
    }

    fn started() -> (MemoryStore, Session) {
        let store = MemoryStore::new();
        let mut session = load(&store, Settings::default());
        session.run(&store, Utc::now(), create("c1")).unwrap();
        decide(&store, &mut session, 0, vec![], None);
        turn(&store, &mut session, 1);
        decide(
            &store,
            &mut session,
            1,
            vec![
                draft("u1", Role::User, "hi"),
                draft("a1", Role::Assistant, "hello"),
            ],
            Some(serde_json::json!({ "branch": "first" })),
        );
        turn(&store, &mut session, 2);
        decide(
            &store,
            &mut session,
            2,
            vec![
                draft("u1", Role::User, "hi"),
                draft("b1", Role::User, "hi, edited"),
            ],
            Some(serde_json::json!({ "branch": "edited" })),
        );
        (store, session)
    }

    fn state_value(session: &Session) -> serde_json::Value {
        session.state().at_head().resolve_state_for().0
    }

    #[test]
    fn an_edit_leaves_only_the_active_branch_in_memory() {
        let (store, session) = started();

        assert_eq!(session.state().head_id.as_deref(), Some("b1"));
        assert!(session.state().has_node("u1"));
        assert!(
            !session.state().has_node("a1"),
            "the old branch is unloaded"
        );
        assert_eq!(
            store.tied("a1").unwrap().len(),
            2,
            "its message and its state are in the log"
        );
        assert_eq!(
            state_value(&session),
            serde_json::json!({ "branch": "edited" })
        );
    }

    #[test]
    fn switching_back_loads_the_old_branch_and_its_state_from_the_log() {
        let (store, mut session) = started();
        turn(&store, &mut session, 3);

        let events = decide(
            &store,
            &mut session,
            3,
            vec![
                draft("u1", Role::User, "hi"),
                draft("a1", Role::Assistant, "hello"),
            ],
            None,
        );

        assert!(events.iter().any(|e| matches!(e, Logged::BranchLoaded(_))));
        assert_eq!(session.state().head_id.as_deref(), Some("a1"));
        assert!(session.state().has_node("a1"));
        assert!(
            !session.state().has_node("b1"),
            "the edited branch is unloaded in turn"
        );
        assert_eq!(
            state_value(&session),
            serde_json::json!({ "branch": "first" })
        );
        let reloaded = load(&store, Settings::default());
        assert_eq!(reloaded.state().head_id.as_deref(), Some("a1"));
        assert_eq!(
            state_value(&reloaded),
            serde_json::json!({ "branch": "first" })
        );
    }

    #[test]
    fn a_resent_message_from_an_unloaded_branch_is_dropped() {
        let (store, mut session) = started();
        let before = session.seq();

        let ran = session
            .run(
                &store,
                Utc::now(),
                Command::system(
                    "resend",
                    TENANT,
                    CommandPayload::SubmitClientPayload {
                        payload: ClientPayload::Append(ClientAppend {
                            messages: vec![draft("a1", Role::User, "hello")],
                            stream: false,
                            client: Default::default(),
                        }),
                        turn: TurnTarget::Open("t9".into()),
                        queue: QueueMode::Refuse,
                        metadata: Default::default(),
                    },
                ),
            )
            .unwrap();

        assert!(ran.result.is_ok());
        assert_eq!(session.seq(), before, "nothing is written");
    }

    #[test]
    fn a_reset_on_a_long_session_keeps_memory_to_the_new_branch() {
        let (store, mut session) = started();
        for n in 3..23 {
            turn(&store, &mut session, n);
            let path: Vec<DraftMessage> = session
                .state()
                .at_head()
                .transcript()
                .into_iter()
                .map(DraftMessage::from)
                .chain([
                    draft(&format!("q{n}"), Role::User, "more"),
                    draft(&format!("r{n}"), Role::Assistant, "ok"),
                ])
                .collect();
            decide(&store, &mut session, n, path, None);
        }
        assert!(session.state().nodes.len() > 40);

        turn(&store, &mut session, 30);
        decide(
            &store,
            &mut session,
            30,
            vec![
                draft("s1", Role::User, "Summary: a long chat."),
                draft("q30", Role::User, "next"),
            ],
            Some(serde_json::json!({ "reset": true })),
        );

        assert_eq!(session.state().nodes.len(), 2);
        let reloaded = load(&store, Settings::default());
        assert_eq!(reloaded.state().nodes.len(), 2);
        assert_eq!(state_value(&reloaded), serde_json::json!({ "reset": true }));
    }
}
