use crossterm::event::KeyCode;
use rustain::domain::models::PermissionResolution;
use serde_json::json;

use super::harness::Fixture;

#[tokio::test]
async fn k06d_standalone_operator_and_policy_resolutions_reach_the_invocation() {
    let input = json!({"task": "inspect the delegated operation"});
    let tool = "a2a__peer__skill";

    let mut once = Fixture::new(64).await;
    once.submit(vec![(tool, input.clone())]).await;
    once.wait_requested().await;
    let id = once.current_id();
    once.paint(80, 24);
    assert!(once.tools.executed.lock().is_empty());
    assert!(once.key(KeyCode::Char('y')).await.is_some());
    once.wait_closed(&id).await;
    once.finish().await;
    assert_eq!(
        *once.tools.executed.lock(),
        vec![(tool.into(), input.clone())]
    );
    assert_eq!(
        *once.tools.permission_resolutions.lock(),
        vec![Some(PermissionResolution::Operator)],
        "the standalone y decision must reach the invocation as Operator"
    );
    assert!(once.errors.is_empty(), "{:?}", once.errors);

    let mut session = Fixture::new(64).await;
    session.submit(vec![(tool, input.clone())]).await;
    session.wait_requested().await;
    let id = session.current_id();
    session.paint(80, 24);
    assert!(session.tools.executed.lock().is_empty());
    assert!(session.key(KeyCode::Char('s')).await.is_some());
    session.wait_closed(&id).await;
    session.finish().await;
    assert_eq!(
        *session.tools.executed.lock(),
        vec![(tool.into(), input.clone())]
    );
    assert_eq!(
        *session.tools.permission_resolutions.lock(),
        vec![Some(PermissionResolution::Operator)],
        "the standalone session-grant decision itself is Operator"
    );
    assert!(session.state.pending_permission.is_none());
    assert!(session.state.permission_queue.is_empty());
    let requested = session.requested.clone();

    session.submit(vec![(tool, input.clone())]).await;
    session.finish().await;
    assert_eq!(
        session.requested, requested,
        "the runtime session grant must execute the second submission without a new card"
    );
    assert!(session.state.pending_permission.is_none());
    assert!(session.state.permission_queue.is_empty());
    assert_eq!(
        *session.tools.executed.lock(),
        vec![(tool.into(), input.clone()), (tool.into(), input)],
        "each session submission must execute exactly once"
    );
    assert_eq!(
        *session.tools.permission_resolutions.lock(),
        vec![
            Some(PermissionResolution::Operator),
            Some(PermissionResolution::Policy),
        ],
        "the runtime session-grant fast path must reach the invocation as Policy"
    );
    assert!(session.errors.is_empty(), "{:?}", session.errors);
}
