use super::SessionError;
use crate::decision::{LlmHandler, ToolHandler};
use crate::protocol::InterruptOrigin;
use crate::state::{LlmCallState, SessionState, ToolCallState};
use crate::Caller;

impl SessionState {
    pub fn ensure_internal(caller: &Caller) -> Result<(), SessionError> {
        match caller {
            Caller::System { .. } => Ok(()),
            _ => Err(SessionError::SessionAccessDenied),
        }
    }

    pub fn ensure_worker_or_system(caller: &Caller) -> Result<(), SessionError> {
        match caller {
            Caller::System { .. } | Caller::ApiKey { .. } => Ok(()),
            Caller::Operator { .. } | Caller::Frontend { .. } => {
                Err(SessionError::SessionAccessDenied)
            }
        }
    }

    pub fn ensure_operator_or_system(caller: &Caller) -> Result<(), SessionError> {
        match caller {
            Caller::System { .. } | Caller::ApiKey { .. } | Caller::Operator { .. } => Ok(()),
            Caller::Frontend { .. } => Err(SessionError::SessionAccessDenied),
        }
    }

    pub(super) fn ensure_tenant_matches(
        caller: &Caller,
        tenant_id: &str,
    ) -> Result<(), SessionError> {
        match caller {
            Caller::System { .. } => Ok(()),
            Caller::ApiKey {
                tenant_id: caller_tenant,
                ..
            }
            | Caller::Operator {
                tenant_id: caller_tenant,
                ..
            }
            | Caller::Frontend {
                tenant_id: caller_tenant,
                ..
            } => {
                if caller_tenant != tenant_id {
                    return Err(SessionError::SessionAccessDenied);
                }
                Ok(())
            }
        }
    }

    pub(super) fn caller_interrupt_origin(caller: &Caller) -> InterruptOrigin {
        match caller {
            Caller::System { .. } => InterruptOrigin::System,
            Caller::Operator { .. } => InterruptOrigin::Operator,
            Caller::ApiKey { .. } => InterruptOrigin::Machine,
            Caller::Frontend { .. } => InterruptOrigin::Frontend,
        }
    }

    pub(super) fn ensure_owns_session(&self, caller: &Caller) -> Result<(), SessionError> {
        match caller {
            Caller::System { .. } | Caller::ApiKey { .. } | Caller::Operator { .. } => Ok(()),
            Caller::Frontend { .. } => {
                let owner = self
                    .owner
                    .as_ref()
                    .ok_or(SessionError::SessionAccessDenied)?;
                match caller.owns(owner) {
                    true => Ok(()),
                    false => Err(SessionError::SessionAccessDenied),
                }
            }
        }
    }

    pub fn check_tool_call_caller(
        &self,
        tc: &ToolCallState,
        caller: &Caller,
    ) -> Result<(), SessionError> {
        self.ensure_owns_session(caller)?;
        if matches!(caller, Caller::Frontend { .. }) && tc.handler != ToolHandler::Client {
            return Err(SessionError::EffectWrongHandler);
        }
        Ok(())
    }

    pub fn check_llm_call_caller(
        &self,
        call: Option<&LlmCallState>,
        caller: &Caller,
    ) -> Result<(), SessionError> {
        match caller {
            Caller::System { .. } => Ok(()),
            Caller::Operator { .. } | Caller::Frontend { .. } => {
                Err(SessionError::EffectWrongHandler)
            }
            Caller::ApiKey { .. } => match call {
                Some(c) if c.handler == LlmHandler::Worker => Ok(()),
                Some(_) => Err(SessionError::EffectWrongHandler),
                None => Err(SessionError::EffectNotFound),
            },
        }
    }
}
