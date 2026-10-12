use crate::ids::Ids;
use crate::protocol::{Content, DraftMessage, Message, StoredContent};

impl Content {
    pub fn text(&self) -> Option<&str> {
        match self {
            Content::Text(s) => Some(s.as_str()),
            Content::Parts(parts) => {
                if parts.len() == 1 {
                    if let StoredContent::Text { text } = &parts[0] {
                        return Some(text.as_str());
                    }
                }
                None
            }
        }
    }

    pub fn text_owned(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    StoredContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

impl From<String> for Content {
    fn from(s: String) -> Self {
        Content::Text(s)
    }
}

impl From<&str> for Content {
    fn from(s: &str) -> Self {
        Content::Text(s.to_string())
    }
}

impl From<Message> for DraftMessage {
    fn from(m: Message) -> Self {
        DraftMessage {
            id: Some(m.id),
            role: m.role,
            content: m.content,
            tool_calls: (!m.tool_calls.is_empty()).then_some(m.tool_calls),
            tool_call_id: m.tool_call_id,
            name: m.name,
            reasoning: m.reasoning,
        }
    }
}

impl DraftMessage {
    pub fn recorded(self, fallback: String) -> Message {
        let id = self.id.clone().unwrap_or(fallback);
        Message {
            id,
            role: self.role,
            content: self.content,
            tool_calls: self.tool_calls.unwrap_or_default(),
            tool_call_id: self.tool_call_id,
            name: self.name,
            reasoning: self.reasoning,
        }
    }

    pub fn record(self, ids: &Ids) -> Message {
        Message {
            id: self.id.unwrap_or_else(|| ids.next()),
            role: self.role,
            content: self.content,
            tool_calls: self.tool_calls.unwrap_or_default(),
            tool_call_id: self.tool_call_id,
            name: self.name,
            reasoning: self.reasoning,
        }
    }

    pub fn rerecord(self, ids: &Ids) -> Message {
        Message {
            id: ids.next(),
            role: self.role,
            content: self.content,
            tool_calls: self.tool_calls.unwrap_or_default(),
            tool_call_id: self.tool_call_id,
            name: self.name,
            reasoning: self.reasoning,
        }
    }
}
