use super::super::ipc::{self, UploadAction, UploadSession};
use super::super::jobs::{JobLedger, JobState, LedgerLimits};
use super::reconcile::DownloadProof;
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use sha2::Digest;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, UnixListener};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn event() -> Event {
    EventBuilder::new(Kind::TextNote, "fixture")
        .custom_created_at(Timestamp::from(150))
        .sign_with_keys(&Keys::generate())
        .unwrap()
}

fn job(dir: &Path, relay: &str) -> (JobLedger, super::super::jobs::Lease) {
    let mut ledger = JobLedger::open(&dir.join("jobs"), LedgerLimits::default()).unwrap();
    ledger.enqueue("fixture", relay, 100, 200).unwrap();
    let lease = ledger.lease_next(now(), 600).unwrap().unwrap();
    (ledger, lease)
}

fn uid() -> u32 {
    UnixStream::pair().unwrap().0.peer_cred().unwrap().uid()
}

async fn send_inventory(stream: &mut UnixStream, lease: &super::super::jobs::Lease, relay: &str) {
    let hello: Hello = read_message(stream).await.unwrap();
    assert_eq!(hello.version, VERSION);
    let mut assignment = Assignment::for_lease(lease);
    // Test-only parent fixture bypasses the production ledger's private-IP filter.
    assignment.relay = RelayUrl::parse(relay).unwrap().to_string();
    let header = assignment.header(1);
    write_message(stream, &ParentMessage::Job { assignment })
        .await
        .unwrap();
    write_message(
        stream,
        &ParentMessage::InventoryEnd {
            header,
            count: 0,
            digest: inventory_hasher().finalize().into(),
        },
    )
    .await
    .unwrap();
}

// Real pinned SDK speaks NIP-77 against this localhost-only relay. In partial
// mode the relay advertises every ID but withholds the last event before EOSE.
async fn relay(partial: bool, hang: bool) -> (String, tokio::task::JoinHandle<()>) {
    relay_with_empty(partial, hang, false).await
}

async fn relay_with_empty(
    partial: bool,
    hang: bool,
    empty: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let events = if empty { Vec::new() } else { vec![event()] };
    relay_events(partial, hang, events, Arc::new(RelayStats::default())).await
}

#[derive(Default)]
struct RelayStats {
    inject_unrequested: bool,
    compare_delay: Duration,
    chatter: bool,
    continuations: std::sync::atomic::AtomicUsize,
    batches: std::sync::atomic::AtomicUsize,
    max_reply: std::sync::atomic::AtomicUsize,
    /// Results per REQ before EOSE, like a relay's per-subscription limit.
    cap: Option<usize>,
    /// Number of IDs named by each REQ, in order.
    requested: std::sync::Mutex<Vec<usize>>,
    /// How the relay answers every REQ after the first (the worker's follow-ups).
    follow_up: FollowUpFault,
}

/// A fault in the relay's answer to a follow-up REQ.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum FollowUpFault {
    /// Answer like the first REQ.
    #[default]
    None,
    /// `CLOSED` on the follow-up's subscription instead of events and EOSE.
    Closed,
    /// A valid, in-window event the follow-up did not name, then the usual answer.
    Unrequested,
}

async fn relay_events(
    partial: bool,
    hang: bool,
    events: Vec<Event>,
    stats: Arc<RelayStats>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let mut storage = negentropy::NegentropyStorageVector::new();
        for event in &events {
            storage
                .insert(150, negentropy::Id::from_byte_array(event.id.to_bytes()))
                .unwrap();
        }
        storage.seal().unwrap();
        let mut engine = negentropy::Negentropy::borrowed(&storage, 1024 * 1024).unwrap();
        let unrequested = event();
        while let Some(Ok(message)) = socket.next().await {
            if !message.is_text() {
                continue;
            }
            let request: serde_json::Value =
                serde_json::from_str(message.to_text().unwrap()).unwrap();
            if stats.chatter && request[0] == "NEG-OPEN" {
                for _ in 0..40 {
                    if socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            serde_json::json!(["NEG-MSG", "unrelated", "00"])
                                .to_string()
                                .into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            if stats.inject_unrequested && matches!(request[0].as_str(), Some("NEG-OPEN" | "REQ")) {
                // Valid, in-window, but not solicited: unknown subscription during
                // diff/fetch, and a wrong ID on the actual fetch subscription.
                for subscription in [serde_json::json!("junk"), request[1].clone()] {
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            serde_json::json!(["EVENT", subscription, unrequested])
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
            let response = match request[0].as_str().unwrap() {
                "NEG-OPEN" | "NEG-MSG" if !hang => {
                    tokio::time::sleep(stats.compare_delay).await;
                    if request[0] == "NEG-MSG" {
                        stats
                            .continuations
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let index = if request[0] == "NEG-OPEN" { 3 } else { 2 };
                    let query = hex::decode(request[index].as_str().unwrap()).unwrap();
                    let reply = engine.reconcile(&query).unwrap();
                    stats
                        .max_reply
                        .fetch_max(reply.len() * 2, std::sync::atomic::Ordering::Relaxed);
                    serde_json::json!(["NEG-MSG", request[1], hex::encode(reply)])
                }
                "REQ" if !hang => {
                    let follow_up = stats
                        .batches
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        > 0;
                    let ids = request[2]["ids"].as_array().unwrap();
                    stats.requested.lock().unwrap().push(ids.len());
                    if follow_up && stats.follow_up == FollowUpFault::Closed {
                        let closed = serde_json::json!(["CLOSED", request[1], "error: test"]);
                        let _ = socket
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                closed.to_string().into(),
                            ))
                            .await;
                        continue;
                    }
                    if follow_up && stats.follow_up == FollowUpFault::Unrequested {
                        let stray = serde_json::json!(["EVENT", request[1], unrequested]);
                        socket
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                stray.to_string().into(),
                            ))
                            .await
                            .unwrap();
                    }
                    let mut sent = 0;
                    for event in events
                        .iter()
                        .take(events.len().saturating_sub(usize::from(partial)))
                    {
                        if !request[2]["ids"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|id| id.as_str() == Some(&event.id.to_hex()))
                        {
                            continue;
                        }
                        if stats.cap.is_some_and(|cap| sent == cap) {
                            break;
                        }
                        sent += 1;
                        let response = serde_json::json!(["EVENT", request[1], event]);
                        socket
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                response.to_string().into(),
                            ))
                            .await
                            .unwrap();
                    }
                    serde_json::json!(["EOSE", request[1]])
                }
                _ => continue,
            };
            if socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    response.to_string().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    (url, task)
}

#[tokio::test]
async fn slow_comparison_can_complete_empty_without_event_admission() {
    let stats = Arc::new(RelayStats {
        compare_delay: Duration::from_millis(400),
        ..Default::default()
    });
    let (url, server) = relay_events(false, false, vec![], stats).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("ipc");
    let listener = UnixListener::bind(&socket).unwrap();
    let (mut ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let parent = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        send_inventory(&mut stream, &lease, &url).await;
        let wire = transport::read_wire(&mut stream).await.unwrap();
        let mut session = UploadSession::new(lease.clone());
        assert!(matches!(
            session
                .receive(&mut ledger, Frame::read(&mut &wire[..]).unwrap(), now())
                .unwrap(),
            UploadAction::ProtocolDone
        ));
    };
    let (result, ()) = tokio::join!(
        run_with_budgets(
            &socket,
            uid(),
            Duration::from_secs(3),
            Duration::from_millis(200),
            Duration::from_secs(2)
        ),
        parent
    );
    assert!(result.is_ok(), "{result:?}");
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn overall_deadline_still_bounds_long_comparison_allowance() {
    let (url, server) = relay(false, true).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("ipc");
    let listener = UnixListener::bind(&socket).unwrap();
    let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let parent = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        send_inventory(&mut stream, &lease, &url).await;
        assert!(transport::read_wire(&mut stream).await.is_err());
    };
    let (result, ()) = timeout(Duration::from_secs(2), async {
        tokio::join!(
            run_with_budgets(
                &socket,
                uid(),
                Duration::from_millis(400),
                Duration::from_secs(1),
                Duration::from_secs(30)
            ),
            parent
        )
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(WorkerError::Deadline)));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn unrelated_protocol_chatter_does_not_extend_comparison_deadline() {
    let (url, server) = relay_events(
        false,
        true,
        vec![],
        Arc::new(RelayStats {
            chatter: true,
            ..Default::default()
        }),
    )
    .await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("ipc");
    let listener = UnixListener::bind(&socket).unwrap();
    let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let parent = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        send_inventory(&mut stream, &lease, &url).await;
        assert!(transport::read_wire(&mut stream).await.is_err());
    };
    let (result, ()) = timeout(Duration::from_secs(2), async {
        tokio::join!(
            run_with_budgets(
                &socket,
                uid(),
                Duration::from_secs(5),
                Duration::from_secs(1),
                Duration::from_millis(150)
            ),
            parent
        )
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(WorkerError::Deadline)));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn stalled_handshake_reports_relay_failure_before_connect_backstop() {
    let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", relay_listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (_socket, _) = relay_listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("ipc");
    let listener = UnixListener::bind(&socket).unwrap();
    let (mut ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let parent = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        send_inventory(&mut stream, &lease, &url).await;
        let wire = transport::read_wire(&mut stream).await.unwrap();
        let mut session = UploadSession::new(lease.clone());
        assert!(matches!(
            session
                .receive(&mut ledger, Frame::read(&mut &wire[..]).unwrap(), now())
                .unwrap(),
            UploadAction::Failed(FailureKind::Relay)
        ));
    };
    let (result, ()) = timeout(Duration::from_secs(28), async {
        tokio::join!(
            run_with_budgets(
                &socket,
                uid(),
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::from_secs(60)
            ),
            parent
        )
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(WorkerError::Incomplete)));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn closed_diff_fails_without_waiting_for_the_idle_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if !message.is_text() {
                continue;
            }
            let request: serde_json::Value =
                serde_json::from_str(message.to_text().unwrap()).unwrap();
            if request[0] == "NEG-OPEN" {
                socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        serde_json::json!(["CLOSED", request[1], "blocked"])
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let client = Client::default();
    client.add_relay(&url).await.unwrap();
    let relay = client.relay(&url).await.unwrap();
    relay.try_connect(Duration::from_secs(2)).await.unwrap();
    let result = timeout(
        Duration::from_secs(2),
        reconcile::download(&relay, &Assignment::for_lease(&lease), vec![]),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(WorkerError::Incomplete)));
    client.disconnect().await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn overlapping_multiround_and_large_reply_multibatch_downloads() {
    use std::sync::atomic::Ordering;
    for (total, overlap) in [(2000, 1000), (2400, 0)] {
        let keys = Keys::generate();
        let events: Vec<_> = (0..total)
            .map(|i| {
                let tags = if i == total - 1 {
                    (0..2001)
                        .map(|n| Tag::parse(["t".to_owned(), n.to_string()]).unwrap())
                        .collect()
                } else {
                    Vec::new()
                };
                EventBuilder::new(Kind::TextNote, i.to_string())
                    .tags(tags)
                    .custom_created_at(Timestamp::from(150))
                    .sign_with_keys(&keys)
                    .unwrap()
            })
            .collect();
        let expected: std::collections::HashSet<_> =
            events[overlap..].iter().map(|e| e.id).collect();
        let items = events[..overlap]
            .iter()
            .map(|e| (e.id, e.created_at))
            .collect();
        let stats = Arc::new(RelayStats::default());
        let (url, server) = relay_events(false, false, events, stats.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
        let assignment = Assignment::for_lease(&lease);
        let (capture, mut receiver) = Capture::new(&assignment);
        let capture = Arc::new(capture);
        let client = Client::builder()
            .opts(ClientOptions::default().relay_limits(relay_limits()))
            .database(capture.clone())
            .build();
        client.add_relay(&url).await.unwrap();
        let relay = client.relay(&url).await.unwrap();
        relay.try_connect(Duration::from_secs(2)).await.unwrap();
        let drain = tokio::spawn(async move {
            let mut count = 0;
            while let Some(frame) = receiver.recv().await {
                count += 1;
                drop(frame);
            }
            count
        });
        let proof = timeout(
            Duration::from_secs(30),
            reconcile::download(&relay, &assignment, items),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(proof.remote, expected);
        client.disconnect().await;
        assert_eq!(
            capture.finish_download(Ok(proof)).await.unwrap().0 as usize,
            total - overlap
        );
        assert_eq!(drain.await.unwrap(), total - overlap);
        server.abort();
        let _ = server.await;
        assert_eq!(
            stats.batches.load(Ordering::Relaxed),
            (total - overlap).div_ceil(128)
        );
        if overlap > 0 {
            assert!(stats.continuations.load(Ordering::Relaxed) > 0);
        } else {
            assert!(stats.max_reply.load(Ordering::Relaxed) > 120_000);
        }
    }
}

/// Run one download against a mock relay that caps each REQ at `cap` results
/// and, with `withhold`, never delivers the last event. Returns the outcome,
/// the IDs named by each REQ and the frames captured for upload.
async fn capped_download(
    cap: Option<usize>,
    withhold: bool,
    events: Vec<Event>,
) -> (Result<DownloadProof, WorkerError>, Vec<usize>, usize) {
    let stats = RelayStats {
        cap,
        ..RelayStats::default()
    };
    download_from(stats, withhold, events).await
}

/// [`capped_download`] against a mock relay configured by `stats`.
async fn download_from(
    stats: RelayStats,
    withhold: bool,
    events: Vec<Event>,
) -> (Result<DownloadProof, WorkerError>, Vec<usize>, usize) {
    let stats = Arc::new(stats);
    let (url, server) = relay_events(withhold, false, events, stats.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let assignment = Assignment::for_lease(&lease);
    let (capture, mut receiver) = Capture::new(&assignment);
    let capture = Arc::new(capture);
    let client = Client::builder()
        .opts(ClientOptions::default().relay_limits(relay_limits()))
        .database(capture.clone())
        .build();
    client.add_relay(&url).await.unwrap();
    let relay = client.relay(&url).await.unwrap();
    relay.try_connect(Duration::from_secs(2)).await.unwrap();
    let drain = tokio::spawn(async move {
        let mut count = 0;
        while receiver.recv().await.is_some() {
            count += 1;
        }
        count
    });
    let result = timeout(
        Duration::from_secs(10),
        reconcile::download(&relay, &assignment, vec![]),
    )
    .await
    .unwrap();
    client.disconnect().await;
    let result = match result {
        Ok(proof) => {
            let remote = proof.remote.clone();
            let received = proof.received.clone();
            capture.finish_download(Ok(proof)).await.unwrap();
            Ok(DownloadProof { remote, received })
        }
        Err(error) => {
            assert!(
                capture
                    .finish_download(Err(WorkerError::Incomplete))
                    .await
                    .is_err()
            );
            Err(error)
        }
    };
    let captured = drain.await.unwrap();
    server.abort();
    let _ = server.await;
    let requested = stats.requested.lock().unwrap().clone();
    (result, requested, captured)
}

#[tokio::test]
async fn relay_result_cap_is_followed_up_not_reported_unavailable() {
    let keys = Keys::generate();
    let events: Vec<_> = (0..10)
        .map(|i| {
            EventBuilder::new(Kind::TextNote, i.to_string())
                .custom_created_at(Timestamp::from(150))
                .sign_with_keys(&keys)
                .unwrap()
        })
        .collect();
    // A relay that answers at most three events per REQ, then EOSE: each
    // follow-up names only the still-missing IDs until all ten arrive.
    let (result, requested, captured) = capped_download(Some(3), false, events.clone()).await;
    let proof = result.unwrap();
    assert_eq!(proof.received.len(), 10);
    assert_eq!(proof.remote, proof.received);
    assert_eq!(requested, vec![10, 7, 4, 1]);
    assert_eq!(captured, 10);
    // The same capped relay also withholding one event: only a REQ naming
    // exactly the remaining ID, which returns nothing, makes it unavailable.
    let withheld = events.last().unwrap().id;
    let (result, requested, captured) = capped_download(Some(3), true, events.clone()).await;
    let Err(WorkerError::Unavailable(report)) = result else {
        panic!("expected unavailable");
    };
    assert_eq!(report.missing_count, 1);
    assert_eq!(report.sample, vec![withheld.to_bytes()]);
    assert_eq!(requested, vec![10, 7, 4, 1]);
    assert_eq!(captured, 9);
    // Uncapped withholding: one follow-up for the missing ID, then unavailable.
    let (result, requested, _) = capped_download(None, true, events).await;
    let Err(WorkerError::Unavailable(report)) = result else {
        panic!("expected unavailable");
    };
    assert_eq!(report.sample, vec![withheld.to_bytes()]);
    assert_eq!(requested, vec![10, 1]);
}

fn signed_events(count: usize) -> Vec<Event> {
    let keys = Keys::generate();
    (0..count)
        .map(|i| {
            EventBuilder::new(Kind::TextNote, i.to_string())
                .custom_created_at(Timestamp::from(150))
                .sign_with_keys(&keys)
                .unwrap()
        })
        .collect()
}

#[tokio::test]
async fn faulty_follow_up_answers_fail_closed_not_unavailable() {
    // The first REQ is capped at three results, so a follow-up is sent. A relay
    // that closes the follow-up, or answers it with an event it did not name,
    // makes the attempt incomplete: neither is evidence of withholding.
    for fault in [FollowUpFault::Closed, FollowUpFault::Unrequested] {
        let stats = RelayStats {
            cap: Some(3),
            follow_up: fault,
            ..RelayStats::default()
        };
        let (result, requested, _) = download_from(stats, false, signed_events(10)).await;
        assert!(
            matches!(result, Err(WorkerError::Incomplete)),
            "{:?}",
            result.map(|proof| proof.received.len())
        );
        assert_eq!(requested, vec![10, 7]);
    }
}

#[tokio::test]
async fn result_cap_is_followed_up_within_each_batch_of_a_multi_batch_diff() {
    // 130 IDs are two fetch batches (128 + 2). A relay capping each REQ at 100
    // gets one follow-up for the first batch's 28-ID tail, then the second
    // batch; each batch stays within n + 1 REQs.
    let (result, requested, captured) = capped_download(Some(100), false, signed_events(130)).await;
    let proof = result.unwrap();
    assert_eq!(proof.received.len(), 130);
    assert_eq!(proof.remote, proof.received);
    assert_eq!(requested, vec![128, 28, 2]);
    assert_eq!(captured, 130);
    // Capped at 50: two follow-ups for the first batch, none crossing into
    // the second.
    let (result, requested, _) = capped_download(Some(50), false, signed_events(130)).await;
    assert_eq!(result.unwrap().received.len(), 130);
    assert_eq!(requested, vec![128, 78, 28, 2]);
}

#[tokio::test]
async fn expired_advertised_event_is_unavailable_not_success() {
    let expired = EventBuilder::new(Kind::TextNote, "expired")
        .tags([Tag::expiration(Timestamp::from(151))])
        .custom_created_at(Timestamp::from(150))
        .sign_with_keys(&Keys::generate())
        .unwrap();
    let (url, server) =
        relay_events(false, false, vec![expired], Arc::new(RelayStats::default())).await;
    let dir = tempfile::tempdir().unwrap();
    let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let assignment = Assignment::for_lease(&lease);
    let (capture, mut receiver) = Capture::new(&assignment);
    let capture = Arc::new(capture);
    let client = Client::builder()
        .opts(ClientOptions::default().relay_limits(relay_limits()))
        .database(capture.clone())
        .build();
    client.add_relay(&url).await.unwrap();
    let relay = client.relay(&url).await.unwrap();
    relay.try_connect(Duration::from_secs(2)).await.unwrap();
    let result = timeout(
        Duration::from_secs(3),
        reconcile::download(&relay, &assignment, vec![]),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(WorkerError::Unavailable(_))));
    client.disconnect().await;
    assert!(
        capture
            .finish_download(Err(WorkerError::Unavailable(
                crate::sync::failure::FailureDiagnostic {
                    kind: crate::sync::failure::FailureKind::Unavailable,
                    missing_count: 0,
                    sample: Vec::new()
                }
            )))
            .await
            .is_err()
    );
    assert!(receiver.recv().await.is_none());
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn explicit_empty_diff_can_complete_without_inventing_events() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("ipc");
    let listener = UnixListener::bind(&socket).unwrap();
    let (url, server) = relay_with_empty(false, false, true).await;
    let (mut ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let parent = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        send_inventory(&mut stream, &lease, &url).await;
        let wire = transport::read_wire(&mut stream).await.unwrap();
        let mut session = UploadSession::new(lease.clone());
        assert!(matches!(
            session
                .receive(&mut ledger, Frame::read(&mut &wire[..]).unwrap(), now())
                .unwrap(),
            UploadAction::ProtocolDone
        ));
        let progress = ledger
            .attempt_progress(lease.job().id, lease.job().attempt)
            .unwrap();
        assert!(progress.protocol_done);
        assert_eq!(progress.received, 0);
    };
    let (outcome, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(
            run_with_limits(
                &socket,
                uid(),
                Duration::from_secs(3),
                Duration::from_secs(2)
            ),
            parent
        )
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn real_sdk_rejects_unsolicited_events_and_requires_archive_admission() {
    for (partial, delivered) in [(false, 1), (true, 0), (true, 1)] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("ipc");
        let listener = UnixListener::bind(&socket).unwrap();
        let events: Vec<_> = (0..delivered + usize::from(partial))
            .map(|_| event())
            .collect();
        let withheld = events.last().unwrap().id.to_bytes();
        let (url, server) = relay_events(
            partial,
            false,
            events,
            Arc::new(RelayStats {
                inject_unrequested: true,
                ..Default::default()
            }),
        )
        .await;
        let (mut ledger, lease) = job(dir.path(), "wss://relay.example.com");
        let id = lease.job().id;
        let parent = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            send_inventory(&mut stream, &lease, &url).await;
            let dedupe = Arc::new(crate::DedupeIndex::open(dir.path().join("dedupe")).unwrap());
            let writer = crate::SegmentWriter::new(
                crate::SegmentConfig {
                    output_dir: dir.path().join("archive"),
                    compress: false,
                    ..Default::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap();
            let mut session = UploadSession::new(lease.clone());
            let mut count = 0;
            loop {
                let wire = transport::read_wire(&mut stream).await.unwrap();
                let frame = Frame::read(&mut &wire[..]).unwrap();
                match session.receive(&mut ledger, frame, now()).unwrap() {
                    UploadAction::Event(event) => {
                        count += 1;
                        let ack = session
                            .admit_and_accept(&mut ledger, event, &dedupe, &writer, now)
                            .unwrap();
                        write_message(&mut stream, &ParentMessage::accepted(ack))
                            .await
                            .unwrap();
                    }
                    UploadAction::ProtocolDone => {
                        assert!(!partial);
                        assert_eq!(count, delivered);
                        assert_eq!(ledger.get(id).unwrap().state, JobState::AwaitingDurability);
                        assert!(
                            !ledger
                                .reconcile_archived(id, &dedupe, &writer, 16)
                                .unwrap()
                                .complete
                        );
                        writer.seal().unwrap();
                        assert!(
                            ledger
                                .reconcile_archived(id, &dedupe, &writer, 16)
                                .unwrap()
                                .complete
                        );
                        break;
                    }
                    UploadAction::Failed(FailureKind::Unavailable) if partial => {
                        let report = ledger
                            .failure_report(id, lease.job().attempt)
                            .unwrap()
                            .unwrap();
                        assert_eq!(report.missing_count, 1);
                        assert_eq!(report.sample, vec![withheld]);
                        assert_eq!(count, delivered);
                        break;
                    }
                    _ => panic!("unexpected failure frame"),
                }
            }
        };
        let outcome = timeout(Duration::from_secs(8), async {
            let (result, ()) = tokio::join!(
                run_with_limits(
                    &socket,
                    uid(),
                    Duration::from_secs(6),
                    Duration::from_secs(3)
                ),
                parent
            );
            result
        })
        .await
        .unwrap();
        server.abort();
        let _ = server.await;
        if partial {
            assert!(matches!(outcome, Err(WorkerError::Unavailable(_))));
            assert_eq!(ledger.get(id).unwrap().state, JobState::Leased);
            assert_eq!(
                ledger.attempt_progress(id, 1).unwrap().received,
                delivered as u64
            );
            let report = ledger.failure_report(id, 1).unwrap().unwrap();
            assert_eq!(report.kind, FailureKind::Unavailable);
            assert_eq!(report.missing_count, 1);
            assert_eq!(report.sample, vec![withheld]);
        } else {
            assert!(outcome.is_ok(), "{outcome:?}");
        }
    }
}

#[tokio::test]
async fn failed_diagnostic_delivery_preserves_the_original_exit_cause() {
    let dir = tempfile::tempdir().unwrap();
    let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
    let assignment = Assignment::for_lease(&lease);
    for expected in [2, 3, 4] {
        let (mut stream, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let error = match expected {
            2 => WorkerError::Unavailable(crate::sync::failure::FailureDiagnostic {
                kind: FailureKind::Unavailable,
                missing_count: 1,
                sample: vec![[1; 32]],
            }),
            3 => WorkerError::Volume,
            _ => WorkerError::EventSize,
        };
        let error = report_failure(
            &mut stream,
            &assignment,
            0,
            Instant::now() + Duration::from_secs(1),
            error,
        )
        .await;
        assert_eq!(error.exit_code(), expected);
    }
}

#[tokio::test]
async fn relay_hang_and_parent_disconnect_terminate_without_protocol_done() {
    for disconnect in [false, true] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path().join("ipc");
        let listener = UnixListener::bind(&path).unwrap();
        let (url, server) = relay(false, true).await;
        let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
        let parent = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            send_inventory(&mut stream, &lease, &url).await;
            if !disconnect {
                assert!(transport::read_wire(&mut stream).await.is_err());
            }
        };
        let (result, ()) = timeout(Duration::from_secs(3), async {
            tokio::join!(
                run_with_limits(
                    &path,
                    uid(),
                    Duration::from_secs(2),
                    Duration::from_millis(250)
                ),
                parent
            )
        })
        .await
        .unwrap();
        assert!(result.is_err());
        if !disconnect {
            assert!(matches!(result, Err(WorkerError::Deadline)));
        }
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn parent_uid_is_checked_before_hello() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let path = dir.path().join("ipc");
    let listener = UnixListener::bind(&path).unwrap();
    let parent = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(transport::read_wire(&mut stream).await.is_err());
    };
    let (result, ()) = tokio::join!(
        run_with_limits(
            &path,
            uid() + 1,
            Duration::from_millis(150),
            Duration::from_secs(1)
        ),
        parent
    );
    assert!(matches!(result, Err(WorkerError::Peer)));
}

#[tokio::test]
async fn idle_wait_outlives_job_budgets_and_cancellation_closes_socket() {
    for partial in [false, true] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path().join("ipc");
        let listener = UnixListener::bind(&path).unwrap();
        let mut worker = tokio::spawn(async move {
            run_with_limits(
                &path,
                uid(),
                Duration::from_millis(10),
                Duration::from_millis(10),
            )
            .await
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let _: Hello = read_message(&mut stream).await.unwrap();
        if partial {
            stream.write_all(&[0]).await.unwrap();
        }
        assert!(
            timeout(Duration::from_millis(40), &mut worker)
                .await
                .is_err()
        );
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        // Dropping the idle future owns socket cleanup, not an orphan task.
        assert!(
            timeout(Duration::from_secs(1), transport::read_wire(&mut stream))
                .await
                .unwrap()
                .is_err()
        );
    }
}

#[tokio::test]
async fn partial_assignment_is_bounded_but_absent_assignment_is_idle() {
    let (mut worker, mut parent) = UnixStream::pair().unwrap();
    parent.write_all(&[0]).await.unwrap();
    assert!(matches!(
        wait_assignment(&mut worker, Duration::from_millis(20)).await,
        Err(WorkerError::Deadline)
    ));
}

#[tokio::test]
async fn delayed_assignment_starts_inventory_deadlines_only_when_assigned() {
    for wall_shorter in [false, true] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path().join("ipc");
        let listener = UnixListener::bind(&path).unwrap();
        let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
        let short = Duration::from_millis(30);
        let long = Duration::from_secs(1);
        let (wall, idle) = if wall_shorter {
            (short, long)
        } else {
            (long, short)
        };
        let mut worker =
            tokio::spawn(async move { run_with_limits(&path, uid(), wall, idle).await });
        let (mut stream, _) = listener.accept().await.unwrap();
        let _: Hello = read_message(&mut stream).await.unwrap();
        // More than the shorter job budget passes before a lease is offered.
        assert!(timeout(short * 2, &mut worker).await.is_err());
        let assigned = Instant::now();
        write_message(
            &mut stream,
            &ParentMessage::Job {
                assignment: Assignment::for_lease(&lease),
            },
        )
        .await
        .unwrap();
        // No inventory end: neither SDK nor a completion is allowed to start.
        assert!(matches!(
            timeout(long * 2, worker).await.unwrap().unwrap(),
            Err(WorkerError::Deadline)
        ));
        assert!(assigned.elapsed() >= short);
        assert!(transport::read_wire(&mut stream).await.is_err());
    }
}

#[tokio::test]
async fn idle_eof_and_expired_assignment_fail_closed() {
    for expired in [false, true] {
        let (mut worker, mut parent) = UnixStream::pair().unwrap();
        if expired {
            let dir = tempfile::tempdir().unwrap();
            let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
            let mut assignment = Assignment::for_lease(&lease);
            assignment.expires_at = 0;
            write_message(&mut parent, &ParentMessage::Job { assignment })
                .await
                .unwrap();
        }
        drop(parent);
        assert!(matches!(
            wait_assignment(&mut worker, Duration::from_secs(1)).await,
            Err(WorkerError::Protocol(_))
        ));
    }
}

#[tokio::test]
async fn inventory_checks_order_digest_cap_and_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let (_ledger, lease) = job(dir.path(), "wss://relay.example.com");
    for failure in 0..7 {
        let assignment = Assignment::for_lease(&lease);
        let mut wire = ipc::encode_value(&ParentMessage::Job {
            assignment: Assignment::for_lease(&lease),
        })
        .unwrap();
        let items = match failure {
            1 => vec![([1; 32], 150), ([1; 32], 150)],
            2 => vec![([1; 32], 99)],
            3 => vec![([1; 32], 150); INVENTORY_CHUNK_ITEMS + 1],
            6 => vec![([1; 32], 150), ([1; 32], 151)],
            _ => vec![([1; 32], 150)],
        };
        let mut hash = inventory_hasher();
        for (id, timestamp) in &items {
            hash_item(&mut hash, id, *timestamp);
        }
        wire.extend(
            ipc::encode_value(&ParentMessage::InventoryChunk {
                header: assignment.header(1),
                items,
            })
            .unwrap(),
        );
        wire.extend(
            ipc::encode_value(&ParentMessage::InventoryEnd {
                header: assignment.header(if failure == 4 { 3 } else { 2 }),
                count: 1,
                digest: if failure == 5 {
                    [0; 32]
                } else {
                    hash.finalize().into()
                },
            })
            .unwrap(),
        );
        let outcome = transport::inventory(&mut &wire[..]).await;
        assert_eq!(outcome.is_ok(), failure == 0);
        wire.pop();
        assert!(transport::inventory(&mut &wire[..]).await.is_err());
    }
    let mut oversized = &((ipc::MAX_FRAME_BYTES as u32 + 1).to_be_bytes())[..];
    assert!(matches!(
        read_message::<_, Hello>(&mut oversized).await,
        Err(ProtocolError::Limit)
    ));
    let (mut write, mut read) = tokio::io::duplex(64);
    write.write_all(&10u32.to_be_bytes()).await.unwrap();
    drop(write);
    assert!(read_message::<_, Hello>(&mut read).await.is_err());
}

#[tokio::test]
async fn lost_or_wrong_ack_never_reports_success_and_preserves_received_obligation() {
    for wrong_ack in [false, true] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("ipc");
        let listener = UnixListener::bind(&socket).unwrap();
        let (url, server) = relay(false, false).await;
        let (mut ledger, lease) = job(dir.path(), "wss://relay.example.com");
        let parent = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            send_inventory(&mut stream, &lease, &url).await;
            let wire = transport::read_wire(&mut stream).await.unwrap();
            let mut session = UploadSession::new(lease.clone());
            assert!(matches!(
                session
                    .receive(&mut ledger, Frame::read(&mut &wire[..]).unwrap(), now())
                    .unwrap(),
                UploadAction::Event(_)
            ));
            if wrong_ack {
                write_message(
                    &mut stream,
                    &ParentMessage::Accepted {
                        outcome: crate::sync::ipc::AdmissionOutcome::Admitted,
                        header: Assignment::for_lease(&lease).header(2),
                    },
                )
                .await
                .unwrap();
                assert!(transport::read_wire(&mut stream).await.is_err());
            }
            // No admission/ACK: the receipt must survive this socket loss.
        };
        let (outcome, ()) = timeout(Duration::from_secs(5), async {
            tokio::join!(
                run_with_limits(
                    &socket,
                    uid(),
                    Duration::from_secs(3),
                    Duration::from_secs(2)
                ),
                parent
            )
        })
        .await
        .unwrap();
        server.abort();
        let _ = server.await;
        assert!(outcome.is_err());
        assert!(ledger.expire(lease.expires_at(), 60).unwrap());
        let progress = ledger
            .attempt_progress(lease.job().id, lease.job().attempt)
            .unwrap();
        assert_eq!(progress.received, 1);
        assert_eq!(progress.archived, 0);
        assert!(!progress.protocol_done);
        assert_eq!(
            ledger.get(lease.job().id).unwrap().state,
            JobState::RetryWait
        );
    }
}
