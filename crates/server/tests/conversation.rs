use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use survive_core::{Agent, Store};
use survive_server::{App, api};
use tower::ServiceExt;

fn birth(store: &Store, owner: &str) -> Agent {
    let env = store
        .create_environment(
            owner,
            serde_json::from_value(json!({"description":"Private environment"})).unwrap(),
        )
        .unwrap();
    store
        .birth_organisms(
            owner,
            &env.id,
            serde_json::from_value(json!({
                "count":1, "defaults":{"initial_energy":2000, "lifetime_seconds":60}
            }))
            .unwrap(),
        )
        .unwrap()
        .remove(0)
}

async fn request(
    router: &Router,
    path: &str,
    token: Option<&str>,
    ip: &str,
    payload: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().uri(path).extension(ConnectInfo(
        format!("{ip}:9000").parse::<SocketAddr>().unwrap(),
    ));
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let req = if let Some(payload) = payload {
        req.method("POST")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
    } else {
        req.body(Body::empty())
    };
    let response = router.clone().oneshot(req.unwrap()).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn only_the_creator_and_source_bound_organism_can_use_their_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(serde_json::from_value(json!({
        "data_dir":dir.path(),"listen":"127.0.0.1:0","env_listen":"127.0.0.1:0","test_mode":true,
        "creators":[{"id":"alice","token_env":"PATH"}],
        "gateway":{"providers":[{"name":"fixture","base_url":"http://127.0.0.1:9/v1","models":["default"]}]}
    })).unwrap()).unwrap();
    let alice = std::env::var("PATH").unwrap();
    let a = birth(&app.0.store, "alice");
    let b = birth(&app.0.store, "bob");
    let ka = app.0.store.agent_credential(&a.id).unwrap();
    let kb = app.0.store.agent_credential(&b.id).unwrap();
    let owner = api::owner_router(app.clone());
    let guest = api::environment_router(app.clone());
    let path = format!("/api/v2/organisms/{}/messages", a.id);
    let foreign = format!("/api/v2/organisms/{}/messages", b.id);
    let inbox = "/env/v2/messages";
    let who = Some(alice.as_str());
    let local = "127.0.0.1";
    let payload = json!({"text":"早上好","idempotency_key":"first"});
    for body in [None, Some(payload.clone())] {
        assert_eq!(
            request(&owner, &path, None, local, body.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&owner, &path, Some(&ka), local, body.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&owner, &foreign, who, local, body).await.0,
            StatusCode::NOT_FOUND
        );
    }
    for extra in ["role", "organism_id", "owner", "reply_to"] {
        let mut spoof = payload.clone();
        spoof[extra] = json!("spoofed");
        assert_eq!(
            request(&owner, &path, who, local, Some(spoof)).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    for text in [" \n ".to_owned(), "界".repeat(683)] {
        assert_eq!(
            request(
                &owner,
                &path,
                who,
                local,
                Some(json!({"text":text,"idempotency_key":"invalid"}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (status, sent) = request(&owner, &path, who, local, Some(payload.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sent["message"]["seq"], 1);
    assert_eq!(sent["message"]["role"], "creator");
    assert_eq!(sent["message"]["text"], "早上好");
    assert!(sent["message"]["created_at"].as_u64().unwrap() > 0);
    assert!(sent["message"]["reply_to"].is_null());
    assert_eq!(
        request(&owner, &path, who, local, Some(payload)).await.1,
        sent
    );
    assert_eq!(
        request(
            &owner,
            &path,
            who,
            local,
            Some(json!({"text":"different","idempotency_key":"first"}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );

    let reply = json!({"text":"你好。","idempotency_key":"first","reply_to":1});
    for body in [None, Some(reply.clone())] {
        assert_eq!(
            request(&guest, inbox, None, &a.source_ip, body.clone())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&guest, inbox, who, &a.source_ip, body.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&guest, inbox, Some(&kb), &a.source_ip, body)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let foreign_view = request(&guest, inbox, Some(&kb), &b.source_ip, None).await;
    assert_eq!(
        foreign_view,
        (
            StatusCode::OK,
            json!({"messages":[],"next_after":0,"has_more":false})
        )
    );
    assert_eq!(
        request(&guest, inbox, Some(&kb), &b.source_ip, Some(reply.clone()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let self_path = "/env/v2/self";
    let (_, info) = request(&guest, self_path, Some(&ka), &a.source_ip, None).await;
    assert_eq!(info["pending_messages"], json!([sent["message"]]));
    assert_eq!(info["recent_messages"], info["pending_messages"]);
    assert_eq!(info["paths"]["environment"], "/opt/survive/environment.md");
    let mut spoof = reply.clone();
    spoof["organism_id"] = json!(b.id);
    assert_eq!(
        request(&guest, inbox, Some(&ka), &a.source_ip, Some(spoof))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, answered) =
        request(&guest, inbox, Some(&ka), &a.source_ip, Some(reply.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answered["message"]["reply_to"], 1);
    assert_eq!(answered["message"]["role"], "organism");
    assert_eq!(
        request(&guest, inbox, Some(&ka), &a.source_ip, Some(reply))
            .await
            .1,
        answered
    );
    for (parent, status) in [(1, StatusCode::CONFLICT), (2, StatusCode::BAD_REQUEST)] {
        let body = json!({"reply_to":parent, "text":"again", "idempotency_key":"another"});
        assert_eq!(
            request(&guest, inbox, Some(&ka), &a.source_ip, Some(body))
                .await
                .0,
            status
        );
    }
    let tail = request(&owner, &format!("{path}?limit=1"), who, local, None).await;
    assert_eq!(
        tail,
        (
            StatusCode::OK,
            json!({"messages":[answered["message"]],"next_after":2,"has_more":false})
        )
    );
    let empty = request(
        &guest,
        "/env/v2/messages?pending=true&limit=1",
        Some(&ka),
        &a.source_ip,
        None,
    )
    .await;
    assert_eq!(
        empty,
        (
            StatusCode::OK,
            json!({"messages":[],"next_after":0,"has_more":false})
        )
    );
}
