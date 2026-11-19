//! Isolated local upstream for the v2 real-sandbox acceptance fixture.
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[tokio::main]
async fn main() {
    let calls = Arc::new(AtomicUsize::new(0));
    let router=Router::new().route("/v1/chat/completions",post(move|Json(_):Json<Value>|{
     let index=calls.fetch_add(1,Ordering::SeqCst);
     async move{
       let (name,args,total)=match index{
          0=>("write_file",json!({"path":"answer.txt","content":"frozen smoke artifact"}),150),
          1=>("report",json!({"body":"ready for review","idempotency_key":"sample","files":["answer.txt"]}),150),
          2=>("sleep",json!({"seconds":3600}),49700),
          3=>("write_file",json!({"path":"resumed.txt","content":"restored after energy grant"}),150),
          4=>("sleep",json!({"seconds":3600}),150),
          _=>("sleep",json!({"seconds":3600}),49700)
       };
       Json(json!({"id":format!("chat-smoke-{index}"),"object":"chat.completion","created":0,"model":"smoke","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":total-50,"completion_tokens":50,"total_tokens":total}}))
     }
 }));
    let address =
        std::env::var("SURVIVE_SMOKE_LISTEN").unwrap_or_else(|_| "127.0.0.1:18090".into());
    let socket: std::net::SocketAddr = address.parse().expect("listen address");
    assert!(socket.ip().is_loopback());
    let listener = tokio::net::TcpListener::bind(socket).await.unwrap();
    axum::serve(listener, router).await.unwrap();
}
