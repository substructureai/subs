use serde_json::Value;

use super::activity::TurnActivity;
use super::render::Rendered;
use super::{clip, display_of, render, with_footer, ButtonValue, Click};
use crate::protocol::{
    Content, InterruptResolution, InterruptResponder, InterruptResumption, Message, ResumeStatus,
    Role,
};
use crate::runtime::session::state::SessionState;
use crate::transport::channel::ChannelKind;

const NO_LONGER_ACTIVE: &str = "(no longer active)";

pub(super) fn title_of(transcript: &[Message]) -> String {
    transcript
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .find_map(|m| {
            let text = m.content.as_ref().map(Content::text_owned)?;
            let text = plain(&text);
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            (!text.is_empty()).then(|| clip(&text, 60))
        })
        .unwrap_or_else(|| "Working…".to_string())
}

fn plain(text: &str) -> String {
    let text = match text.strip_prefix("<@").and_then(|r| r.split_once(">: ")) {
        Some((_, rest)) => rest,
        None => text,
    };
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        match rest[start + 1..].split_once('>') {
            Some((token, after)) => {
                let label = match token.split_once('|') {
                    Some((_, label)) => label,
                    None if token.starts_with(['@', '#', '!']) => "",
                    None => token,
                };
                out.push_str(label);
                rest = after;
            }
            None => {
                out.push('<');
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub(super) fn live(turn: &TurnActivity, title: &str) -> Rendered {
    Rendered {
        text: String::new(),
        blocks: vec![turn.card(title)],
    }
}

pub(super) fn finished(
    turn: &TurnActivity,
    data: &Value,
    title: &str,
    footer: Option<String>,
) -> Rendered {
    let answer = match data {
        Value::Null => "(no result)".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut blocks = vec![turn.settled_card(title), render::section_block(&answer)];
    if let Some(footer) = &footer {
        blocks.push(render::context_block(footer));
    }
    Rendered {
        text: with_footer(&answer, footer.as_deref()),
        blocks,
    }
}

pub(super) enum Pick {
    Resume(InterruptResumption),
    Stale(Rendered),
    Other,
}

pub(super) fn pick(state: &SessionState, click: &Click) -> Pick {
    let Ok(ButtonValue::InterruptOption {
        interrupt_id,
        option,
    }) = serde_json::from_str(&click.value)
    else {
        return Pick::Other;
    };
    let chosen = state
        .open_interrupt(&interrupt_id)
        .and_then(|open| display_of(&open.payload))
        .and_then(|display| display.options.into_iter().nth(option));
    let Some(option) = chosen else {
        return Pick::Stale(Rendered {
            text: format!("{}\n\n{NO_LONGER_ACTIVE}", click.message_text),
            blocks: render::settled_prompt_blocks(
                &click.message_blocks,
                &click.message_text,
                NO_LONGER_ACTIVE,
            ),
        });
    };
    let resolution = InterruptResolution {
        status: ResumeStatus::Resolved,
        payload: option.value,
        responder: Some(InterruptResponder {
            channel: ChannelKind::SLACK.to_string(),
            user: Some(click.user.clone()),
            label: Some(option.label),
            style: option.style,
        }),
    };
    Pick::Resume(InterruptResumption {
        interrupt_id,
        payload: serde_json::to_value(resolution).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::activity::tests::{started, tool_requested};
    use super::*;
    use crate::protocol::{InterruptOrigin, Issuer, Requester, Subject};
    use crate::runtime::session::state::OpenInterrupt;

    const SESSION: &str = "slack:C1:1.0";

    fn click(value: &str) -> Click {
        Click {
            action_id: "prompt_option_0".to_string(),
            value: value.to_string(),
            user: "U9".to_string(),
            channel: "C1".to_string(),
            message_ts: "8.0".to_string(),
            thread_ts: "1.0".to_string(),
            message_text: "Run it?".to_string(),
            message_blocks: Vec::new(),
            session: None,
        }
    }

    fn state() -> SessionState {
        let mut s = SessionState::new(SESSION.to_string());
        s.owner = Some(crate::protocol::SessionOwner {
            tenant_id: "t".to_string(),
            requester: Requester::new(
                Subject::new(Issuer::new("slack"), "T1:U1"),
                Default::default(),
            ),
            metadata: Default::default(),
        });
        s
    }

    fn prompt(interrupt_id: &str, options: Value) -> OpenInterrupt {
        OpenInterrupt {
            interrupt_id: interrupt_id.to_string(),
            origin: InterruptOrigin::Frontend,
            reason: "hold".to_string(),
            payload: serde_json::json!({
                "message": "Run it?",
                "metadata": { "options": options },
            }),
            anchor: None,
        }
    }

    fn state_with_prompt() -> SessionState {
        let mut s = state();
        s.open_interrupts.push(prompt(
            "int-1",
            serde_json::json!([
                { "label": "Approve", "value": { "decision": "approve" } },
                { "label": "Deny", "value": { "decision": "deny" } },
            ]),
        ));
        s
    }

    fn turn() -> TurnActivity {
        TurnActivity::fold(&[
            started("turn-1", 1),
            tool_requested("tc1", "search_web", "{}", 2),
        ])
        .expect("a turn")
    }

    #[test]
    fn a_finished_turn_settles_its_card_above_the_answer_and_footer() {
        let view = finished(
            &turn(),
            &serde_json::json!("the answer"),
            "Working…",
            Some("2.0s".to_string()),
        );
        assert_eq!(view.text, "the answer\n\n_2.0s_");
        assert_eq!(
            view.blocks[0]["type"], "task_card",
            "the settled card leads"
        );
        assert_eq!(view.blocks[0]["status"], "complete");
        assert_eq!(
            view.blocks[0]["details"]["elements"][0]["elements"][0]["text"],
            "• search_web `{}`"
        );
        assert_eq!(view.blocks[1]["text"]["text"], "the answer");
        assert_eq!(view.blocks[2]["type"], "context", "the footer closes it");
    }

    #[test]
    fn a_running_turn_shows_one_card_of_its_work() {
        let view = live(&turn(), "Working…");
        assert_eq!(view.blocks.len(), 1);
        assert_eq!(view.blocks[0]["task_id"], "turn-1");
        assert_eq!(view.blocks[0]["status"], "in_progress");
        assert_eq!(view.blocks[0]["title"], "Working… · search_web · 1 action");
    }

    #[test]
    fn the_card_is_titled_by_what_the_user_asked() {
        let asked = Message {
            id: "m1".into(),
            role: Role::User,
            content: Some(Content::Text("fix the   login\nbutton".into())),
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
            reasoning: None,
        };
        let answered = Message {
            id: "m2".into(),
            role: Role::Assistant,
            content: Some(Content::Text("on it".into())),
            ..asked.clone()
        };
        assert_eq!(title_of(&[asked.clone(), answered]), "fix the login button");
        assert_eq!(title_of(&[]), "Working…");
        let mention = Message {
            content: Some(Content::Text(
                "<@U0B8ECR818W>: check <#C1|general> and <https://a.test|the doc> for <@U5>".into(),
            )),
            ..asked.clone()
        };
        assert_eq!(title_of(&[mention]), "check general and the doc for");
        let addressed = Message {
            content: Some(Content::Text("<@U0BNB0VAF8E> what is the push rate".into())),
            ..asked.clone()
        };
        assert_eq!(title_of(&[addressed]), "what is the push rate");
        let long = Message {
            content: Some(Content::Text("x".repeat(100))),
            ..asked
        };
        assert_eq!(title_of(&[long]).chars().count(), 60);
    }

    #[test]
    fn a_prompt_click_resumes_with_the_options_value_and_who_chose_it() {
        let Pick::Resume(resumption) = pick(
            &state_with_prompt(),
            &click(r#"{"type":"interrupt.option","interrupt_id":"int-1","option":1}"#),
        ) else {
            panic!("a resume");
        };
        assert_eq!(resumption.interrupt_id, "int-1");
        assert_eq!(resumption.payload["status"], "resolved");
        assert_eq!(resumption.payload["payload"]["decision"], "deny");
        assert_eq!(resumption.payload["responder"]["channel"], "slack");
        assert_eq!(resumption.payload["responder"]["user"], "U9");
        assert_eq!(resumption.payload["responder"]["label"], "Deny");
    }

    #[test]
    fn clicking_an_authorization_prompt_resumes_it() {
        let mut state = state();
        state.open_interrupts.push(prompt(
            "mcp-auth:mcp.sentry",
            serde_json::json!([{ "label": "Retry", "value": { "connection": "mcp.sentry" } }]),
        ));
        let Pick::Resume(resumption) = pick(
            &state,
            &click(
                r#"{"type":"interrupt.option","interrupt_id":"mcp-auth:mcp.sentry","option":0}"#,
            ),
        ) else {
            panic!("a resume");
        };
        assert_eq!(resumption.interrupt_id, "mcp-auth:mcp.sentry");
        assert_eq!(resumption.payload["payload"]["connection"], "mcp.sentry");
    }

    #[test]
    fn a_click_on_a_settled_prompt_marks_it_no_longer_active() {
        let Pick::Stale(view) = pick(
            &state(),
            &click(r#"{"type":"interrupt.option","interrupt_id":"gone","option":0}"#),
        ) else {
            panic!("a stale prompt");
        };
        assert!(view.text.contains(NO_LONGER_ACTIVE));
    }

    #[test]
    fn a_workers_own_button_is_not_a_prompt() {
        assert!(matches!(
            pick(&state_with_prompt(), &click("summarize-thread")),
            Pick::Other
        ));
    }
}
