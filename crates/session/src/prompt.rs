use std::collections::BTreeMap;

use crate::prompt_context;
use crate::protocol::{ConnectorTool, DraftMessage, EffectKind, LlmRequest, LlmTool, Message};
use crate::state::View;
use crate::state::{At, SessionState};

#[derive(Debug, Clone)]
pub struct CallSent {
    pub asked: crate::tree::Asked,
    pub asked_tools: Option<Vec<LlmTool>>,
    pub tools: Option<Vec<LlmTool>>,
}

#[derive(Debug, Clone)]
pub struct Placing {
    pub prompt: Vec<Message>,
    pub placed: Vec<prompt_context::Placed>,
}

impl SessionState {}

impl At<'_, '_> {
    pub fn prompt(&self, call: &str) -> Vec<Message> {
        let view: Vec<DraftMessage> = self
            .transcript()
            .into_iter()
            .map(DraftMessage::from)
            .collect();
        let mut prompt: Vec<Message> = match self.config() {
            Some(config) => config.prompt_for(&view),
            None => view,
        }
        .into_iter()
        .enumerate()
        .map(|(i, m)| m.recorded(format!("{call}:{i}")))
        .collect();
        prompt_context::restore(&mut prompt, &self.placed_on_path());
        prompt
    }

    pub fn place(&self, call: &str) -> Placing {
        let mut prompt = self.prompt(call);
        let mut placed = Vec::new();
        let open = prompt_context::Open {
            system: self.system_prefix_open(),
            inline: self.inline_open(),
        };
        prompt_context::merge(
            &mut prompt,
            &mut placed,
            call,
            open,
            prompt_context::owed(*self),
        );
        Placing { prompt, placed }
    }

    pub fn system_prefix_open(&self) -> bool {
        for node in self.up() {
            match &node.fact {
                crate::tree::Fact::Context { .. } => return false,
                crate::tree::Fact::Reset { .. } => break,
                _ => {}
            }
        }
        true
    }

    fn inline_open(&self) -> bool {
        for node in self.up() {
            match &node.fact {
                crate::tree::Fact::Context { .. } => return false,
                crate::tree::Fact::Message { .. } => return true,
                _ => {}
            }
        }
        true
    }

    /// In code mode, the one tool the model gets: `run_code`, declaring the
    /// tools a script calls that are not deferred. It does not change while
    /// the session runs, but for tools that connect later.
    /// The tools a call asking for `asked` is sent from here, and how its
    /// context node records them.
    pub fn sends(&self, asked: Option<Vec<LlmTool>>) -> (Option<Vec<LlmTool>>, crate::tree::Tools) {
        let tools = match self.run_code() {
            Some(run_code) => Some(vec![run_code]),
            None => sent_tools(asked, &self.connector_tools().tools, &self.locked_tools()),
        };
        let record = self.record(&tools, crate::tree::Fact::sent_tools);
        (tools, record)
    }

    /// `tools` as a context node here records them: a reference to the
    /// nearest one above that holds the same set, or the set.
    pub fn record(
        &self,
        tools: &Option<Vec<LlmTool>>,
        pick: fn(&crate::tree::Fact) -> Option<&crate::tree::Tools>,
    ) -> crate::tree::Tools {
        use crate::tree::Tools;
        let nearest = self
            .up()
            .find_map(|node| Some((node.id().to_string(), pick(&node.fact)?.clone())));
        match nearest {
            Some((_, Tools::As(set))) if self.session().tree.resolve(&set, pick) == *tools => {
                Tools::As(set)
            }
            Some((id, Tools::Set(set))) if set == *tools => Tools::As(id),
            _ => Tools::Set(tools.clone()),
        }
    }

    /// What `call` asked for and was sent, from its context node on this
    /// path.
    pub fn sent(&self, call: &str) -> Option<CallSent> {
        let node = self.up().find(|node| {
            matches!(&node.fact, crate::tree::Fact::Context { call: here, .. } if here == call)
        })?;
        let crate::tree::Fact::Context { asked, .. } = &node.fact else {
            return None;
        };
        let tree = &self.session().tree;
        Some(CallSent {
            asked: asked.clone(),
            asked_tools: tree.resolve(node.id(), crate::tree::Fact::asked_tools),
            tools: tree.tools(node.id()),
        })
    }
}

/// The tools a call sends: those it asked for, then its connections', but
/// those a skill holds back.
fn sent_tools(
    asked: Option<Vec<LlmTool>>,
    connector_tools: &[ConnectorTool],
    locked: &BTreeMap<String, Vec<String>>,
) -> Option<Vec<LlmTool>> {
    let mut sent = asked;
    if !connector_tools.is_empty() {
        let tools = sent.get_or_insert_with(Vec::new);
        for tool in connector_tools {
            if !tools.iter().any(|t| t.name == tool.name) {
                tools.push(tool.to_llm_tool());
            }
        }
    }
    if let Some(tools) = sent.as_mut() {
        tools.retain(|t| !locked.contains_key(&t.name));
    }
    sent
}

pub fn fingerprint(prompt: &[Message], tools: &Option<Vec<crate::protocol::LlmTool>>) -> String {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for m in prompt {
        let mut m = DraftMessage::from(m.clone());
        m.id = None;
        hasher.update(&serde_json::to_vec(&m).unwrap_or_default());
        hasher.update(b"\n");
    }
    hasher.update(&serde_json::to_vec(tools).unwrap_or_default());
    format!("{:016x}", hasher.digest())
}

impl View {
    pub fn prompt(&self, id: &str) -> Option<Vec<Message>> {
        let effect = self.effect(EffectKind::LlmCall, id)?;
        let node = effect
            .llm()?
            .reads_from
            .as_deref()
            .or(effect.asked_at.as_deref());
        Some(self.at(node).prompt(id))
    }
}

impl View {
    /// The tools a call was sent, or asked for before it was dispatched.
    pub fn tools_sent(&self, id: &str) -> Option<Vec<LlmTool>> {
        match self.llm_call(id) {
            Some(call) => match call.reads_from.as_deref() {
                Some(node) => self.tree.tools(node),
                None => call.spec.tools.clone(),
            },
            None => self.at(Some(id)).sent(id)?.tools,
        }
    }
}

impl View {
    /// The request the model gets: its prompt, and the tools it was sent.
    pub fn sent_request(&self, id: &str) -> Option<LlmRequest> {
        let call = self.llm_call(id)?;
        let mut request = call.spec.to_request(self.prompt(id)?);
        request.tools = self.tools_sent(id);
        Some(request)
    }
}
