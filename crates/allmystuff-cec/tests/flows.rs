//! End-to-end flows against the in-memory reference backend. These exercise
//! the same `CecClient` code paths the app and the agent binary use — the
//! only difference is the transport (here: no sockets).

use std::sync::Arc;

use allmystuff_cec::convention;
use allmystuff_cec::mock::{MockBackend, MockConfig, MockTransport};
use allmystuff_cec::model::*;
use allmystuff_cec::{CecClient, Error, Method};

/// Sign a customer in (granting them a Concierge tier first) and bind a device.
async fn signed_in_customer(
    backend: &Arc<MockBackend>,
    email: &str,
    device_id: &str,
) -> CecClient<MockTransport> {
    let mut client = CecClient::new(MockTransport::new(backend.clone()));
    client
        .dev_grant(&DevGrant {
            email: email.into(),
            entitlements: Some(Entitlements {
                concierge: Some(ConciergeTier::Priority),
                ..Default::default()
            }),
            agent: false,
        })
        .await
        .unwrap();
    client.start_sign_in(email).await.unwrap();
    let code = backend.last_code(email).expect("a code was sent");
    client
        .verify_sign_in(email, &code, Some(device_id), Some("Casey's laptop"))
        .await
        .unwrap();
    client
}

#[tokio::test]
async fn customer_signs_in_provisions_mesh_and_asks_for_help() {
    let backend = MockBackend::shared();
    let client = signed_in_customer(&backend, "casey@example.com", "dev-casey-1").await;

    // me() reflects the grant + binding.
    let me = client.me().await.unwrap();
    assert!(me.account.is_customer());
    assert_eq!(me.entitlements.concierge, Some(ConciergeTier::Priority));
    assert!(me.entitlements.can_ask_for_help());
    assert!(me.entitlements.wants_cec_mesh());
    assert!(me.account.device_ids.contains(&"dev-casey-1".to_string()));

    // Provision the isolated CEC mesh.
    let prov = client.provision_mesh("dev-casey-1").await.unwrap();
    assert!(convention::is_cec_network(&prov.network_id));
    assert!(prov.auto_approve);
    assert!(!prov.cec_service_node_id.is_empty());
    assert!(prov.venue.url.is_some());
    // Stable: a second provision is the same network + service node.
    let prov2 = client.provision_mesh("dev-casey-1").await.unwrap();
    assert_eq!(prov.network_id, prov2.network_id);
    assert_eq!(prov.cec_service_node_id, prov2.cec_service_node_id);

    // The venue file is fetchable (this is the remote-venue the app loads).
    let venue_path = prov.venue.url.unwrap();
    let file = client.raw(Method::Get, &venue_path, None).await.unwrap();
    assert_eq!(file["kind"], VenueFile::KIND);
    assert!(!file["signaling_servers"].as_array().unwrap().is_empty());

    // Ask for help: mint a help room hosted by this device, then open a session.
    let room_id = convention::help_room_id("dev-casey-1");
    assert!(convention::is_help_room(&room_id));
    let session = client
        .ask_for_help(&AskForHelp {
            network_id: prov.network_id.clone(),
            room_id: room_id.clone(),
            device_id: "dev-casey-1".into(),
            topic: Some("printer won't print".into()),
        })
        .await
        .unwrap();
    assert_eq!(session.status, HelpStatus::Queued);
    assert_eq!(session.room_id, room_id);
    assert_eq!(session.cec_service_node_id, prov.cec_service_node_id);

    // The customer can poll their own session.
    let polled = client.help_status(&session.id).await.unwrap();
    assert_eq!(polled.status, HelpStatus::Queued);
}

#[tokio::test]
async fn agent_sees_queue_accepts_and_customer_sees_assignment() {
    let backend = MockBackend::shared();

    // A customer queues a help request.
    let customer = signed_in_customer(&backend, "casey@example.com", "dev-casey-1").await;
    let prov = customer.provision_mesh("dev-casey-1").await.unwrap();
    let room_id = convention::help_room_id("dev-casey-1");
    let session = customer
        .ask_for_help(&AskForHelp {
            network_id: prov.network_id.clone(),
            room_id,
            device_id: "dev-casey-1".into(),
            topic: None,
        })
        .await
        .unwrap();

    // An agent signs in.
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
    let code = backend.last_code("sam@cec.example").unwrap();
    agent
        .verify_sign_in("sam@cec.example", &code, None, None)
        .await
        .unwrap();
    assert!(agent.me().await.unwrap().account.is_agent());

    // Offline → the queue is hidden.
    let offline = agent.agent_queue().await.unwrap_err();
    assert_eq!(offline.code(), Some("offline"));

    // Go online → the queued session shows up.
    let presence = agent.set_presence(true).await.unwrap();
    assert!(presence.online);
    let queue = agent.agent_queue().await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, session.id);

    // Accept → assignment carries the venue to join as the CEC Service node.
    let assignment = agent.accept_help(&session.id).await.unwrap();
    assert_eq!(assignment.session.status, HelpStatus::Assigned);
    assert!(assignment.session.agent_label.is_some());
    assert!(!assignment.venue.signaling.is_empty());

    // The customer now sees it assigned, with the agent's name.
    let polled = customer.help_status(&session.id).await.unwrap();
    assert_eq!(polled.status, HelpStatus::Assigned);
    assert_eq!(polled.agent_label.as_deref(), Some("sam"));

    // A second agent can't double-accept.
    let taken = agent.accept_help(&session.id).await.unwrap_err();
    assert_eq!(taken.code(), Some("taken"));

    // End the session.
    agent.end_help(&session.id).await.unwrap();
    assert_eq!(
        customer.help_status(&session.id).await.unwrap().status,
        HelpStatus::Ended
    );
}

#[tokio::test]
async fn private_line_rent_list_and_cancel_updates_entitlements() {
    let backend = MockBackend::shared();
    let client = signed_in_customer(&backend, "casey@example.com", "dev-1").await;

    assert!(!client.me().await.unwrap().entitlements.private_line);

    let pl = client.rent_private_line(Some("Home")).await.unwrap();
    assert_eq!(pl.status, SubscriptionStatus::Active);
    assert_eq!(pl.label, "Home");
    assert!(pl.venue.url.is_some());
    assert_eq!(pl.monthly_price_cents, 1000);

    assert!(client.me().await.unwrap().entitlements.private_line);
    assert_eq!(client.list_private_lines().await.unwrap().len(), 1);

    client.cancel_private_line(&pl.id).await.unwrap();
    assert!(!client.me().await.unwrap().entitlements.private_line);
    assert_eq!(
        client.list_private_lines().await.unwrap()[0].status,
        SubscriptionStatus::Cancelled
    );
}

#[tokio::test]
async fn auth_failures_are_typed() {
    let backend = MockBackend::shared();
    let mut client = CecClient::new(MockTransport::new(backend.clone()));

    // Unauthenticated calls fail before hitting the wire.
    assert!(matches!(client.me().await, Err(Error::Unauthenticated)));

    // Wrong code → 401 bad_code.
    client.start_sign_in("x@y.z").await.unwrap();
    let err = client
        .verify_sign_in("x@y.z", "000000", None, None)
        .await
        .unwrap_err();
    match err {
        Error::Api { status, code, .. } => {
            assert_eq!(status, 401);
            assert_eq!(code.as_deref(), Some("bad_code"));
        }
        other => panic!("expected Api error, got {other:?}"),
    }

    // A non-agent can't touch agent endpoints.
    let real_code = {
        client.start_sign_in("x@y.z").await.unwrap();
        backend.last_code("x@y.z").unwrap()
    };
    client
        .verify_sign_in("x@y.z", &real_code, None, None)
        .await
        .unwrap();
    let not_agent = client.set_presence(true).await.unwrap_err();
    assert_eq!(not_agent.code(), Some("not_agent"));
}

#[tokio::test]
async fn demo_config_lights_everything_up() {
    // The server's --demo mode: every account gets a tier and is an agent.
    let backend = Arc::new(MockBackend::with_config(MockConfig {
        default_entitlements: Entitlements {
            concierge: Some(ConciergeTier::PayAsYouGo),
            private_line: false,
            hardware: false,
        },
        everyone_is_agent: true,
    }));
    let mut client = CecClient::new(MockTransport::new(backend.clone()));
    client.start_sign_in("solo@example.com").await.unwrap();
    let code = backend.last_code("solo@example.com").unwrap();
    let session = client
        .verify_sign_in("solo@example.com", &code, Some("dev-solo"), None)
        .await
        .unwrap();
    assert!(session.entitlements.can_ask_for_help());
    assert!(session.account.is_agent());
    assert!(session.account.is_customer());
}
