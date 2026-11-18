use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::caller::Caller;
use crate::command::{CommandPayload, Working};
use crate::entry::{Accepted, Command, CommandId, CommandResult, Entry, Handled, Rejected};
use crate::events::EventPayload;
use crate::ids::Ids;
use crate::span::SpanContext;
use crate::state::{waiting_on_client, ApplyContext, EventMeta, SessionState};
use crate::store::{Store, StoreError};

pub struct CommitContext {
    pub span: SpanContext,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub id: Uuid,
    pub tenant_id: String,
    pub session_id: String,
    pub seq: u64,
    pub span: SpanContext,
    pub occurred_at: DateTime<Utc>,
    pub payload: EventPayload,
    pub meta: EventMeta,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
}

impl SessionEvent {
    pub fn payload_type(&self) -> String {
        serde_json::to_value(&self.payload)
            .ok()
            .and_then(|v| v.get("type")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_string())
    }

    pub fn ends_run(&self) -> bool {
        match &self.payload {
            EventPayload::TurnCompleted(_)
            | EventPayload::SessionInterrupted(_)
            | EventPayload::SessionCancelled => true,
            EventPayload::LlmCallErrored(e) => !e.retryable,
            _ => waiting_on_client(&self.meta.calls),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub snapshot_every: u64,
    pub retention: chrono::Duration,
    pub attempts: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            snapshot_every: 20,
            retention: chrono::Duration::days(7),
            attempts: 32,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the session is busy: {0} writers raced for it")]
    Busy(u32),
}

#[derive(Debug)]
pub struct Ran {
    pub result: CommandResult,
    pub written: Option<Entry>,
    pub wake_moved: bool,
}

struct Draft {
    entry: Entry,
    state: Option<SessionState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub(crate) id: String,
    pub(crate) tenant_id: String,
    pub(crate) state: SessionState,
    pub(crate) seq: u64,
    pub(crate) first_event_at: Option<DateTime<Utc>>,
    pub(crate) last_event_at: Option<DateTime<Utc>>,
    pub(crate) wake_at: Option<DateTime<Utc>>,
    pub(crate) version: u64,
    #[serde(skip)]
    settings: Settings,
}

impl Session {
    pub fn new(id: impl Into<String>, tenant_id: impl Into<String>, settings: Settings) -> Self {
        let id = id.into();
        Self {
            state: SessionState::new(id.clone()),
            id,
            tenant_id: tenant_id.into(),
            seq: 0,
            first_event_at: None,
            last_event_at: None,
            wake_at: None,
            version: 0,
            settings,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn with_state(id: String, tenant_id: String, state: SessionState) -> Self {
        Self {
            state,
            ..Self::new(id, tenant_id, Settings::default())
        }
    }

    pub fn load(
        store: &dyn Store,
        id: impl Into<String>,
        tenant_id: impl Into<String>,
        settings: Settings,
    ) -> Result<Self, Error> {
        let mut session = store
            .latest()?
            .unwrap_or_else(|| Self::new(id, tenant_id, Settings::default()));
        session.settings = settings;
        session.state.tree = crate::tree::Tree::over(store.nodes());
        let tail = store.read(session.seq)?;
        for event in &tail.events {
            session.apply_event(event);
        }
        session.version = tail.version;
        session.state.tree.written();
        session.checked()
    }

    fn checked(self) -> Result<Self, Error> {
        match self.state.tree.poisoned() {
            Some(error) => Err(error.into()),
            None => Ok(self),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn into_state(self) -> SessionState {
        self.state
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn first_event_at(&self) -> Option<DateTime<Utc>> {
        self.first_event_at
    }

    pub fn last_event_at(&self) -> Option<DateTime<Utc>> {
        self.last_event_at
    }

    pub fn exists(&self) -> bool {
        self.seq > 0
    }

    pub fn handled(
        &self,
        store: &dyn Store,
        id: &CommandId,
        now: DateTime<Utc>,
    ) -> Result<Option<CommandResult>, Error> {
        let horizon = now - self.settings.retention;
        Ok(store
            .handled(id)?
            .filter(|(_, at)| *at >= horizon)
            .map(|(result, _)| result))
    }

    pub fn run(
        &mut self,
        store: &dyn Store,
        now: DateTime<Utc>,
        command: Command,
    ) -> Result<Ran, Error> {
        self.write(store, |session| {
            if let Some(result) = session.handled(store, &command.id, now)? {
                return Ok((None, result));
            }
            session.propose(now, &command)
        })
    }

    pub fn wake(&mut self, store: &dyn Store, now: DateTime<Utc>) -> Result<Option<Entry>, Error> {
        let ran = self.write(store, |session| {
            let accepted = Ok(Accepted {
                seq: session.seq,
                turn_id: None,
                queued: false,
                next_turn_id: None,
            });
            Ok((session.woken(now)?, accepted))
        })?;
        Ok(ran.written)
    }

    pub fn events(
        store: &dyn Store,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, Error> {
        Ok(store.events(after_seq, limit)?)
    }

    pub fn wake_at(&self) -> Option<DateTime<Utc>> {
        self.wake_at
    }

    fn write(
        &mut self,
        store: &dyn Store,
        decide: impl Fn(&Session) -> Result<(Option<Draft>, CommandResult), Error>,
    ) -> Result<Ran, Error> {
        for _ in 0..self.settings.attempts {
            let (draft, result) = decide(self)?;
            let Some(Draft { entry, state }) = draft else {
                return Ok(Ran {
                    result,
                    written: None,
                    wake_moved: false,
                });
            };
            match store.append(&entry) {
                Ok(()) => {}
                Err(StoreError::VersionTaken(_)) => {
                    let (id, tenant_id) = (self.id.clone(), self.tenant_id.clone());
                    *self = Self::load(store, id, tenant_id, self.settings.clone())?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            let before = self.wake_at;
            match state {
                Some(state) => {
                    self.state = state;
                    self.record(&entry);
                }
                None => {
                    entry.events.iter().for_each(|e| self.apply_event(e));
                    self.version = entry.version;
                }
            }
            self.state.tree.written();
            if entry.version % self.settings.snapshot_every.max(1) == 0 {
                if let Err(error) = store.snapshot(self) {
                    tracing::warn!(session_id = %self.id, %error, "writing a snapshot failed");
                }
            }
            return Ok(Ran {
                result,
                written: Some(entry),
                wake_moved: self.wake_at != before,
            });
        }
        Err(Error::Busy(self.settings.attempts))
    }

    fn propose(
        &self,
        now: DateTime<Utc>,
        command: &Command,
    ) -> Result<(Option<Draft>, CommandResult), Error> {
        let mut working = Working::new(self.state.clone(), self.seq, now);
        let ran = working.run(command.payload.clone(), &command.caller);
        if let Some(error) = working.tree_failed() {
            return Err(error.into());
        }
        let (state, events, result) = match ran {
            Ok(()) => {
                let waiting = working.waiting_turn().map(str::to_string);
                let next_turn_id = working.joining_turn().map(str::to_string);
                let (state, emitted) = working.finish();
                let events = self.stamp(emitted, &command.span, now, Some(&command.id));
                let accepted = Ok(Accepted {
                    seq: self.seq + events.len() as u64,
                    queued: waiting.is_some(),
                    turn_id: waiting.or_else(|| state.turn_id().map(str::to_string)),
                    next_turn_id,
                });
                (Some(state), events, accepted)
            }
            Err(error) => (None, Vec::new(), Err(Rejected { error })),
        };
        if events.is_empty() && !self.exists() {
            return Ok((None, result));
        }
        let entry = Entry {
            version: self.version + 1,
            at: now,
            commands: vec![Handled {
                id: command.id.clone(),
                kind: command.payload.kind().to_string(),
                caller: command.caller.kind(),
                result: result.clone(),
            }],
            events,
        };
        Ok((Some(Draft { entry, state }), result))
    }

    fn woken(&self, now: DateTime<Utc>) -> Result<Option<Draft>, Error> {
        let caller = Caller::System {
            tenant_id: self.tenant_id.clone(),
        };
        let mut working = Working::new(self.state.clone(), self.seq, now);
        let ran = working.run(CommandPayload::Wake { now }, &caller);
        if let Some(error) = working.tree_failed() {
            return Err(error.into());
        }
        if ran.is_err() {
            return Ok(None);
        }
        let (state, emitted) = working.finish();
        if emitted.is_empty() {
            return Ok(None);
        }
        let span = Ids::new(&self.id, self.seq, now).span("wake");
        let events = self.stamp(emitted, &span, now, None);
        Ok(Some(Draft {
            entry: Entry {
                version: self.version + 1,
                at: now,
                commands: Vec::new(),
                events,
            },
            state: Some(state),
        }))
    }

    fn stamp(
        &self,
        emitted: Vec<(EventPayload, EventMeta)>,
        span: &SpanContext,
        at: DateTime<Utc>,
        command_id: Option<&CommandId>,
    ) -> Vec<SessionEvent> {
        let ids = Ids::new(&format!("{}/events", self.id), self.seq, at);
        (self.seq + 1..)
            .zip(emitted)
            .map(|(seq, (payload, meta))| SessionEvent {
                id: ids.uuid(),
                tenant_id: self.tenant_id.clone(),
                session_id: self.id.clone(),
                seq,
                span: span.clone(),
                occurred_at: at,
                payload,
                meta,
                start_time: at,
                end_time: at,
                command_id: command_id.map(|id| id.0.clone()),
            })
            .collect()
    }

    fn apply_event(&mut self, event: &SessionEvent) {
        self.state.apply(
            &event.payload,
            &ApplyContext {
                occurred_at: event.occurred_at,
                sequence: event.seq,
            },
        );
        self.record_event(event);
    }

    fn record(&mut self, entry: &Entry) {
        entry.events.iter().for_each(|e| self.record_event(e));
        self.version = entry.version;
    }

    fn record_event(&mut self, event: &SessionEvent) {
        self.seq = event.seq;
        self.first_event_at.get_or_insert(event.occurred_at);
        self.last_event_at = Some(event.occurred_at);
        self.wake_at = event.meta.wake_at;
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn handle(
        &self,
        cmd: CommandPayload,
        caller: &Caller,
        now: DateTime<Utc>,
    ) -> Result<Vec<EventPayload>, crate::command::SessionError> {
        let mut working = Working::new(self.state.clone(), self.seq, now);
        working.run(cmd, caller)?;
        Ok(working
            .finish()
            .1
            .into_iter()
            .map(|(payload, _)| payload)
            .collect())
    }

    #[cfg(test)]
    pub(crate) fn try_handle(
        &self,
        cmd: CommandPayload,
        caller: &Caller,
    ) -> Result<Vec<EventPayload>, crate::command::SessionError> {
        self.handle(cmd, caller, Utc::now())
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn commit(
        &mut self,
        events: Vec<EventPayload>,
        context: &CommitContext,
    ) -> Vec<SessionEvent> {
        let mut emitted = Vec::with_capacity(events.len());
        for (seq, payload) in (self.seq + 1..).zip(events) {
            self.state.apply(
                &payload,
                &ApplyContext {
                    occurred_at: context.occurred_at,
                    sequence: seq,
                },
            );
            emitted.push((payload, self.state.event_meta(context.occurred_at)));
        }
        let events = self.stamp(emitted, &context.span, context.occurred_at, None);
        self.record(&Entry {
            version: self.version,
            at: context.occurred_at,
            commands: Vec::new(),
            events: events.clone(),
        });
        events
    }
}

#[cfg(test)]
mod ends_run_tests {
    use super::*;
    use crate::events::DecisionQueued;
    use crate::protocol::{Effect, EffectKind, EffectStatus, Handler};
    use crate::state::{EventMeta, SessionStatus};

    fn call(id: &str, handler: Handler) -> Effect {
        Effect {
            id: id.into(),
            kind: EffectKind::ToolCall,
            status: EffectStatus::Pending,
            attempt: 0,
            deadline: None,
            asked_at: None,
            name: None,
            arguments: None,
            handler: Some(handler),
            stream: None,
            agent_id: None,
            session_id: None,
            branch: None,
        }
    }

    fn event(payload: EventPayload, calls: Vec<Effect>) -> SessionEvent {
        let at = chrono::DateTime::UNIX_EPOCH;
        SessionEvent {
            id: Uuid::nil(),
            tenant_id: "t".into(),
            session_id: "s".into(),
            seq: 1,
            span: SpanContext::root(),
            occurred_at: at,
            payload,
            meta: EventMeta {
                status: SessionStatus::Idle,
                wake_at: None,
                owner: None,
                agent_id: None,
                ancestry: Vec::new(),
                turn_id: None,
                cost: Default::default(),
                subagent_cost: Default::default(),
                calls,
                decisions: Vec::new(),
            },
            start_time: at,
            end_time: at,
            command_id: None,
        }
    }

    fn queued(calls: Vec<Effect>) -> SessionEvent {
        event(
            EventPayload::DecisionQueued(DecisionQueued {
                id: "d1".into(),
                trigger: crate::decision::Trigger::SessionStart,
                branch: None,
            }),
            calls,
        )
    }

    #[test]
    fn a_completed_turn_ends_the_run() {
        let done = event(
            EventPayload::TurnCompleted(crate::events::TurnCompleted {
                turn_id: "t1".into(),
                data: serde_json::Value::Null,
                turn_cost: Default::default(),
                turn_token_usage: Default::default(),
                turn_calls: Default::default(),
                turn_started_at: None,
                error: None,
            }),
            vec![call("c", Handler::Client)],
        );
        assert!(done.ends_run());
    }

    #[test]
    fn an_outstanding_client_call_ends_the_run() {
        assert!(queued(vec![call("c", Handler::Client)]).ends_run());
    }

    #[test]
    fn a_worker_call_alongside_it_does_not() {
        assert!(!queued(vec![call("c", Handler::Client), call("w", Handler::Worker)]).ends_run());
    }

    #[test]
    fn a_subagent_names_no_handler_so_it_holds_the_run() {
        let mut sub = call("s", Handler::Client);
        sub.kind = EffectKind::Subagent;
        sub.handler = None;
        assert!(!queued(vec![call("c", Handler::Client), sub]).ends_run());
    }

    #[test]
    fn nothing_outstanding_is_not_an_ending() {
        assert!(!queued(Vec::new()).ends_run());
    }

    #[test]
    fn a_retryable_llm_error_keeps_the_run() {
        let payload = |retryable: bool| {
            EventPayload::LlmCallErrored(crate::events::LlmCallErrored {
                id: "l".into(),
                attempt: 0,
                error: crate::protocol::ErrorInfo::new(
                    crate::protocol::ErrorCode::Internal,
                    "boom",
                ),
                retryable,
            })
        };
        assert!(!event(payload(true), Vec::new()).ends_run());
        assert!(event(payload(false), Vec::new()).ends_run());
    }
}
