use std::collections::HashSet;

use crate::protocol::DraftMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedWrite {
    pub index: usize,
    pub rerecord: bool,
}

pub fn plan_reconcile(known: &HashSet<&str>, messages: &[DraftMessage]) -> Vec<PlannedWrite> {
    let mut writes = Vec::new();
    let mut news_started = false;
    for (index, msg) in messages.iter().enumerate() {
        let is_known = msg.id.as_deref().is_some_and(|id| known.contains(id));
        if is_known && !news_started {
            continue;
        }
        news_started = true;
        writes.push(PlannedWrite {
            index,
            rerecord: is_known,
        });
    }
    writes
}

pub fn news_start(known: &HashSet<&str>, messages: &[DraftMessage]) -> usize {
    plan_reconcile(known, messages)
        .first()
        .map(|w| w.index)
        .unwrap_or(messages.len())
}

pub fn landing_leaf(messages: &[DraftMessage], plan: &[PlannedWrite]) -> Option<String> {
    let boundary = plan.first().map(|w| w.index).unwrap_or(messages.len());
    messages[..boundary].last().and_then(|m| m.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Content, Role};

    fn msg(id: &str) -> DraftMessage {
        DraftMessage {
            id: (!id.is_empty()).then(|| id.to_string()),
            role: Role::User,
            content: Some(Content::Text(String::new())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn known_of<'a>(ids: &'a [&'a str]) -> HashSet<&'a str> {
        ids.iter().copied().collect()
    }

    #[test]
    fn append_writes_only_the_tail() {
        let known = known_of(&["u1", "a1"]);
        let view = [msg("u1"), msg("a1"), msg("u2")];
        let plan = plan_reconcile(&known, &view);
        assert_eq!(
            plan,
            vec![PlannedWrite {
                index: 2,
                rerecord: false
            }]
        );
        assert_eq!(news_start(&known, &view), 2);
    }

    #[test]
    fn no_op_resend_writes_nothing() {
        let known = known_of(&["u1", "a1"]);
        let view = [msg("u1"), msg("a1")];
        assert!(plan_reconcile(&known, &view).is_empty());
        assert_eq!(news_start(&known, &view), 2);
    }

    #[test]
    fn news_does_not_stop_and_re_records_known_ids() {
        let known = known_of(&["u1", "a1", "u2"]);
        let view = [msg("u1"), msg("e1"), msg("u2")];
        let plan = plan_reconcile(&known, &view);
        assert_eq!(
            plan,
            vec![
                PlannedWrite {
                    index: 1,
                    rerecord: false
                },
                PlannedWrite {
                    index: 2,
                    rerecord: true
                },
            ]
        );
    }

    #[test]
    fn idless_head_is_all_new() {
        let known = known_of(&["u1"]);
        let view = [msg(""), msg("")];
        assert_eq!(news_start(&known, &view), 0);
    }
}
