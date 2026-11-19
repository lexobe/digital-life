use serde_json::json;
use std::sync::{Arc, Barrier};
use survive_core::{AgentState, MessageRole, Store};

fn birth(store: &Store) -> String {
    let environment = store
        .create_environment(
            "creator",
            serde_json::from_value(json!({"description":"Older environment."})).unwrap(),
        )
        .unwrap();
    store
        .birth_organisms(
            "creator",
            &environment.id,
            serde_json::from_value(
                json!({"count":1, "defaults":{"initial_energy":100, "lifetime_seconds":60}}),
            )
            .unwrap(),
        )
        .unwrap()
        .remove(0)
        .id
}

#[test]
fn preexisting_organism_queues_messages_while_stopped_and_keeps_them_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("platform.redb");
    let original = Store::open(&path).unwrap();
    let id = birth(&original);
    original
        .set_agent_state(&id, AgentState::Suspended, None)
        .unwrap();
    let before = original.get_agent(&id).unwrap();
    let life = original.life_state(&id).unwrap();
    drop(original);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.messages(&id, None, 50, false).unwrap().next_after, 0);
    for i in 1..=6 {
        let message = store
            .send_creator_message("creator", &id, format!("message {i}"), &i.to_string())
            .unwrap();
        assert_eq!(message.seq, i);
    }
    let response = store.reply_to_message(&id, 2, "hello".into(), "2").unwrap();
    assert_eq!(
        (response.role, response.seq, response.reply_to),
        (MessageRole::Organism, 7, Some(2))
    );
    drop(store);

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.reply_to_message(&id, 2, "hello".into(), "2").unwrap(),
        response
    );
    assert!(
        store
            .reply_to_message(&id, 1, "hello".into(), "2")
            .unwrap_err()
            .to_string()
            .contains("conflict")
    );
    let (waiting, recent) = store.message_context(&id).unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].seq, 1);
    assert_eq!(
        recent.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [4, 5, 6, 7]
    );
    let tail = store.messages(&id, None, 2, false).unwrap();
    assert_eq!(
        tail.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [6, 7]
    );
    assert_eq!((tail.next_after, tail.has_more), (7, false));
    let first = store.messages(&id, Some(0), 2, false).unwrap();
    assert_eq!(
        first.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!((first.next_after, first.has_more), (2, true));
    let pending = store.messages(&id, None, 2, true).unwrap();
    assert_eq!(
        pending.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [1, 3]
    );
    assert_eq!((pending.next_after, pending.has_more), (3, true));
    let rest = store
        .messages(&id, Some(pending.next_after), 10, true)
        .unwrap();
    assert_eq!(
        rest.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [4, 5, 6]
    );
    assert!(!rest.has_more);
    for pending_only in [false, true] {
        let empty = store
            .messages(&id, Some(u64::MAX), 100, pending_only)
            .unwrap();
        assert!(empty.messages.is_empty());
        assert_eq!((empty.next_after, empty.has_more), (u64::MAX, false));
    }
    let after = store.get_agent(&id).unwrap();
    assert_eq!(
        (after.state, after.generation, after.granted, after.used),
        (before.state, before.generation, before.granted, before.used)
    );
    assert_eq!(
        life.remaining_lifetime_ms,
        store.life_state(&id).unwrap().remaining_lifetime_ms
    );
    assert!(store.reservations(&id).unwrap().is_empty());
    assert!(store.grants(&id).unwrap().is_empty());
}

#[test]
fn parallel_retries_post_once_and_parallel_writers_can_only_answer_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("platform.redb")).unwrap();
    let id = birth(&store);
    let run = |reply| {
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|i| {
                    let (barrier, store, id) = (Arc::clone(&barrier), &store, &id);
                    scope.spawn(move || {
                        barrier.wait();
                        if reply {
                            store.reply_to_message(
                                id,
                                1,
                                format!("reply {i}"),
                                &format!("reply-{i}"),
                            )
                        } else {
                            store.send_creator_message("creator", id, "hello".into(), "same")
                        }
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .collect::<Vec<_>>()
        })
    };
    assert!(run(false).into_iter().all(|r| r.unwrap().seq == 1));
    let replies = run(true);
    assert_eq!(replies.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        replies
            .into_iter()
            .filter_map(Result::err)
            .all(|e| e.to_string().contains("reply_conflict"))
    );
    assert_eq!(
        store
            .messages(&id, Some(0), 100, false)
            .unwrap()
            .messages
            .len(),
        2
    );
    assert!(
        store
            .messages(&id, None, 1, true)
            .unwrap()
            .messages
            .is_empty()
    );
}
