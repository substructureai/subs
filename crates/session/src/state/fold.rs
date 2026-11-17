use super::*;

impl SessionState {
    #[cfg(test)]
    pub fn seed(&mut self, node: crate::tree::Node) {
        let seq = self.tree.unwritten().count() as u64;
        self.grow(node, seq);
    }

    #[cfg(test)]
    pub fn seed_config(&mut self, config: AgentConfig) {
        let parent = self.head_id.clone();
        let id = format!("seed-config-{}", self.tree.unwritten().count());
        self.seed(crate::tree::Node {
            place: crate::tree::Place {
                id,
                parent_id: parent,
                branch: None,
            },
            fact: crate::tree::Fact::Config {
                config: Some(config),
                root: false,
            },
        });
    }

    #[cfg(test)]
    pub fn seed_message(&mut self, message: crate::protocol::NewMessage) {
        self.seed(crate::tree::Node::of_message(&message));
    }

    fn grow(&mut self, node: crate::tree::Node, seq: u64) {
        let id = node.id().to_string();
        let user = node.message().is_some_and(|m| m.role == Role::User);
        let branch = node.branch().map(str::to_string);
        self.tree.push(Logged { seq, entry: node });
        match branch {
            Some(b) => {
                if let Some(branch) = self.branches.get_mut(&b) {
                    branch.leaf = Some(id);
                }
            }
            None => {
                if user {
                    self.status = SessionStatus::Idle;
                }
                self.head_id = Some(id);
            }
        }
    }

    fn insert_effect(
        &mut self,
        id: &str,
        tracking: EffectTracking,
        payload: EffectPayload,
        branch: Option<&str>,
    ) {
        let anchor = match payload {
            EffectPayload::ConnectorSync(_) => None,
            _ => self.scope_node(branch),
        };
        let turn = match self.detached(branch) {
            true => None,
            false => self.phase.turn_id().map(str::to_string),
        };
        self.effects.insert(
            (payload.kind(), id.to_string()),
            EffectState {
                id: id.to_string(),
                tracking,
                asked_at: anchor,
                branch: branch.map(str::to_string),
                turn,
                payload,
            },
        );
    }

    pub(super) fn remove_effect(&mut self, kind: EffectKind, id: &str) -> Option<EffectState> {
        self.effects.remove(&(kind, id.to_string()))
    }

    fn enqueue(&mut self, seq: u64, kind: EffectKind, id: &str) {
        if !self
            .schedule_queue
            .iter()
            .any(|e| e.kind == kind && e.id == id)
        {
            self.schedule_queue.push(QueueEntry {
                seq,
                kind,
                id: id.to_string(),
            });
        }
    }

    pub(super) fn dequeue(&mut self, kind: EffectKind, id: &str) {
        self.schedule_queue
            .retain(|e| !(e.kind == kind && e.id == id));
    }

    fn apply_llm_requested(&mut self, payload: &LlmCallRequested, ctx: &ApplyContext) {
        let spec = payload.spec.clone();
        if let Some(existing) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
            existing.tracking.requeue();
            existing.asked_at = payload.at.clone();
            if let Some(call) = existing.llm_mut() {
                call.prompt = Vec::new();
                call.llm = payload.llm.clone();
                call.spec = spec;
                call.stream = payload.stream;
                call.handler = payload.handler;
                call.format = payload.format;
                call.defer_tools_strategy = payload.defer_tools_strategy;
            }
        } else {
            if !self.detached(payload.branch.as_deref()) {
                self.turn_calls.llm.count += 1;
            }
            self.insert_effect(
                &payload.id,
                EffectTracking::new_queued(payload.retry.clone()),
                EffectPayload::LlmCall(LlmCallState {
                    prompt: Vec::new(),
                    llm: payload.llm.clone(),
                    spec,
                    stream: payload.stream,
                    handler: payload.handler,
                    format: payload.format,
                    reads_from: None,
                    dispatched_seq: None,
                    sent_tools: None,
                    unanswered: Vec::new(),
                    defer_tools_strategy: payload.defer_tools_strategy,
                }),
                payload.branch.as_deref(),
            );
            if let Some(e) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
                e.asked_at = payload.at.clone();
            }
        }
        if payload.branch.is_none() {
            self.turn_called = true;
        }
        self.enqueue(ctx.sequence, EffectKind::LlmCall, &payload.id);
    }

    fn settle_prompt(&mut self, id: &str) {
        if let Some(call) = self
            .effect_mut(EffectKind::LlmCall, id)
            .and_then(|e| e.llm_mut())
        {
            call.prompt = Vec::new();
        }
    }

    fn apply_llm_dispatched(&mut self, payload: &LlmCallDispatched, ctx: &ApplyContext) {
        let Some(out) = self.outgoing(&payload.id) else {
            self.dequeue(EffectKind::LlmCall, &payload.id);
            return;
        };
        if let Some(recorded) = &payload.fingerprint {
            let built = prompt_context::fingerprint(&out.prompt);
            if *recorded != built {
                tracing::warn!(
                    call_id = %payload.id,
                    %recorded,
                    %built,
                    "the prompt built for a call is not the prompt it sent"
                );
            }
        }
        let at = self.at(out.node.as_deref());
        let connector_tools = at.connector_tools().tools;
        let locked = at.locked_tools();
        let run_code = at.run_code();
        if let Some(effect) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
            effect.tracking.dispatch(ctx.occurred_at);
            if let Some(call) = effect.llm_mut() {
                call.sent_tools = match run_code {
                    Some(run_code) => Some(vec![run_code]),
                    None => sent_tools(call.spec.tools.clone(), &connector_tools, &locked),
                };
                call.prompt = out.prompt;
                call.dispatched_seq = Some(ctx.sequence);
            }
        }
        self.dequeue(EffectKind::LlmCall, &payload.id);
    }

    fn apply_tool_requested(&mut self, payload: &ToolCallRequested, ctx: &ApplyContext) {
        if let Some(existing) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
            existing.tracking.requeue();
            if let Some(tc) = existing.tool_mut() {
                tc.arguments = payload.arguments.clone();
            }
        } else {
            if !self.detached(payload.branch.as_deref()) {
                self.turn_calls.tool.count += 1;
            }
            let script_language = match payload.target {
                Some(ConnectorTarget::Code) => self
                    .at_scope(payload.branch.as_deref())
                    .config()
                    .and_then(|c| c.codemode)
                    .unwrap_or_default(),
                _ => Default::default(),
            };
            self.insert_effect(
                &payload.id,
                EffectTracking::new_queued(payload.retry.clone()),
                EffectPayload::ToolCall(ToolCallState {
                    name: payload.name.clone(),
                    handler: payload.handler,
                    target: payload.target.clone(),
                    arguments: payload.arguments.clone(),
                    result: None,
                    is_error: false,
                    script: payload.script.clone(),
                    script_language,
                    script_cpu_ms: 0,
                    script_file_bytes: 0,
                    script_skill_files: None,
                    script_asked: Vec::new(),
                    recorded: false,
                }),
                payload.branch.as_deref(),
            );
            if let Some(script) = &payload.script {
                if let Some(parent) = self
                    .effect_mut(EffectKind::ToolCall, &script.parent)
                    .and_then(|e| e.tool_mut())
                {
                    parent.script_asked.retain(|a| a.id != payload.id);
                }
                let anchor = self
                    .effect(EffectKind::ToolCall, &script.parent)
                    .and_then(|e| e.asked_at.clone());
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.asked_at = anchor;
                }
            }
        }
        self.enqueue(ctx.sequence, EffectKind::ToolCall, &payload.id);
    }

    fn apply_system1_requested(&mut self, p: &System1CallRequested, ctx: &ApplyContext) {
        match self.effect_mut(EffectKind::System1Call, &p.id) {
            Some(existing) => {
                existing.tracking.requeue();
                if let Some(call) = existing.system1_mut() {
                    call.request = p.request.clone();
                }
            }
            None => {
                if !self.detached(p.branch.as_deref()) {
                    self.turn_calls.system1.count += 1;
                }
                self.insert_effect(
                    &p.id,
                    EffectTracking::new_queued(p.retry.clone()),
                    EffectPayload::System1Call(System1CallState {
                        system1: p.system1.clone(),
                        request: p.request.clone(),
                    }),
                    p.branch.as_deref(),
                )
            }
        }
        self.enqueue(ctx.sequence, EffectKind::System1Call, &p.id);
    }

    fn apply_subagent_requested(&mut self, payload: &SubagentRequested, ctx: &ApplyContext) {
        if let Some(existing) = self.effect_mut(EffectKind::Subagent, &payload.id) {
            existing.tracking.requeue();
        } else {
            if !self.detached(payload.branch.as_deref()) {
                self.turn_calls.subagent.count += 1;
            }
            self.insert_effect(
                &payload.id,
                EffectTracking::new_queued(payload.retry.clone()),
                EffectPayload::Subagent(SubagentCallState {
                    on_branch: payload.on_branch.clone(),
                    agent_id: payload.agent_id.clone(),
                    session_id: payload.session_id.clone(),
                    message: payload.message.clone(),
                    result: None,
                    is_error: false,
                    mode: payload.mode,
                }),
                payload.branch.as_deref(),
            );
        }
        self.enqueue(ctx.sequence, EffectKind::Subagent, &payload.id);
    }

    fn apply_interrupted(&mut self, payload: &SessionInterrupted) {
        if !self
            .open_interrupts
            .iter()
            .any(|i| i.interrupt_id == payload.interrupt_id)
        {
            self.open_interrupts.push(OpenInterrupt {
                interrupt_id: payload.interrupt_id.clone(),
                origin: payload.origin,
                reason: payload.reason.clone(),
                payload: payload.payload.clone(),
                anchor: payload.anchor.clone(),
                branch: payload.branch.clone(),
            });
        }
        let lane = self.lane(payload.branch.as_deref());
        let held: Vec<(EffectKind, String)> = self
            .effects_of(EffectKind::Decision)
            .filter(|e| e.tracking.status() == EffectStatus::Pending)
            .filter(|e| self.lane(e.branch.as_deref()) == lane)
            .map(|e| (EffectKind::Decision, e.id.clone()))
            .collect();
        for key in held {
            self.effects.remove(&key);
        }
    }

    fn apply_decision_errored(&mut self, p: &DecisionErrored, now: DateTime<Utc>) {
        let failed = self
            .effect_mut(EffectKind::Decision, &p.id)
            .map(|e| {
                e.tracking.record_error(p.retryable, now);
                e.tracking.status == EffectStatus::Failed
            })
            .unwrap_or(false);
        if failed {
            let started = self
                .remove_effect(EffectKind::Decision, &p.id)
                .and_then(|e| e.decision().map(|d| d.trigger.clone()))
                .is_some_and(|t| matches!(t, Trigger::SessionStart));
            self.session_start_failed |= started;
            self.dequeue(EffectKind::Decision, &p.id);
        }
    }

    fn apply_decision_queued(&mut self, p: &DecisionQueued, ctx: &ApplyContext) {
        if let Some(queued) = self
            .effect_mut(EffectKind::Decision, &p.id)
            .and_then(|e| e.decision_mut())
        {
            queued.trigger = p.trigger.clone();
            return;
        }
        match &p.trigger {
            Trigger::SessionStart => self.session_start_failed = false,
            Trigger::TurnFinished { .. } => self.quiet_end = None,
            _ => {}
        }
        let retry_policy = self.worker_retry.clone().unwrap_or(RetryPolicy::no_retry());
        let branch = p.branch.clone().or_else(|| self.trigger_branch(&p.trigger));
        self.insert_effect(
            &p.id,
            EffectTracking::new_queued(retry_policy),
            EffectPayload::Decision(WorkerDecisionState {
                trigger: p.trigger.clone(),
                branch: branch.clone(),
                source_event_sequence: ctx.sequence,
                dispatched_seq: None,
                turn: None,
            }),
            branch.as_deref(),
        );
        self.enqueue(ctx.sequence, EffectKind::Decision, &p.id);
        if let Trigger::TurnFinished {
            turn_id,
            data,
            cost,
            usage,
        } = &p.trigger
        {
            self.last_turn_at = Some(ctx.occurred_at);
            self.phase = TurnPhase::Finalizing(TurnEnd {
                turn_id: turn_id.clone(),
                data: data.clone(),
                cost: *cost,
                usage: usage.clone(),
            });
            let turn_id = turn_id.clone();
            self.enqueue(ctx.sequence, EffectKind::TurnEnd, &turn_id);
        }
    }

    fn apply_call_voided(&mut self, p: &CallVoided) {
        let kind = p.kind;
        if kind != EffectKind::ConnectorSync {
            if let Some(effect) = self.effect_mut(kind, &p.id) {
                effect.tracking.void();
            }
        }
        if kind == EffectKind::LlmCall {
            self.settle_prompt(&p.id);
        }
        if kind == EffectKind::ToolCall {
            for e in self.effects.values_mut() {
                match &mut e.payload {
                    EffectPayload::LlmCall(call) => call.unanswered.retain(|id| *id != p.id),
                    EffectPayload::ToolCall(tool) if e.id == p.id => tool.recorded = true,
                    _ => {}
                }
            }
        }
        self.dequeue(kind, &p.id);
    }

    pub fn apply(&mut self, event: &EventPayload, ctx: &ApplyContext) {
        let now = ctx.occurred_at;
        match event {
            EventPayload::SessionCreated(payload) => {
                self.status = SessionStatus::Idle;
                self.agent_id = Some(payload.agent_id.clone());
                self.owner = Some(payload.identity.clone());
                self.ancestry = payload.ancestry.clone();
                self.worker_retry = Some(payload.worker_retry.clone());
                self.worker = payload.worker.clone();
                self.events = payload.events.clone();
            }
            EventPayload::NewMessage(payload) => {
                self.answered(&payload.message, payload.branch.as_deref());
                self.grow(crate::tree::Node::of_message(payload), ctx.sequence);
            }
            EventPayload::BranchStarted(p) => {
                if let Some(l) = self.limited.as_mut().filter(|l| l.open()) {
                    if p.parent.is_none() && crate::context::is_summary_branch(&l.call.id, &p.id) {
                        l.summary = Some(p.id.clone());
                    }
                }
                if !self.branches.contains_key(&p.id) {
                    let parent = p.parent.as_deref().and_then(|b| self.branches.get(b));
                    let depth = parent.map_or(0, |b| b.depth) + 1;
                    let detached =
                        p.mode == BranchMode::Detached || parent.is_some_and(|b| b.detached);
                    let branch = BranchState::started(p, ctx.sequence, depth, detached);
                    self.tree.record(&p.id, branch.clone(), None);
                    self.branches.insert(p.id.clone(), branch);
                    if let Some(node) = crate::tree::Node::of_event(event) {
                        self.grow(node, ctx.sequence);
                    }
                }
            }
            EventPayload::BranchClosed(p) => {
                if let Some(branch) = self.branches.remove(&p.id) {
                    self.tree.record(&p.id, branch, Some(p.reason));
                }
            }
            EventPayload::BranchMoved(payload) => match payload.branch.as_deref() {
                Some(b) => {
                    if let Some(branch) = self.branches.get_mut(b) {
                        branch.leaf = Some(payload.leaf.clone());
                    }
                }
                None => self.head_id = Some(payload.leaf.clone()),
            },
            EventPayload::LlmCallRequested(payload) => self.apply_llm_requested(payload, ctx),
            EventPayload::LlmCallDispatched(payload) => self.apply_llm_dispatched(payload, ctx),
            EventPayload::ContextPlaced(payload) => {
                if let Some(node) = crate::tree::Node::of_event(event) {
                    self.grow(node, ctx.sequence);
                }
                if let Some(call) = self
                    .effect_mut(EffectKind::LlmCall, &payload.call)
                    .and_then(|e| e.llm_mut())
                {
                    call.reads_from = Some(payload.node.id.clone());
                }
            }
            EventPayload::LlmCallCompleted(payload) => {
                let branch = self.effect_branch(EffectKind::LlmCall, &payload.id);
                match &payload.response.usage {
                    Some(usage) if branch.is_none() => {
                        self.last_read = Some(LastRead {
                            call_id: payload.id.clone(),
                            tokens: usage.input,
                        });
                    }
                    _ => {}
                }
                let in_turn = !self.detached(branch.as_deref());
                self.track_usage(&payload.response.usage);
                if in_turn {
                    self.track_turn_usage(&payload.response.usage);
                }
                if let Some(c) = payload.response.cost {
                    self.cost += c;
                    if in_turn {
                        self.turn_cost += c;
                        self.turn_calls.llm.cost += c;
                    }
                }
                if let Some(e) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
                    e.tracking.complete();
                }
                self.settle_prompt(&payload.id);
                if let Some(call) = self
                    .effect_mut(EffectKind::LlmCall, &payload.id)
                    .and_then(|e| e.llm_mut())
                {
                    call.unanswered = payload
                        .response
                        .tool_calls
                        .iter()
                        .map(|c| c.id.clone())
                        .collect();
                }
            }
            EventPayload::LlmCallErrored(payload) => {
                self.turn_calls.llm.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::LlmCall, &payload.id) {
                    e.tracking.record_error(payload.retryable, now);
                }
                if self
                    .tracking(EffectKind::LlmCall, &payload.id)
                    .is_some_and(|t| t.status() == EffectStatus::Failed)
                {
                    self.settle_prompt(&payload.id);
                }
            }
            EventPayload::System1CallRequested(p) => self.apply_system1_requested(p, ctx),
            EventPayload::System1CallDispatched(p) => {
                if let Some(e) = self.effect_mut(EffectKind::System1Call, &p.id) {
                    e.tracking.dispatch(now);
                }
                self.dequeue(EffectKind::System1Call, &p.id);
            }
            EventPayload::System1CallCompleted(p) => {
                let usage = &p.response.usage;
                let in_turn = !self.detached(
                    self.effect_branch(EffectKind::System1Call, &p.id)
                        .as_deref(),
                );
                self.token_usage.add(usage);
                if in_turn {
                    self.turn_token_usage.add(usage);
                    self.turn_calls.system1.tokens.add(usage);
                }
                if let Some(c) = p.response.cost {
                    self.cost += c;
                    if in_turn {
                        self.turn_cost += c;
                        self.turn_calls.system1.cost += c;
                    }
                }
                if let Some(e) = self.effect_mut(EffectKind::System1Call, &p.id) {
                    e.tracking.complete();
                }
            }
            EventPayload::System1CallErrored(p) => {
                self.turn_calls.system1.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::System1Call, &p.id) {
                    e.tracking.record_error(p.retryable, now);
                }
            }
            EventPayload::ToolCallRequested(payload) => self.apply_tool_requested(payload, ctx),
            EventPayload::ToolCallDispatched(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.tracking.dispatch(now);
                }
                self.dequeue(EffectKind::ToolCall, &payload.id);
            }
            EventPayload::ToolCallCompleted(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.tracking.complete();
                    if let Some(tc) = e.tool_mut() {
                        tc.result = Some(payload.result.rendered());
                        tc.is_error = false;
                    }
                }
            }
            EventPayload::ToolCallStepped(payload) => {
                if let Some(tc) = self
                    .effect_mut(EffectKind::ToolCall, &payload.id)
                    .and_then(|e| e.tool_mut())
                {
                    tc.script_cpu_ms += payload.cpu_ms;
                    tc.script_file_bytes += payload.file_bytes;
                    if tc.script_skill_files.is_none() {
                        tc.script_skill_files = payload.skill_files.clone();
                    }
                    tc.script_asked.extend(payload.asked.iter().cloned());
                }
            }
            EventPayload::ToolCallErrored(payload) => {
                self.turn_calls.tool.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::ToolCall, &payload.id) {
                    e.tracking.record_error(payload.retryable, now);
                    let failed = e.tracking.status == EffectStatus::Failed;
                    if let Some(tc) = e.tool_mut().filter(|_| failed) {
                        tc.result = Some(payload.error.message.clone());
                        tc.is_error = true;
                    }
                }
            }
            EventPayload::SubagentRequested(payload) => self.apply_subagent_requested(payload, ctx),
            EventPayload::SubagentDispatched(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    e.tracking.dispatch(now);
                }
                self.dequeue(EffectKind::Subagent, &payload.id);
            }
            EventPayload::SubagentStarted(payload) => {
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    match e
                        .subagent()
                        .is_some_and(|sa| sa.mode == SpawnMode::Detached)
                    {
                        true => e.tracking.complete(),
                        false => e.tracking.run(),
                    }
                }
            }
            EventPayload::SubagentErrored(payload) => {
                self.turn_calls.subagent.errors += 1;
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    e.tracking.record_error(payload.retryable, now);
                    let failed = e.tracking.status == EffectStatus::Failed;
                    if let Some(sa) = e.subagent_mut().filter(|_| failed) {
                        sa.result = Some(payload.error.message.clone());
                        sa.is_error = true;
                    }
                }
            }
            EventPayload::SessionInterrupted(payload) => self.apply_interrupted(payload),
            EventPayload::InterruptResumed(p) => {
                self.open_interrupts
                    .retain(|i| i.interrupt_id != p.interrupt_id);
            }
            EventPayload::DecisionDispatched(p) => {
                let turnless = self.detached(self.decision_branch(&p.id))
                    || matches!(
                        self.worker_decision(&p.id).map(|d| &d.trigger),
                        Some(Trigger::BranchFinished {
                            mode: BranchMode::Detached,
                            ..
                        })
                    );
                let turn = self.phase.turn_id().filter(|_| !turnless).map(|id| {
                    Box::new(DecisionTurn {
                        id: id.to_string(),
                        metadata: self.turn_metadata.clone(),
                    })
                });
                let stamped = self
                    .worker_decision(&p.id)
                    .and_then(|d| d.turn.as_ref())
                    .is_some_and(|t| !self.turn_completed(&t.id));
                if let Some(e) = self.effect_mut(EffectKind::Decision, &p.id) {
                    e.tracking.dispatch(now);
                    if let Some(d) = e.decision_mut() {
                        d.dispatched_seq = Some(ctx.sequence);
                        if !stamped {
                            d.turn = turn;
                        }
                    }
                }
                self.dequeue(EffectKind::Decision, &p.id);
                self.status = SessionStatus::Idle;
            }
            EventPayload::DecisionCompleted(p) => {
                let owned = self.worker_decision(&p.id).and_then(|d| {
                    let turn = d.turn.as_ref()?;
                    d.trigger
                        .owns_turn(&p.id, &turn.id)
                        .then(|| turn.id.clone())
                });
                if let Some(turn) = owned.filter(|t| self.phase.active_turn_id() == Some(t)) {
                    self.quiet_end = Some(turn);
                }
                self.remove_effect(EffectKind::Decision, &p.id);
                self.dequeue(EffectKind::Decision, &p.id);
                self.release();
            }
            EventPayload::DecisionErrored(p) => self.apply_decision_errored(p, now),
            EventPayload::SessionMessageRequested(_) => {}
            EventPayload::DecisionQueued(p) => self.apply_decision_queued(p, ctx),
            EventPayload::DecisionDropped(p) => {
                self.remove_effect(EffectKind::Decision, &p.id);
                self.dequeue(EffectKind::Decision, &p.id);
                self.release();
            }
            EventPayload::CallVoided(p) => self.apply_call_voided(p),
            EventPayload::ConnectorSyncRequested(p) => {
                if !self.has_effect(EffectKind::ConnectorSync, &p.path.to_string()) {
                    self.insert_effect(
                        &p.path.to_string(),
                        EffectTracking::new(p.retry.clone(), now),
                        EffectPayload::ConnectorSync(ConnectorSyncState {
                            tools: Vec::new(),
                            prefix: None,
                            instructions: None,
                            error: None,
                            auth: None,
                        }),
                        None,
                    );
                }
                if let Some(e) = self.effect_mut(EffectKind::ConnectorSync, &p.path.to_string()) {
                    match e.tracking.status() {
                        EffectStatus::Failed | EffectStatus::Completed => e.tracking.restart(now),
                        _ => e.tracking.dispatch(now),
                    }
                }
            }
            EventPayload::ConnectorSyncCompleted(p) => {
                if let Some(e) = self.effect_mut(EffectKind::ConnectorSync, &p.path.to_string()) {
                    e.tracking.complete();
                    if let Some(sync) = e.connector_mut() {
                        sync.tools = p.tools.clone();
                        sync.prefix = p.prefix.clone();
                        sync.instructions = p.instructions.clone();
                        sync.error = None;
                        sync.auth = None;
                    }
                }
            }
            EventPayload::ConnectorSyncErrored(p) => {
                if let Some(e) = self.effect_mut(EffectKind::ConnectorSync, &p.path.to_string()) {
                    e.tracking.record_error(p.retryable, now);
                    if let Some(sync) = e.connector_mut() {
                        sync.error = Some(p.error.message.clone());
                        sync.auth = p.auth;
                    }
                }
            }
            EventPayload::ConnectorAuthFailed(p) => {
                if let Some(sync) = self
                    .effect_mut(EffectKind::ConnectorSync, &p.path.to_string())
                    .and_then(EffectState::connector_mut)
                {
                    sync.auth = Some(p.auth);
                }
            }
            EventPayload::StateUpdated(_) | EventPayload::ConfigUpdated(_) => {
                if let Some(node) = crate::tree::Node::of_event(event) {
                    self.grow(node, ctx.sequence);
                }
            }
            EventPayload::SessionCancelled => {
                self.limited = None;
            }
            EventPayload::SessionDone(_) => {
                if !self.ancestry.is_empty() {
                    self.status = SessionStatus::Done;
                } else {
                    self.status = SessionStatus::Idle;
                }
                self.schedule_queue
                    .retain(|e| e.kind != EffectKind::TurnEnd);
            }
            EventPayload::SubagentTurnCompleted(payload) => {
                self.subagent_cost += payload.cost;
                self.turn_cost += payload.cost;
                self.subagent_token_usage.add(&payload.token_usage);
                self.turn_token_usage.add(&payload.token_usage);
                self.turn_calls.subagent.cost += payload.cost;
                self.turn_calls.subagent.tokens.add(&payload.token_usage);
                self.turn_calls.subagent.errors += u32::from(payload.error.is_some());
                if let Some(turn_id) = &payload.turn_id {
                    remember(&mut self.detached_turns, turn_id);
                }
                if let Some(e) = self.effect_mut(EffectKind::Subagent, &payload.id) {
                    if let Some(sa) = e.subagent_mut() {
                        sa.result = Some(match &payload.error {
                            Some(error) => error.message.clone(),
                            None => json_to_string(&payload.data),
                        });
                        sa.is_error = payload.error.is_some();
                    }
                    match e.tracking.status() {
                        EffectStatus::Queued | EffectStatus::RetryScheduled => {
                            e.tracking.dispatch(now);
                            e.tracking.complete();
                        }
                        EffectStatus::Pending | EffectStatus::Running => e.tracking.complete(),
                        _ => {}
                    }
                }
                self.dequeue(EffectKind::Subagent, &payload.id);
            }
            EventPayload::TurnJoined(p) => self.turn_metadata.extend(p.metadata.clone()),
            EventPayload::TurnStarted(p) => {
                self.phase = TurnPhase::Active {
                    turn_id: p.turn_id.clone(),
                };
                self.turn_started_seq = Some(ctx.sequence);
                self.turn_head = self.head_id.clone();
                self.turn_metadata = p.metadata.clone();
                self.quiet_end = None;
                self.turn_called = false;
                self.context_kept = false;
                self.turn_cost = Decimal::ZERO;
                self.turn_token_usage = Usage::default();
                self.turn_calls = TurnCalls::default();
                self.turn_started_at = Some(now);
            }
            EventPayload::TurnCompleted(payload) => {
                self.turns += 1;
                remember(&mut self.recent_turns, &payload.turn_id);
                self.last_turn_failed = payload.error.is_some();
                if !matches!(self.phase, TurnPhase::Finalizing(_)) {
                    self.last_turn_at = Some(now);
                }
                self.data = payload.data.clone();
                self.turn_cost = Decimal::ZERO;
                self.turn_token_usage = Usage::default();
                self.turn_calls = TurnCalls::default();
                self.turn_started_at = None;
                self.phase = TurnPhase::Idle;
                self.turn_started_seq = None;
                self.limited = None;
                self.turn_metadata.clear();
                self.quiet_end = None;
                self.dequeue(EffectKind::TurnEnd, &payload.turn_id);
            }
            EventPayload::WorkUnsent(p) => {
                if let Some(e) = self.effect_mut(p.kind, &p.id) {
                    e.tracking.unsent(p.retry_at);
                }
            }
            EventPayload::DeliveryFailed(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.seq.saturating_sub(1));
                d.attempts = p.attempt;
                d.failing_since.get_or_insert(now);
                d.retry_at = Some(p.retry_at);
            }
            EventPayload::DeliverySkipped(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.seq);
                d.attempts = 0;
                d.retry_at = None;
                d.skipped += 1;
            }
            EventPayload::DeliveryParked(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.seq.saturating_sub(1));
                d.attempts = p.attempt;
                d.failing_since.get_or_insert(now);
                d.retry_at = None;
                d.parked = Some(crate::delivery::Parked {
                    seq: p.seq,
                    error: p.error.clone(),
                });
            }
            EventPayload::DeliveryRecovered(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.floor = d.floor.max(p.through);
                d.attempts = 0;
                d.failing_since = None;
                d.retry_at = None;
            }
            EventPayload::ContextLimited(p) => {
                self.limited = Some(Limited {
                    reason: p.reason,
                    call: p.call.clone(),
                    summary: None,
                    released: false,
                });
            }
            EventPayload::ContextReplaced(_) => {
                self.last_read = None;
                if let Some(l) = &mut self.limited {
                    l.released = true;
                }
            }
            EventPayload::ContextKept => {
                self.context_kept = true;
                if let Some(l) = &mut self.limited {
                    l.released = true;
                }
            }
            EventPayload::DeliveryResumed(p) => {
                let d = self.deliveries.entry(p.subscription.clone()).or_default();
                d.attempts = 0;
                d.failing_since = None;
                d.retry_at = None;
                d.parked = None;
            }
        }
    }

    fn track_usage(&mut self, usage: &Option<Usage>) {
        if let Some(u) = usage {
            self.token_usage.add(u);
        }
    }

    fn track_turn_usage(&mut self, usage: &Option<Usage>) {
        if let Some(u) = usage {
            self.turn_token_usage.add(u);
            self.turn_calls.llm.tokens.add(u);
        }
    }
}

pub struct ApplyContext {
    pub occurred_at: DateTime<Utc>,
    pub sequence: u64,
}

const REMEMBERED_TURNS: usize = 64;

fn remember(turns: &mut Vec<String>, turn_id: &str) {
    turns.retain(|t| t != turn_id);
    turns.push(turn_id.to_string());
    let over = turns.len().saturating_sub(REMEMBERED_TURNS);
    turns.drain(..over);
}

#[cfg(test)]
mod turn_fold_tests {
    use serde_json::json;

    use super::*;

    fn apply(s: &mut SessionState, payload: serde_json::Value) {
        let event: EventPayload = serde_json::from_value(payload).unwrap();
        s.apply(
            &event,
            &ApplyContext {
                occurred_at: Utc::now(),
                sequence: 1,
            },
        );
    }

    #[test]
    fn the_last_turn_decides_whether_the_session_failed() {
        let mut s = SessionState::new("sess-1".to_string());
        apply(
            &mut s,
            json!({"type": "turn.completed", "turn_id": "t1", "turn_cost": "0", "turn_token_usage": crate::protocol::Usage::default(), "turn_calls": crate::events::TurnCalls::default(), "error": {"message": "boom", "code": "invalid_response"}}),
        );
        assert!(s.last_turn_failed);
        apply(
            &mut s,
            json!({"type": "turn.completed", "turn_id": "t2", "turn_cost": "0", "turn_token_usage": crate::protocol::Usage::default(), "turn_calls": crate::events::TurnCalls::default()}),
        );
        assert!(!s.last_turn_failed);
    }
}
