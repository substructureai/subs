use serde_json::Value;

use super::clip;
use super::render;
use crate::session::events::EventPayload;
use crate::session::SessionEvent;

const MAX_SAID: usize = 400;
const MAX_STEP: usize = 80;
const MAX_LOG: usize = 3_000;
const MILESTONE_EVERY: usize = 25;

#[derive(Clone)]
enum Item {
    Said {
        id: String,
        text: String,
    },
    Step {
        id: String,
        name: String,
        preview: String,
    },
}

impl Item {
    fn id(&self) -> &str {
        match self {
            Item::Said { id, .. } | Item::Step { id, .. } => id,
        }
    }
}

#[derive(Clone)]
pub(super) struct TurnActivity {
    turn_id: String,
    items: Vec<Item>,
}

impl TurnActivity {
    pub(super) fn new(turn_id: &str) -> Self {
        Self {
            turn_id: turn_id.to_string(),
            items: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn fold(events: &[SessionEvent]) -> Option<Self> {
        let mut turn: Option<Self> = None;
        for event in events {
            if let EventPayload::TurnStarted(t) = &event.payload {
                turn = Some(Self {
                    turn_id: t.turn_id.clone(),
                    items: Vec::new(),
                });
                continue;
            }
            if let Some(turn) = turn.as_mut() {
                turn.apply(event);
            }
        }
        turn
    }

    pub(super) fn apply(&mut self, event: &SessionEvent) {
        match &event.payload {
            EventPayload::LlmCallCompleted(c) if !c.response.tool_calls.is_empty() => {
                if let Some(text) = c.response.content.as_deref().map(str::trim) {
                    if !text.is_empty() {
                        self.say(&c.id, text);
                    }
                }
            }
            EventPayload::ToolCallRequested(t) => {
                self.step(&t.id, &t.name, Some(&t.arguments));
            }
            EventPayload::SubagentRequested(s) => {
                self.step(&s.id, &format!("agent {}", s.agent_id), None);
            }
            _ => {}
        }
    }

    fn say(&mut self, id: &str, text: &str) {
        if self.items.iter().any(|item| item.id() == id) {
            return;
        }
        self.items.push(Item::Said {
            id: id.to_string(),
            text: text.to_string(),
        });
    }

    fn step(&mut self, id: &str, name: &str, input: Option<&str>) {
        if self.items.iter().any(|item| item.id() == id) {
            return;
        }
        let input = input
            .map(flatten)
            .map(|i| i.replace('`', "'"))
            .filter(|i| !i.is_empty());
        let preview = match input {
            Some(input) => format!("{name} `{}`", clip(&input, MAX_STEP)),
            None => name.to_string(),
        };
        self.items.push(Item::Step {
            id: id.to_string(),
            name: name.to_string(),
            preview,
        });
    }

    fn steps(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i, Item::Step { .. }))
            .count()
    }

    fn title(&self, ask: &str, running: bool) -> String {
        let steps = self.steps();
        if steps == 0 {
            return ask.to_string();
        }
        let count = match steps {
            1 => "1 action".to_string(),
            n => format!("{n} actions"),
        };
        let current = running.then(|| self.current()).flatten();
        match current {
            Some(name) => format!("{ask} · {name} · {count}"),
            None => format!("{ask} · {count}"),
        }
    }

    fn current(&self) -> Option<&str> {
        self.items.iter().rev().find_map(|item| match item {
            Item::Step { name, .. } => Some(name.as_str()),
            _ => None,
        })
    }

    pub(super) fn card(&self, ask: &str) -> Value {
        let title = self.title(ask, true);
        render::turn_card(&self.turn_id, &title, "in_progress", self.log().as_deref())
    }

    pub(super) fn settled_card(&self, ask: &str) -> Value {
        let title = self.title(ask, false);
        render::turn_card(&self.turn_id, &title, "complete", self.log().as_deref())
    }

    fn log(&self) -> Option<String> {
        let mut log = String::new();
        let mut steps = 0usize;
        let mut counting = false;
        for item in &self.items {
            match item {
                Item::Said { .. } if counting => continue,
                Item::Said { text, .. } => {
                    if !log.is_empty() {
                        log.push_str("\n\n");
                    }
                    log.push_str(&clip(text, MAX_SAID));
                }
                Item::Step { preview, .. } => {
                    steps += 1;
                    if counting {
                        if steps.is_multiple_of(MILESTONE_EVERY) {
                            log.push_str(&format!("\n• … {steps} actions"));
                        }
                        continue;
                    }
                    if !log.is_empty() {
                        log.push('\n');
                    }
                    log.push_str(&format!("• {preview}"));
                }
            }
            if !counting && log.chars().count() >= MAX_LOG {
                counting = true;
                log.push_str("\n• …");
            }
        }
        (!log.is_empty()).then_some(log)
    }
}

pub(super) fn elapsed(
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> String {
    let ms = (to - from).num_milliseconds().max(0);
    if ms < 60_000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1000)
}

fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
pub(in crate::transport::slack) mod tests {
    use super::*;
    use crate::session::events::{SubagentRequested, ToolCallRequested, TurnStarted};

    pub(in crate::transport::slack) fn said(
        id: &str,
        content: &str,
        calling: bool,
        seq: u64,
    ) -> SessionEvent {
        use crate::protocol::{LlmResponse, ToolCall, ToolCallFunction};
        event(
            seq,
            EventPayload::LlmCallCompleted(crate::session::events::LlmCallCompleted {
                id: id.into(),
                attempt: 0,
                response: LlmResponse {
                    model: "m".into(),
                    content: Some(content.into()),
                    tool_calls: calling
                        .then(|| ToolCall {
                            id: "tc1".into(),
                            call_type: "function".into(),
                            function: ToolCallFunction {
                                name: "search_web".into(),
                                arguments: "{}".into(),
                            },
                        })
                        .into_iter()
                        .collect(),
                    finish_reason: None,
                    usage: None,
                    cost: None,
                    images: Vec::new(),
                    reasoning: None,
                },
            }),
        )
    }

    fn retry() -> crate::protocol::RetryPolicy {
        crate::protocol::RetryPolicy {
            queue_timeout_secs: None,
            run_timeout_secs: None,
            total_timeout_secs: None,
            max_attempts: 0,
            backoff_base_secs: 1,
            backoff_max_secs: 1,
        }
    }

    pub(in crate::transport::slack) fn event(seq: u64, payload: EventPayload) -> SessionEvent {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        SessionEvent {
            id: uuid::Uuid::nil(),
            tenant_id: "t".into(),
            session_id: "slack:C1:1.0".into(),
            seq,
            span: crate::span::SpanContext::root(),
            occurred_at: at,
            payload,
            meta: crate::session::state::EventMeta {
                status: crate::session::state::SessionStatus::Idle,
                wake_at: None,
                owner: None,
                agent_id: None,
                ancestry: Vec::new(),
                turn_id: None,
                cost: Default::default(),
                subagent_cost: Default::default(),
                head_id: None,
                calls: Vec::new(),
                decisions: Vec::new(),
            },
            start_time: at,
            end_time: at,
            command_id: None,
        }
    }

    pub(in crate::transport::slack) fn started(turn: &str, seq: u64) -> SessionEvent {
        event(
            seq,
            EventPayload::TurnStarted(TurnStarted {
                turn_id: turn.into(),
            }),
        )
    }

    pub(in crate::transport::slack) fn tool_requested(
        id: &str,
        name: &str,
        arguments: &str,
        seq: u64,
    ) -> SessionEvent {
        event(
            seq,
            EventPayload::ToolCallRequested(ToolCallRequested {
                id: id.into(),
                attempt: 0,
                name: name.into(),
                arguments: arguments.into(),
                handler: Default::default(),
                target: None,
                retry: retry(),
            }),
        )
    }

    fn details(events: &[SessionEvent]) -> String {
        let card = TurnActivity::fold(events).unwrap().card("t");
        card["details"]["elements"][0]["elements"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn the_card_carries_the_turn_as_a_growing_log() {
        let mut events = vec![
            started("turn-1", 1),
            said("llm1", "Let me look that up.", true, 2),
            tool_requested("tc1", "search_web", r#"{"q":"x"}"#, 3),
        ];
        let card = TurnActivity::fold(&events).unwrap().card("Find x");
        assert_eq!(card["type"], "task_card");
        assert_eq!(card["task_id"], "turn-1");
        assert_eq!(card["title"], "Find x · search_web · 1 action");
        assert_eq!(card["status"], "in_progress");
        let first = details(&events);
        assert_eq!(first, "Let me look that up.\n• search_web `{\"q\":\"x\"}`");
        events.push(said("llm2", "Now the details.", true, 4));
        events.push(tool_requested("tc2", "get_page", "{}", 5));
        let second = details(&events);
        assert!(second.starts_with(&first), "{second}");
        assert_eq!(
            second,
            "Let me look that up.\n• search_web `{\"q\":\"x\"}`\n\nNow the details.\n• get_page `{}`",
            "a blank line holds a preamble out of the list before it"
        );
    }

    #[test]
    fn a_redelivered_preamble_lands_once() {
        let events = vec![
            started("turn-1", 1),
            said("llm1", "First.", true, 2),
            tool_requested("tc1", "a", "{}", 3),
            said("llm1", "First.", true, 4),
        ];
        assert_eq!(details(&events), "First.\n• a `{}`");
    }

    #[test]
    fn a_retry_lands_once() {
        let events = vec![
            started("turn-1", 1),
            tool_requested("tc1", "a", "{}", 2),
            tool_requested("tc1", "a", r#"{"again":1}"#, 3),
        ];
        assert_eq!(details(&events), "• a `{}`");
    }

    #[test]
    fn a_response_with_nothing_left_to_call_is_the_answer_not_a_preamble() {
        let events = vec![
            started("turn-1", 1),
            said("llm1", "Here is the answer.", false, 2),
        ];
        let card = TurnActivity::fold(&events).unwrap().card("t");
        assert!(card.get("details").is_none());
    }

    #[test]
    fn a_silent_call_says_nothing() {
        let events = vec![started("turn-1", 1), said("llm1", "   ", true, 2)];
        let card = TurnActivity::fold(&events).unwrap().card("t");
        assert!(card.get("details").is_none());
    }

    #[test]
    fn a_subagent_previews_by_name() {
        let events = vec![
            started("turn-1", 1),
            event(
                2,
                EventPayload::SubagentRequested(SubagentRequested {
                    id: "tc1".into(),
                    agent_id: "researcher".into(),
                    session_id: "sub-1".into(),
                    message: None,
                    retry: retry(),
                    mode: Default::default(),
                }),
            ),
        ];
        assert_eq!(details(&events), "• agent researcher");
    }

    #[test]
    fn only_the_latest_turn_folds() {
        let events = vec![
            started("turn-1", 1),
            tool_requested("tc1", "old", "{}", 2),
            started("turn-2", 3),
            tool_requested("tc2", "new", "{}", 4),
        ];
        let turn = TurnActivity::fold(&events).unwrap();
        assert_eq!(turn.card("t")["task_id"], "turn-2");
        assert_eq!(details(&events), "• new `{}`");
        assert!(TurnActivity::fold(&[tool_requested("tc1", "t", "{}", 1)]).is_none());
    }

    #[test]
    fn a_chatty_item_is_clipped_per_line() {
        let long = "x".repeat(500);
        let events = vec![
            started("turn-1", 1),
            said("llm1", &long, true, 2),
            tool_requested("tc1", "tool", &long, 3),
        ];
        let text = details(&events);
        assert!(text.chars().count() <= MAX_SAID + MAX_STEP + 20, "{text}");
        assert!(text.contains('…'));
    }

    #[test]
    fn the_settled_card_carries_the_same_log_done() {
        let events = vec![
            started("turn-1", 1),
            said("llm1", "First.", true, 2),
            tool_requested("tc1", "a", "{}", 3),
            said("llm2", "Second.", true, 4),
            tool_requested("tc2", "b", "{}", 5),
        ];
        let card = TurnActivity::fold(&events).unwrap().settled_card("Find x");
        assert_eq!(card["task_id"], "turn-1");
        assert_eq!(card["status"], "complete");
        assert_eq!(
            card["details"]["elements"][0]["elements"][0]["text"],
            "First.\n• a `{}`\n\nSecond.\n• b `{}`",
            "every step, oldest first"
        );
    }

    fn long_turn(steps: u64) -> Vec<SessionEvent> {
        let arg = "x".repeat(100);
        let mut events = vec![started("turn-1", 1)];
        for i in 0..steps {
            events.push(tool_requested(&format!("tc{i}"), "tool", &arg, 2 + i));
        }
        events
    }

    #[test]
    fn a_long_turn_keeps_the_log_moving() {
        let (at_100, at_200) = (details(&long_turn(100)), details(&long_turn(200)));
        assert!(at_200.starts_with(&at_100), "the log only grows");
        assert!(
            at_200.chars().count() > at_100.chars().count(),
            "100 more steps must say something; got {} chars twice",
            at_200.chars().count()
        );
        assert!(at_200.contains("• … 200 actions"), "{at_200}");
    }

    #[test]
    fn a_long_turn_counts_instead_of_listing() {
        let log = details(&long_turn(200));
        let listed = log.lines().filter(|l| l.starts_with("• tool")).count();
        let counted = log.lines().filter(|l| l.starts_with("• … ")).count();
        assert!(listed < 40, "the list stops; got {listed} lines");
        assert_eq!(counted, 7, "{log}");
        assert!(log.contains("• … 50 actions"), "{log}");
        assert!(log.chars().count() < MAX_LOG * 2, "{}", log.chars().count());
    }

    #[test]
    fn the_title_says_where_the_turn_is() {
        let turn = |events: &[SessionEvent]| TurnActivity::fold(events).unwrap();
        let none = vec![started("turn-1", 1)];
        assert_eq!(turn(&none).card("Find x")["title"], "Find x");

        let one = vec![
            started("turn-1", 1),
            tool_requested("tc1", "search_web", "{}", 2),
        ];
        assert_eq!(
            turn(&one).card("Find x")["title"],
            "Find x · search_web · 1 action"
        );

        let mut two = one.clone();
        two.push(tool_requested("tc2", "get_page", "{}", 3));
        assert_eq!(
            turn(&two).card("Find x")["title"],
            "Find x · get_page · 2 actions"
        );
        assert_eq!(
            turn(&two).settled_card("Find x")["title"],
            "Find x · 2 actions"
        );
        assert_eq!(
            turn(&long_turn(200)).settled_card("Find x")["title"],
            "Find x · 200 actions"
        );
    }

    #[test]
    #[ignore]
    fn dump_long_turn() {
        let tools = [
            ("rg", r#"{"pattern":"themePalette","glob":"*.swift"}"#),
            (
                "read_file",
                r#"{"path":"rider/ios/RiderApp/Features/Auth/LoginView.swift"}"#,
            ),
            (
                "edit_file",
                r#"{"path":"rider/ios/RiderApp/Features/Auth/LoginView.swift"}"#,
            ),
            ("bash", r#"{"cmd":"which swift swiftc xcodebuild"}"#),
        ];
        let says = [
            "Let me check how sibling views consume the theme palette so the border matches conventions.",
            "Now I'll add the border, wiring it to the existing theme palette.",
            "No Swift toolchain here, so I can't build; the change is small and localized.",
        ];
        let mut events = vec![started("turn-1", 1)];
        let mut seq = 2;
        for i in 0..60u64 {
            if i % 5 == 0 {
                events.push(said(
                    &format!("llm{i}"),
                    says[(i / 5) as usize % 3],
                    true,
                    seq,
                ));
                seq += 1;
            }
            let (name, args) = tools[i as usize % 4];
            events.push(tool_requested(&format!("tc{i}"), name, args, seq));
            seq += 1;
            let card = TurnActivity::fold(&events)
                .unwrap()
                .card("fix the login button");
            println!(
                "CARD {}",
                serde_json::json!({
                    "title": card["title"],
                    "details": card["details"]["elements"][0]["elements"][0]["text"],
                })
            );
        }
        let card = TurnActivity::fold(&events)
            .unwrap()
            .settled_card("fix the login button");
        println!(
            "CARD {}",
            serde_json::json!({
                "title": card["title"], "settled": true,
                "details": card["details"]["elements"][0]["elements"][0]["text"],
            })
        );
    }

    #[test]
    fn the_footer_reports_how_long_the_turn_took() {
        let at = |s: i64| chrono::DateTime::from_timestamp(1_700_000_000 + s, 0).unwrap();
        assert_eq!(elapsed(at(0), at(4)), "4.0s");
        assert_eq!(elapsed(at(0), at(90)), "1m 30s");
    }
}
