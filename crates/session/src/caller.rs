use std::collections::HashMap;

use crate::protocol::{SessionOwner, Subject};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Caller {
    System {
        tenant_id: String,
    },
    ApiKey {
        tenant_id: String,
        key_id: String,
    },
    Operator {
        tenant_id: String,
        subject: Subject,
    },
    Frontend {
        tenant_id: String,
        subject: Subject,
        attrs: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerKind {
    System,
    ApiKey,
    Operator,
    Frontend,
}

impl Caller {
    pub fn kind(&self) -> CallerKind {
        match self {
            Caller::System { .. } => CallerKind::System,
            Caller::ApiKey { .. } => CallerKind::ApiKey,
            Caller::Operator { .. } => CallerKind::Operator,
            Caller::Frontend { .. } => CallerKind::Frontend,
        }
    }

    pub fn tenant_id(&self) -> &str {
        match self {
            Caller::System { tenant_id }
            | Caller::ApiKey { tenant_id, .. }
            | Caller::Operator { tenant_id, .. }
            | Caller::Frontend { tenant_id, .. } => tenant_id,
        }
    }

    pub fn subject(&self) -> Option<&Subject> {
        match self {
            Caller::System { .. } | Caller::ApiKey { .. } => None,
            Caller::Operator { subject, .. } | Caller::Frontend { subject, .. } => Some(subject),
        }
    }

    pub fn owns(&self, owner: &SessionOwner) -> bool {
        let Some(subject) = self.subject() else {
            return false;
        };
        owner.tenant_id == self.tenant_id() && owner.requester.subject.as_ref() == Some(subject)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Issuer;
    use crate::protocol::Requester;

    fn owner(issuer: Issuer, id: &str) -> SessionOwner {
        SessionOwner {
            tenant_id: "tenant-a".to_string(),
            requester: Requester::private(Subject::new(issuer, id)),
            metadata: HashMap::new(),
        }
    }

    fn frontend(id: &str) -> Caller {
        Caller::Frontend {
            tenant_id: "tenant-a".to_string(),
            subject: Subject::new(Issuer::app(), id),
            attrs: HashMap::new(),
        }
    }

    #[test]
    fn the_same_id_from_another_source_is_another_person() {
        assert!(frontend("alex").owns(&owner(Issuer::app(), "alex")));
        assert!(!frontend("alex").owns(&owner(Issuer::operator(), "alex")));
        assert!(!frontend("alex").owns(&owner(Issuer::new("chat"), "alex")));
        assert!(!frontend("alex").owns(&owner(Issuer::app(), "sam")));
    }

    #[test]
    fn a_session_in_another_tenant_is_never_ours() {
        let other = SessionOwner {
            tenant_id: "tenant-b".to_string(),
            requester: Requester::private(Subject::new(Issuer::app(), "alex")),
            metadata: HashMap::new(),
        };
        assert!(!frontend("alex").owns(&other));
    }

    #[test]
    fn a_machine_owns_nothing() {
        let key = Caller::ApiKey {
            tenant_id: "tenant-a".to_string(),
            key_id: "key-1".to_string(),
        };
        let system = Caller::System {
            tenant_id: "tenant-a".to_string(),
        };
        assert!(key.subject().is_none());
        assert!(!key.owns(&owner(Issuer::app(), "key-1")));
        assert!(!system.owns(&owner(Issuer::app(), "anything")));
    }

    #[test]
    fn an_owner_without_a_subject_is_nobodys() {
        let stored = r#"{"tenant_id":"tenant-a"}"#;
        let owner: SessionOwner = serde_json::from_str(stored).unwrap();
        assert!(owner.requester.subject.is_none());
        assert!(!frontend("alex").owns(&owner));
    }
}
