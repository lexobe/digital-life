//! Explicit #demo fixtures. Authenticated pages fetch all data from the creator API.
use serde_json::{Value, json};
fn now() -> u64 {
    js_sys::Date::now() as u64
}
pub fn overview() -> Value {
    let now = now();
    let env = json!({"id":"demo-environment","owner":"demo","name":"开放环境","description":super::forms::OPEN_ENVIRONMENT,"note":"演示用环境。","created_at":now-86_400_000});
    let organisms:Vec<_>=[("苔藓","running",43280),("","queued",50000),("余晖","depleted",0)].iter().enumerate().map(|(i,(name,state,free))|{
        json!({"id":format!("demo-organism-{:02}",i+1),"slot":i+1,"name":name,"environment_id":"demo-environment","state":state,"generation":1,"created_at":now-3_600_000,
               "initial_energy":50000,"lifetime_seconds":86400,"dna":"","default_model":"gpt-6.2-sol","allowed_models":null,"network":{"peers":true,"internet":true,"services":true},"limits":{"memory_bytes":268435456,"cpu_millis":500,"pids":64},
               "life":{"energy_balance":free,"energy_available":free,"energy_reserved":0,"remaining_lifetime_ms":86400000-i as u64*900000,"last_heartbeat_at":if i==1{None}else{Some(now-450000)},"stop_reasons":if *free==0{vec!["energy"]}else{vec![]}}})
    }).collect();
    json!({"creator_id":"demo","generated_at":now,"environments":[env],"organisms":organisms,"summary":{"running":1},

           "system":{"models":[{"id":"gpt-6.2-sol","input":1,"output":3,"available":true},{"id":"demo-research","input":4,"output":12,"available":true}],"runtime":{"backend":"youki","available":true,"status":"演示就绪","message":"本地合成示例。"},"limits":{"pool_memory_bytes":1073741824},"llm":{"configured":true,"providers":[{"name":"Demo route","configured":true}]}},
           "recent_events":[{"id":"demo-event-01","agent_id":"demo-organism-01","kind":"heartbeat","created_at":now-450000,"data":{"energy_available":43280,"remaining_lifetime_ms":86400000}}]})
}
pub fn topology(snapshot: &Value) -> Value {
    let mut nodes = vec![
        json!({"id":"demo-environment","kind":"environment","label":"开放环境","environment_id":"demo-environment"}),
        json!({"id":"llm","kind":"llm","label":"LLM","state":"configured"}),
    ];
    let mut edges = vec![];
    for a in super::arr(snapshot, "organisms") {
        nodes.push(json!({"id":a["id"],"kind":"organism","organism_id":a["id"],"label":if super::s(a,"name").is_empty(){a["id"].clone()}else{a["name"].clone()},"state":a["state"]}));
        edges.push(json!({"source":a["id"],"target":"demo-environment","kind":"membership","basis":"configured"}));
        edges.push(
            json!({"source":a["id"],"target":"llm","kind":"llm_access","basis":"configured"}),
        );
    }
    edges.push(json!({"source":"demo-organism-01","target":"llm","kind":"settled_energy","basis":"observed","count":6720}));
    json!({"nodes":nodes,"edges":edges})
}
pub fn report(id: &str) -> Value {
    json!({"id":"demo-report-01","organism_id":id,"body":"完成第一轮观察，附上本次报告的记录。","files":["notes.md"],"snapshots":[{"path":"notes.md","data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,"# 观察记录\n第一次观察已完成。")}],"created_at":now()-540000})
}
pub fn detail(id: &str, tab: &str, path: &str, _offset: u64) -> Value {
    let first = id == "demo-organism-01";
    match tab {
        "events" => json!({"events":if first{overview()["recent_events"].clone()}else{json!([])}}),
        "ledger" => {
            json!({"life":{"energy_balance":43280,"energy_reserved":0,"energy_available":43280},"granted":50000,"used":if first{6720}else{0},"reservations":if first{json!([{"id":"demo-call-01","status":"settled","amount":7200,"model":"default","price":{"input":1,"output":3},"usage":{"input_tokens":3720,"output_tokens":1000},"energy_charged":6720,"created_at":now()-60000}])}else{json!([])}})
        }
        "reports" => json!({"reports":if first{vec![report(id)]}else{vec![]}}),
        "grants" => {
            json!({"grants":if first{json!([{"id":"demo-grant-01","organism_id":id,"idempotency_key":"demo-time","lifetime_seconds":3600,"energy":0,"note":"延长观察窗口","report_id":"demo-report-01","created_at":now()-480000}])}else{json!([])}})
        }
        "services" => {
            json!({"services":if first{json!([{"name":"生长日志","port":8080,"preview":{"mode":"live"}}])}else{json!([])}})
        }
        "files" => {
            if path.is_empty() {
                json!({"entries":["dna.md","notes.md"],"path":""})
            } else {
                use base64::Engine;
                let text = if path == "dna.md" {
                    "记录环境中的变化。"
                } else {
                    "# 观察记录\n新一轮观察正在进行。"
                };
                json!({"path":path,"offset":0,"next_offset":text.len(),"data":base64::engine::general_purpose::STANDARD.encode(text)})
            }
        }
        _ => Value::Null,
    }
}
