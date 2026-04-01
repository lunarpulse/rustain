use rustain::domain::models::UserMessage;
use rustain::domain::services::turn_queue::TurnQueue;

#[test]
fn test_turn_queue_enqueue_dequeue() {
    use rustain::domain::models::ImageAttachment;

    let mut queue = TurnQueue::default();

    // Use image messages to prevent auto-merge
    queue
        .enqueue(UserMessage {
            content: "first".to_string(),
            images: vec![ImageAttachment {
                media_type: "image/png".to_string(),
                data: "data1".to_string(),
            }],
        })
        .unwrap();
    queue
        .enqueue(UserMessage {
            content: "second".to_string(),
            images: vec![ImageAttachment {
                media_type: "image/png".to_string(),
                data: "data2".to_string(),
            }],
        })
        .unwrap();

    assert_eq!(queue.len(), 2);

    let msg1 = queue.dequeue().unwrap();
    assert_eq!(msg1.content, "first");

    let msg2 = queue.dequeue().unwrap();
    assert_eq!(msg2.content, "second");

    assert!(queue.dequeue().is_none());
    assert!(queue.is_empty());
}

#[test]
fn test_turn_queue_merge_text_only() {
    let mut queue = TurnQueue::default();

    queue
        .enqueue(UserMessage {
            content: "hello".to_string(),
            images: vec![],
        })
        .unwrap();
    queue
        .enqueue(UserMessage {
            content: "world".to_string(),
            images: vec![],
        })
        .unwrap();

    // Two text-only messages should merge into one
    assert_eq!(queue.len(), 1);

    let msg = queue.dequeue().unwrap();
    assert_eq!(msg.content, "hello\nworld");
}

#[test]
fn test_turn_queue_no_merge_with_images() {
    use rustain::domain::models::ImageAttachment;

    let mut queue = TurnQueue::default();

    queue
        .enqueue(UserMessage {
            content: "text".to_string(),
            images: vec![],
        })
        .unwrap();
    queue
        .enqueue(UserMessage {
            content: "with image".to_string(),
            images: vec![ImageAttachment {
                media_type: "image/png".to_string(),
                data: "base64data".to_string(),
            }],
        })
        .unwrap();

    // Image message should not merge
    assert_eq!(queue.len(), 2);
}

#[test]
fn test_turn_queue_full_returns_error() {
    let mut queue = TurnQueue::default();

    // Fill queue with 8 image messages (can't merge)
    for i in 0..8 {
        queue
            .enqueue(UserMessage {
                content: format!("msg {}", i),
                images: vec![rustain::domain::models::ImageAttachment {
                    media_type: "image/png".to_string(),
                    data: "data".to_string(),
                }],
            })
            .unwrap();
    }

    assert_eq!(queue.len(), 8);

    // 9th message should fail
    let result = queue.enqueue(UserMessage {
        content: "overflow".to_string(),
        images: vec![],
    });
    assert!(result.is_err());
}

#[test]
fn test_turn_queue_merge_respects_threshold() {
    let mut queue = TurnQueue::default();

    // Create a message close to the 12,000 char threshold
    let long_content = "x".repeat(11_999);
    queue
        .enqueue(UserMessage {
            content: long_content,
            images: vec![],
        })
        .unwrap();

    // This message would exceed the merge threshold
    queue
        .enqueue(UserMessage {
            content: "more text".to_string(),
            images: vec![],
        })
        .unwrap();

    // Should NOT merge since combined length exceeds threshold
    assert_eq!(queue.len(), 2);
}
