use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::protocol::{ErrorCode, ErrorInfo};
use crate::runtime::inbox::{execute, ExecuteInput, Inbox};
use crate::runtime::queue::{holding, Queue, Received};
use crate::runtime::session::command::{CommandPayload, SettleError};
use crate::runtime::session::effects::{QUEUED, RUN};
use crate::runtime::session::state::EffectKind;
use crate::runtime::span::SpanContext;
use crate::runtime::Caller;

const RESULT_NOT_SAVED: Duration = Duration::from_secs(5);
const WATCH: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct TaskBound {
    pub tenant_id: String,
    pub session_id: String,
    pub kind: EffectKind,
    pub id: String,
    pub attempt: Option<u32>,
    pub enqueued_at: DateTime<Utc>,
    pub queue_timeout: Option<Duration>,
    pub run_timeout: Option<Duration>,
    pub span: SpanContext,
}

impl TaskBound {
    pub fn work(&self) -> substructure_session::WorkId {
        substructure_session::WorkId::new(self.kind, self.id.clone(), self.attempt)
    }

    fn waited(&self, now: DateTime<Utc>) -> Duration {
        (now - self.enqueued_at).to_std().unwrap_or(Duration::ZERO)
    }
}

pub struct Bound {
    pub tenant_id: String,
    pub session_id: String,
    pub kind: EffectKind,
    pub id: String,
    pub attempt: Option<u32>,
    pub queue_timeout: Option<Duration>,
    pub run_timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutorPool {
    pub workers: usize,
    pub concurrency: usize,
}

pub trait BoundedTask: Clone + Serialize + DeserializeOwned + Send + Sync + 'static {
    fn bound(&self) -> Option<Bound>;
}

pub fn spawn_bounded_executors<T, F, Fut>(
    inbox: Arc<dyn Inbox>,
    queue: Arc<dyn Queue<T>>,
    topic: &'static str,
    pool: ExecutorPool,
    cancel: CancellationToken,
    handle: F,
) -> Vec<JoinHandle<()>>
where
    T: BoundedTask,
    F: Fn(T, SpanContext) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send,
{
    let workers = pool.workers.max(1);
    let permits = Arc::new(Semaphore::new(
        workers.saturating_mul(pool.concurrency.max(1)),
    ));
    (0..workers)
        .map(|_| {
            let inbox = inbox.clone();
            let queue = queue.clone();
            let cancel = cancel.clone();
            let handle = handle.clone();
            let permits = permits.clone();
            tokio::spawn(async move {
                loop {
                    let received = tokio::select! {
                        _ = cancel.cancelled() => return,
                        received = queue.receive(topic) => received,
                    };
                    let received = match received {
                        Ok(received) => received,
                        Err(error) => {
                            tracing::warn!(topic, %error, "receiving a task failed");
                            tokio::time::sleep(RESULT_NOT_SAVED).await;
                            continue;
                        }
                    };
                    let Ok(permit) = permits.clone().acquire_owned().await else {
                        return;
                    };
                    let inbox = inbox.clone();
                    let queue = queue.clone();
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        work(inbox.as_ref(), queue.as_ref(), received, handle).await;
                    });
                }
            })
        })
        .collect()
}

async fn work<T, F, Fut>(inbox: &dyn Inbox, queue: &dyn Queue<T>, received: Received<T>, handle: F)
where
    T: BoundedTask,
    F: Fn(T, SpanContext) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let Received { envelope, receipt } = received;
    let bound = envelope.task.bound().map(|b| TaskBound {
        tenant_id: b.tenant_id,
        session_id: b.session_id,
        kind: b.kind,
        id: b.id,
        attempt: b.attempt,
        enqueued_at: envelope.enqueued_at,
        queue_timeout: b.queue_timeout,
        run_timeout: b.run_timeout,
        span: envelope.span.clone(),
    });
    let open = match &bound {
        Some(bound) => {
            inbox
                .is_open(&bound.tenant_id, &bound.session_id, &bound.work())
                .await
        }
        None => true,
    };
    let result = match open {
        false => Ok(()),
        true => {
            let watch = bound.clone();
            let run = run_bounded(
                bound,
                envelope.task,
                envelope.span,
                handle,
                |bound, message| async move { expire(inbox, &bound, message).await },
            );
            holding(queue, &receipt, watched(inbox, watch.as_ref(), run)).await
        }
    };
    let finished = match result {
        Ok(()) => queue.ack(&receipt).await,
        Err(error) => {
            tracing::warn!(id = %envelope.id, %error, "a task's result was not saved; it runs again");
            queue.retry(&receipt, RESULT_NOT_SAVED).await
        }
    };
    if let Err(error) = finished {
        tracing::warn!(id = %envelope.id, %error, "finishing a task failed");
    }
}

async fn watched(
    inbox: &dyn Inbox,
    bound: Option<&TaskBound>,
    run: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let Some(bound) = bound else {
        return run.await;
    };
    tokio::select! {
        result = run => result,
        () = closed(inbox, bound) => {
            tracing::info!(
                session_id = %bound.session_id,
                kind = bound.kind.label(),
                id = %bound.id,
                "stopping a task whose call ended"
            );
            Ok(())
        }
    }
}

async fn closed(inbox: &dyn Inbox, bound: &TaskBound) {
    loop {
        tokio::time::sleep(WATCH).await;
        if !inbox
            .is_open(&bound.tenant_id, &bound.session_id, &bound.work())
            .await
        {
            return;
        }
    }
}

async fn run_bounded<T, F, Fut, E, EFut>(
    bound: Option<TaskBound>,
    task: T,
    span: SpanContext,
    handle: F,
    expire: E,
) -> Result<(), String>
where
    F: Fn(T, SpanContext) -> Fut,
    Fut: Future<Output = Result<(), String>>,
    E: Fn(TaskBound, &'static str) -> EFut,
    EFut: Future<Output = Result<(), String>>,
{
    let Some(bound) = bound else {
        return handle(task, span).await;
    };
    let waited = bound.waited(Utc::now());
    if bound.queue_timeout.is_some_and(|q| waited > q) {
        tracing::warn!(
            session_id = %bound.session_id,
            kind = bound.kind.label(),
            id = %bound.id,
            waited_secs = waited.as_secs(),
            "dropping a task that waited too long for an executor"
        );
        return expire(bound, QUEUED).await;
    }
    let Some(limit) = bound.run_timeout else {
        return handle(task, span).await;
    };
    match tokio::time::timeout(limit, handle(task, span)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                session_id = %bound.session_id,
                kind = bound.kind.label(),
                id = %bound.id,
                run_secs = limit.as_secs(),
                "cancelling a task that ran too long"
            );
            expire(bound, RUN).await
        }
    }
}

async fn expire(inbox: &dyn Inbox, bound: &TaskBound, message: &str) -> Result<(), String> {
    let command = CommandPayload::settle(
        bound.kind,
        bound.id.clone(),
        bound.attempt,
        SettleError::new(ErrorInfo::new(ErrorCode::DeadlineExceeded, message), true),
    );
    execute(
        inbox,
        ExecuteInput {
            id: None,
            session_id: bound.session_id.clone(),
            caller: Caller::System {
                tenant_id: bound.tenant_id.clone(),
            },
            command,
            span: bound.span.child("expire"),
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

const SAVE_TRIES: u32 = 4;

pub async fn settled(
    inbox: &dyn Inbox,
    tenant_id: &str,
    session_id: &str,
    command: CommandPayload,
    span: SpanContext,
) -> Result<(), String> {
    let mut wait = Duration::from_millis(200);
    let mut tries = 0;
    loop {
        let saved = execute(
            inbox,
            ExecuteInput {
                id: None,
                session_id: session_id.to_string(),
                caller: Caller::System {
                    tenant_id: tenant_id.to_string(),
                },
                command: command.clone(),
                span: span.clone(),
            },
        )
        .await;
        match saved {
            Ok(_) | Err(crate::runtime::inbox::ExecuteError::Command(_)) => return Ok(()),
            Err(error) if tries + 1 >= SAVE_TRIES => return Err(error.to_string()),
            Err(error) => {
                tracing::warn!(%session_id, %error, "saving a result failed; trying again");
                tokio::time::sleep(wait).await;
                wait = wait.saturating_mul(2);
                tries += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use super::*;

    struct Task(Option<TaskBound>);

    fn bound(queue: Option<u64>, run: Option<u64>, waited_ms: i64) -> Option<TaskBound> {
        Some(TaskBound {
            tenant_id: "t".into(),
            session_id: "s".into(),
            kind: EffectKind::ToolCall,
            id: "tc-1".into(),
            attempt: Some(0),
            enqueued_at: Utc::now() - chrono::Duration::milliseconds(waited_ms),
            queue_timeout: queue.map(Duration::from_millis),
            run_timeout: run.map(Duration::from_millis),
            span: SpanContext::root(),
        })
    }

    async fn drive(task: Task, work: Duration) -> (bool, Option<&'static str>) {
        let ran = Arc::new(AtomicBool::new(false));
        let expired: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
        let (r, e) = (ran.clone(), expired.clone());
        let _ = run_bounded(
            task.0,
            (),
            SpanContext::root(),
            |_, _| {
                let r = r.clone();
                async move {
                    tokio::time::sleep(work).await;
                    r.store(true, Ordering::SeqCst);
                    Ok(())
                }
            },
            |_, message| {
                let e = e.clone();
                async move {
                    *e.lock().unwrap() = Some(message);
                    Ok(())
                }
            },
        )
        .await;
        let out = *expired.lock().unwrap();
        (ran.load(Ordering::SeqCst), out)
    }

    #[tokio::test]
    async fn work_inside_both_bounds_runs_and_settles_itself() {
        let (ran, expired) = drive(Task(bound(Some(500), Some(500), 0)), Duration::ZERO).await;
        assert!(ran);
        assert_eq!(expired, None, "the work settles its own result");
    }

    #[tokio::test]
    async fn a_task_past_its_queue_bound_never_runs() {
        let (ran, expired) = drive(Task(bound(Some(10), Some(500), 200)), Duration::ZERO).await;
        assert!(!ran, "it was stale before an executor reached it");
        assert_eq!(expired, Some(QUEUED));
    }

    #[tokio::test]
    async fn work_past_its_run_bound_is_cancelled() {
        let (ran, expired) = drive(
            Task(bound(Some(500), Some(20), 0)),
            Duration::from_millis(5_000),
        )
        .await;
        assert!(!ran, "the future was dropped before it finished");
        assert_eq!(expired, Some(RUN));
    }

    #[tokio::test]
    async fn a_long_wait_does_not_shorten_the_run() {
        let (ran, expired) = drive(
            Task(bound(Some(500), Some(200), 400)),
            Duration::from_millis(100),
        )
        .await;
        assert!(ran, "400ms queued, and still a full run bound to work in");
        assert_eq!(expired, None);
    }

    #[tokio::test]
    async fn an_unbounded_run_is_never_cancelled() {
        let (ran, expired) =
            drive(Task(bound(Some(500), None, 0)), Duration::from_millis(50)).await;
        assert!(ran);
        assert_eq!(expired, None);
    }

    #[tokio::test]
    async fn plumbing_no_effect_waits_on_runs_unbounded() {
        let (ran, expired) = drive(Task(None), Duration::from_millis(50)).await;
        assert!(ran);
        assert_eq!(expired, None, "nothing is holding a result to settle");
    }

    #[tokio::test]
    async fn a_stamp_in_the_future_is_no_wait() {
        let (ran, _) = drive(Task(bound(Some(10), Some(500), -5_000)), Duration::ZERO).await;
        assert!(ran);
    }

    struct Closing(AtomicBool);

    #[async_trait::async_trait]
    impl Inbox for Closing {
        async fn submit(
            &self,
            _tenant_id: &str,
            _session_id: &str,
            _command: substructure_session::Command,
        ) -> Result<substructure_session::CommandResult, substructure_session::Error> {
            unreachable!("a watched task settles nothing")
        }

        async fn is_open(&self, _: &str, _: &str, _: &substructure_session::WorkId) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_stops_when_its_call_ends() {
        let inbox = Closing(AtomicBool::new(true));
        let bound = bound(None, None, 0);
        let run = async {
            inbox.0.store(false, Ordering::SeqCst);
            std::future::pending::<Result<(), String>>().await
        };
        let result = watched(&inbox, bound.as_ref(), run).await;
        assert_eq!(result, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_whose_call_is_open_runs_to_the_end() {
        let inbox = Closing(AtomicBool::new(true));
        let bound = bound(None, None, 0);
        let run = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Err("finished".to_string())
        };
        let result = watched(&inbox, bound.as_ref(), run).await;
        assert_eq!(result, Err("finished".to_string()));
    }

    struct Flaky {
        fails: std::sync::atomic::AtomicU32,
        tries: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl Inbox for Flaky {
        async fn submit(
            &self,
            _tenant_id: &str,
            _session_id: &str,
            _command: substructure_session::Command,
        ) -> Result<substructure_session::CommandResult, substructure_session::Error> {
            self.tries.fetch_add(1, Ordering::SeqCst);
            if self.fails.load(Ordering::SeqCst) > 0 {
                self.fails.fetch_sub(1, Ordering::SeqCst);
                return Err(substructure_session::Error::Store(
                    substructure_session::StoreError::Unavailable("blip".into()),
                ));
            }
            Ok(Ok(substructure_session::Accepted {
                seq: 1,
                turn_id: None,
                queued: false,
            }))
        }

        async fn is_open(&self, _: &str, _: &str, _: &substructure_session::WorkId) -> bool {
            true
        }
    }

    fn settle() -> CommandPayload {
        CommandPayload::settle(
            EffectKind::ToolCall,
            "tc-1".to_string(),
            Some(0),
            SettleError::new(ErrorInfo::internal("x"), false),
        )
    }

    #[tokio::test]
    async fn a_result_is_saved_again_before_the_work_runs_again() {
        let inbox = Flaky {
            fails: 2.into(),
            tries: 0.into(),
        };
        let saved = settled(&inbox, "t", "s", settle(), SpanContext::root()).await;
        assert!(saved.is_ok());
        assert_eq!(inbox.tries.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_result_that_cannot_be_saved_is_reported() {
        let inbox = Flaky {
            fails: 100.into(),
            tries: 0.into(),
        };
        let saved = settled(&inbox, "t", "s", settle(), SpanContext::root()).await;
        assert!(saved.is_err());
        assert_eq!(inbox.tries.load(Ordering::SeqCst), SAVE_TRIES);
    }
}
