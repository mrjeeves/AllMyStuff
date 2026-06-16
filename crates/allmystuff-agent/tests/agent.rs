//! The agent's reusable core, exercised against the in-memory backend.

use allmystuff_agent::{watch_once, Config};
use allmystuff_cec::convention;
use allmystuff_cec::mock::{MockBackend, MockTransport};
use allmystuff_cec::model::*;
use allmystuff_cec::CecClient;

#[test]
fn config_round_trips_and_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("allmystuff-agent.json");

    // Missing file → defaults.
    let fresh = Config::load(&path);
    assert!(!fresh.is_signed_in());
    assert_eq!(fresh.backend_url, allmystuff_agent::DEFAULT_BACKEND);

    // Save then reload.
    let c = Config {
        backend_url: "http://127.0.0.1:8787".into(),
        token: Some("tok_abc".into()),
        email: Some("sam@cec.example".into()),
    };
    c.save(&path).unwrap();

    let back = Config::load(&path);
    assert!(back.is_signed_in());
    assert_eq!(back.backend_url, "http://127.0.0.1:8787");
    assert_eq!(back.email.as_deref(), Some("sam@cec.example"));
}

#[tokio::test]
async fn watch_once_picks_up_the_oldest_queued_session() {
    let backend = MockBackend::shared();

    // A customer queues a request.
    let mut customer = CecClient::new(MockTransport::new(backend.clone()));
    customer
        .dev_grant(&DevGrant {
            email: "casey@example.com".into(),
            entitlements: Some(Entitlements {
                concierge: Some(ConciergeTier::PayAsYouGo),
                ..Default::default()
            }),
            agent: false,
        })
        .await
        .unwrap();
    customer.start_sign_in("casey@example.com").await.unwrap();
    let code = backend.last_code("casey@example.com").unwrap();
    customer
        .verify_sign_in("casey@example.com", &code, Some("dev-casey"), None)
        .await
        .unwrap();
    let prov = customer.provision_mesh("dev-casey").await.unwrap();
    let session = customer
        .ask_for_help(&AskForHelp {
            network_id: prov.network_id,
            room_id: convention::help_room_id("dev-casey"),
            device_id: "dev-casey".into(),
            topic: Some("can't hear sound".into()),
        })
        .await
        .unwrap();

    // An agent goes online and watches with auto-accept.
    let mut agent = CecClient::new(MockTransport::new(backend.clone()));
    agent
        .dev_grant(&DevGrant {
            email: "sam@cec.example".into(),
            agent: true,
            ..Default::default()
        })
        .await
        .unwrap();
    agent.start_sign_in("sam@cec.example").await.unwrap();
    let acode = backend.last_code("sam@cec.example").unwrap();
    agent
        .verify_sign_in("sam@cec.example", &acode, None, None)
        .await
        .unwrap();
    agent.set_presence(true).await.unwrap();

    let report = watch_once(&agent, true).await.unwrap();
    assert_eq!(report.queued.len(), 1);
    let accepted = report.accepted.expect("auto-accept took the session");
    assert_eq!(accepted.session.id, session.id);
    assert_eq!(accepted.session.status, HelpStatus::Assigned);

    // Next pass: nothing left waiting.
    let again = watch_once(&agent, true).await.unwrap();
    assert!(again.queued.is_empty());
    assert!(again.accepted.is_none());
}
