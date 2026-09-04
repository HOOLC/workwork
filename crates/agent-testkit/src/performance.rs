use std::time::{Duration, Instant};

use serde_json::json;
use zork_agent::session::events::{SessionEvent, TurnOutcome};
use zork_agent::session::service::LiveSessionEvent;
use zork_agent::session::wire::SessionSelection;

use crate::TestWorld;

#[derive(Clone, Copy, Debug)]
pub struct VirtualPerformance {
    pub session_count: usize,
    pub event_count: usize,
    pub elapsed: Duration,
}

impl VirtualPerformance {
    pub fn sessions_per_second(self) -> f64 {
        self.session_count as f64 / self.elapsed.as_secs_f64()
    }
}

pub async fn measure_virtual_sessions(session_count: usize) -> VirtualPerformance {
    let mut world = TestWorld::new();
    let selection = SessionSelection {
        profile_id: "performance-profile".into(),
        model: "performance-model".into(),
        thinking: "medium".into(),
    };
    let started = Instant::now();
    let mut event_count = 0usize;

    for index in 0..session_count {
        let session_id = world
            .create_session(
                selection.clone(),
                None,
                format!("/virtual/workspace/{index}"),
            )
            .await
            .expect("create performance session");
        world
            .send_mail(&session_id, format!("performance input {index}"))
            .await
            .expect("append performance mailbox input");
        let request = world.request().await;
        let mut events = world
            .subscribe(&session_id)
            .expect("subscribe to performance session");
        request
            .respond_call(format!("provider-call-{index}"), "end", json!({}))
            .expect("complete performance provider request");
        loop {
            let event = events.recv().await.expect("performance event stream");
            if matches!(
                event,
                LiveSessionEvent::Durable(envelope)
                    if matches!(
                        envelope.event,
                        SessionEvent::TurnFinished {
                            outcome: TurnOutcome::Finished,
                            ..
                        }
                    )
            ) {
                break;
            }
        }
        event_count += world.events(&session_id).len();
    }

    let elapsed = started.elapsed();
    world.shutdown().await;
    VirtualPerformance {
        session_count,
        event_count,
        elapsed,
    }
}
