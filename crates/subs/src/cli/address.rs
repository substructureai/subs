use crate::connectors::registry::ConnectionPath;

pub enum Address {
    Llm(String),
    System1(String),
    Worker(String),
    Events(String),
    SlackApp(String),
    Connection(ConnectionPath),
}

impl Address {
    pub fn parse(written: &str) -> Option<Self> {
        if let Some(agent) = written
            .strip_prefix("agent.")
            .and_then(|rest| rest.strip_suffix(".slack"))
        {
            return flat(agent).map(|a| Self::SlackApp(a.to_string()));
        }
        if let Some(id) = written.strip_prefix("llm.") {
            return flat(id).map(|i| Self::Llm(i.to_string()));
        }
        if let Some(id) = written.strip_prefix("system1.") {
            return flat(id).map(|i| Self::System1(i.to_string()));
        }
        if let Some(id) = written.strip_prefix("worker.") {
            return flat(id).map(|i| Self::Worker(i.to_string()));
        }
        if let Some(id) = written.strip_prefix("events.") {
            return flat(id).map(|i| Self::Events(i.to_string()));
        }
        ConnectionPath::parse(written).map(Self::Connection)
    }
}

fn flat(id: &str) -> Option<&str> {
    (!id.is_empty() && !id.contains('.')).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_grammar_covers_every_command_address() {
        assert!(
            matches!(Address::parse("llm.openrouter"), Some(Address::Llm(b)) if b == "openrouter")
        );
        assert!(matches!(Address::parse("worker.main"), Some(Address::Worker(w)) if w == "main"));
        assert!(matches!(Address::parse("system1.cf"), Some(Address::System1(b)) if b == "cf"));
        assert!(matches!(Address::parse("events.app"), Some(Address::Events(c)) if c == "app"));
        assert!(
            matches!(Address::parse("agent.support.slack"), Some(Address::SlackApp(a)) if a == "support")
        );
        assert!(matches!(
            Address::parse("mcp.sentry"),
            Some(Address::Connection(ConnectionPath::Mcp(_)))
        ));
        assert!(matches!(
            Address::parse("plugin.reggu.mcp.code"),
            Some(Address::Connection(ConnectionPath::PluginServer { .. }))
        ));
        for wrong in ["llm.", "worker.a.b", "sentry.mcp", "agent..slack"] {
            assert!(Address::parse(wrong).is_none(), "{wrong}");
        }
    }
}
