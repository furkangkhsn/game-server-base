//! The economy service's explicit stop (BACKLOG F5): everything queued
//! before the stop is answered, the task ends only once those answers are
//! out, and a request after the stop is refused.

use std::time::Duration;

use super::*;

/// Long enough that "the task ended before its answers were out" and
/// "the answer was already out" are far apart.
const LATENCY: Duration = Duration::from_millis(80);
const WAIT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn requests_queued_before_the_stop_are_answered_before_the_task_ends() {
    let (economy, service) = EconomyService::start(LATENCY);
    let answers: Vec<_> = ["potion", "sword", "nope"]
        .into_iter()
        .map(|kind| {
            let economy = economy.clone();
            tokio::spawn(async move { economy.buy(kind.into()).await })
        })
        .collect();
    // Let every request reach the mailbox before the stop is posted.
    tokio::time::sleep(Duration::from_millis(10)).await;
    tokio::time::timeout(WAIT, service.request_stop())
        .await
        .expect("the economy ends on its stop request")
        .expect("worker panicked");
    let mut got = Vec::new();
    for answer in answers {
        // Already out: the task waited for its in-flight answers.
        let answer = tokio::time::timeout(LATENCY / 4, answer)
            .await
            .expect("the task ended before this answer was sent");
        got.push(answer.expect("buyer panicked"));
    }
    assert_eq!(
        got,
        vec![Ok(100), Ok(500), Err("unknown item `nope`".to_string())]
    );
}

#[tokio::test]
async fn a_request_after_the_stop_is_refused() {
    let (economy, service) = EconomyService::start(Duration::ZERO);
    tokio::time::timeout(WAIT, service.request_stop())
        .await
        .expect("the economy ends on its stop request")
        .expect("worker panicked");
    assert_eq!(
        economy.buy("potion".into()).await,
        Err("economy service gone".to_string())
    );
}

#[tokio::test]
async fn a_spawned_economy_is_not_stopped_by_its_dropped_service() {
    let economy = EconomyService::spawn(Duration::ZERO);
    assert_eq!(economy.buy("shield".into()).await, Ok(300));
}
