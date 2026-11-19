use super::*;

pub(super) fn life(v: &Value) -> &Value {
    v.get("life").unwrap_or(v)
}
fn fact(label: &str, value: impl Into<String>) -> Html {
    html! {<div><dt>{label}</dt><dd>{value.into()}</dd></div>}
}
fn raw(v: &Value) -> Html {
    html! {<pre class="code-block">{serde_json::to_string_pretty(v).unwrap_or_default()}</pre>}
}
fn report_attachment(file: &Value) -> Html {
    let path = s(file, "path");
    let encoded = s(file, "data");
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return raw(file);
    };
    let filename = path
        .rsplit('/')
        .next()
        .filter(|v| !v.is_empty())
        .unwrap_or("snapshot");
    html! {<details class="advanced attachment"><summary>{path}</summary><div class="attachment-tools">
    <a class="text-link" href={format!("data:application/octet-stream;base64,{encoded}")} download={filename.to_owned()}>{"下载提交时的文件"}</a>
    <span>{format!("{} 字节",num(bytes.len() as u64))}</span></div>
    {if let Ok(text)=std::str::from_utf8(&bytes){html!{<pre class="code-block">{text}</pre>}}else{html!{<p class="small muted">{"二进制附件"}</p>}}}
    {if !s(file,"sha256").is_empty(){html!{<code class="attachment-hash">{format!("SHA-256 · {}",s(file,"sha256"))}</code>}}else{Html::default()}}</details>}
}
impl App {
    pub(super) fn agent_page(&self, ctx: &Context<Self>, id: &str) -> Html {
        let a = self.agent(id);
        if a.is_null() {
            return empty("等待生物档案", "请刷新列表后重试。");
        }
        let recipient = id.to_string();
        html! {<div class="page"><button class="back-link" onclick={ctx.link().callback(|_|Msg::Navigate(Page::Individuals))}>{"← 生物档案"}</button>
        <div class="agent-page-heading"><div><span class="eyebrow">{"ORGANISM"}</span><h1>{display_name(&a)}</h1><code>{id}</code></div>{badge(s(&a,"state"))}
        <button class="button primary" onclick={ctx.link().callback(move|_|Msg::OpenForm(Action::Grant(recipient.clone()),None))}>{icon("plus")}{"发放资源"}</button></div>
        <div class="tabs" role="tablist">{for [("overview","生命状态"),("events","事件"),("ledger","能量账本"),("reports","报告"),("grants","发放历史"),("files","工作区"),("services","服务")].map(|(key,label)|html!{
            <button role="tab" aria-selected={(self.tab==key).to_string()} class={classes!((self.tab==key).then_some("active"))} onclick={ctx.link().callback(move|_|Msg::Tab(key.into()))}>{label}</button>
        })}</div>
        {if self.detail_loading{html!{<div class="loading-screen"><span class="spinner"/><p>{"正在读取记录…"}</p></div>}}else if let Some(error)=&self.detail_error{html!{<div class="panel padded" role="alert"><p>{error}</p><button class="text-link" onclick={ctx.link().callback(|_|Msg::RetryDetail)}>{"重试"}</button></div>}}else{match self.tab.as_str(){
            "overview"=>self.life_view(ctx,&a),"events"=>self.events_view(ctx),"ledger"=>self.ledger_view(ctx),"files"=>self.files_view(ctx),
            "services"=>self.services_view(ctx),"reports"=>self.reports_view(ctx,id),"grants"=>self.grants_view(ctx),_=>Html::default()}}}
        {if matches!(self.tab.as_str(),"ledger"|"reports"|"events"|"grants") && (self.page_cursors.len()>1||self.detail["next_cursor"].is_string()){self.pagination(ctx)}else{Html::default()}}</div>}
    }
    fn life_view(&self, ctx: &Context<Self>, a: &Value) -> Html {
        let l = life(a);
        let env = self.exp(s(a, "environment_id"));
        let birth = json!({
            "initial_energy":a["initial_energy"], "lifetime_seconds":a["lifetime_seconds"],
            "default_model":a["default_model"], "allowed_models":a["allowed_models"],
            "limits":a["limits"], "network":a["network"], "dna":a["dna"]
        });
        let birth = &birth;
        let permitted = birth["allowed_models"]
            .as_array()
            .or_else(|| env["allowed_models"].as_array());
        let stop = arr(l, "stop_reasons")
            .iter()
            .filter_map(Value::as_str)
            .map(|v| match v {
                "energy" => "能量耗尽",
                "lifetime" => "寿命耗尽",
                v => v,
            })
            .collect::<Vec<_>>()
            .join(" · ");
        html! {<><div class="detail-grid"><section class="panel padded"><h2>{"生命状态"}</h2><div class="detail-balance"><span>{"剩余运行寿命"}</span><strong>{lifetime(l)}</strong></div>
        <dl class="facts">{fact("可用能量",amount(l,"energy_available"))}{fact("账面余额",amount(l,"energy_balance"))}{fact("调用预留",amount(l,"energy_reserved"))}
        {fact("最近心跳",timestamp(l,"last_heartbeat_at"))}{fact("启动代次",amount(a,"generation"))}{fact("出生时间",timestamp(a,"created_at"))}
        {if !stop.is_empty(){fact("停止原因",stop)}else{Html::default()}}</dl>
        {if !s(a,"last_error").is_empty(){html!{<p class="inline-error">{s(a,"last_error")}</p>}}else{Html::default()}}
        </section><section class="panel padded"><h2>{display_name(&env)}</h2><p class="body-copy">{s(&env,"description")}</p>
        <dl class="facts">{fact("出生默认模型",s(birth,"default_model"))}
        {fact("允许的模型",permitted.map(|ids|ids.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_else(||"环境全部模型".into()))}
        {fact("出生能量",amount(birth,"initial_energy"))}{fact("出生寿命（秒）",amount(birth,"lifetime_seconds"))}</dl>
        <button class="text-link" onclick={ctx.link().callback(|_|Msg::Dna)}>{"查看当前 /work/dna.md"}{icon("arrow")}</button>
        <details class="advanced"><summary>{"出生配置与初始 DNA"}</summary>{raw(birth)}</details></section></div>
        <section class="panel padded spaced"><h2>{"模型价格"}</h2>{self.price_table_for(permitted.map(Vec::as_slice))}</section></>}
    }
    fn ledger_view(&self, ctx: &Context<Self>) -> Html {
        let rows = arr(&self.detail, "reservations");
        let life_record = life(&self.detail);
        html! {<section class="panel"><div class="panel-heading"><h2>{"模型调用与结算"}</h2></div><div class="ledger-totals">
            {for [("累计获得","granted"),("累计消耗","used"),("预留能量","energy_reserved"),("可用能量","energy_available")].map(|(label,key)|html!{<div><span>{label}</span><strong>{if self.detail[key].is_number(){amount(&self.detail,key)}else{amount(life_record,key)}}</strong></div>})}</div>
        {if rows.is_empty(){empty("尚无模型调用","调用后将显示原始 token 用量、模型单价及实际能量支出。")}else{html!{<div class="table-scroll"><table><thead><tr>
        {for ["调用","模型","输入 / 输出 token","单价（输入 / 输出）","预留能量","实际扣费","状态"].map(|label|html!{<th>{label}</th>})}</tr></thead><tbody>{for rows.iter().map(|r|{
            let record=r.clone();html!{<tr><td><button class="text-link" onclick={ctx.link().callback(move|_|Msg::Inspect("调用记录".into(),record.clone()))}>{short(s(r,"id"))}</button></td>
            <td>{s(r,"model")}</td><td>{format!("{} / {}",amount(&r["usage"],"input_tokens"),amount(&r["usage"],"output_tokens"))}</td><td>{format!("{} / {}",amount(&r["price"],"input"),amount(&r["price"],"output"))}</td>
            <td>{amount(r,"amount")}</td><td>{amount(r,"energy_charged")}</td><td>{badge(s(r,"status"))}</td></tr>}
        })}</tbody></table></div>}}}</section>}
    }
    fn reports_view(&self, ctx: &Context<Self>, id: &str) -> Html {
        let rows = arr(&self.detail, "reports");
        html! {<section class="panel"><div class="panel-heading"><h2>{"生物报告"}</h2></div>
        {if rows.is_empty(){empty("尚无报告","生物主动发送的报告及提交时的文件快照将在这里显示。")}else{html!{<div class="record-list">{for rows.iter().map(|r|{
            let report=s(r,"id").to_string();let associated=report.clone();let recipient=id.to_owned();let owner=recipient.clone();
            html!{<article class="life-record"><header><time>{timestamp(r,"created_at")}</time><code>{&report}</code></header><p class="body-copy">{s(r,"body")}</p>
            <div class="record-actions"><button class="button secondary" onclick={ctx.link().callback(move|_|Msg::FetchReport(report.clone(),owner.clone()))}>{"查看报告与快照"}</button>
            <button class="text-link" onclick={ctx.link().callback(move|_|Msg::OpenForm(Action::Grant(recipient.clone()),Some(associated.clone())))}>{"关联报告发放资源"}{icon("arrow")}</button></div></article>}
        })}</div>}}}</section>}
    }
    fn grants_view(&self, ctx: &Context<Self>) -> Html {
        let rows = arr(&self.detail, "grants");
        html! {<section class="panel"><div class="panel-heading"><h2>{"资源发放记录"}</h2></div>
        {if rows.is_empty(){empty("尚无发放记录","创造者发放的能量和寿命将在这里显示。")}else{html!{<div class="table-scroll"><table><thead><tr>{for ["时间","新增能量","延寿（秒）","关联报告","备注","幂等键"].map(|s|html!{<th>{s}</th>})}</tr></thead>
        <tbody>{for rows.iter().map(|g|{let record=g.clone();html!{<tr><td>{timestamp(g,"created_at")}</td><td>{amount(g,"energy")}</td><td>{amount(g,"lifetime_seconds")}</td><td>{s(g,"report_id")}</td><td>{s(g,"note")}</td><td><button class="text-link" onclick={ctx.link().callback(move|_|Msg::Inspect("资源发放".into(),record.clone()))}>{s(g,"idempotency_key")}</button></td></tr>}})}</tbody></table></div>}}}</section>}
    }
    pub(super) fn inspection(title: &str, v: &Value) -> Html {
        if title != "报告与提交时快照" {
            return raw(v);
        }
        let r = v.get("report").unwrap_or(v);
        html! {<><p class="body-copy">{s(r,"body")}</p><p class="muted">{timestamp(r,"created_at")}</p>
        <h3>{"提交时的文件快照"}</h3>
        {if let Some(files)=r["snapshots"].as_array(){
            if files.is_empty(){html!{<p>{"此报告未附文件。"}</p>}}else{html!{<>{for files.iter().map(report_attachment)}</>}}
        }else{raw(&r["snapshots"])}}
        <details class="advanced"><summary>{"完整报告记录"}</summary>{raw(r)}</details></>}
    }
    pub(super) fn price_table(&self) -> Html {
        self.price_table_for(None)
    }
    pub(super) fn price_table_for(&self, permitted: Option<&[Value]>) -> Html {
        let list = arr(&self.snapshot["system"], "models");
        html! {<div class="table-scroll price-table"><table><thead><tr><th>{"模型名称"}</th><th>{"输入能量 / token"}</th><th>{"输出能量 / token"}</th></tr></thead>
        <tbody>{for list.iter().filter(|m|permitted.is_none_or(|ids|ids.contains(&m["id"]))).map(|m|{
            html!{<tr><td>{s(m,"id")}</td><td>{amount(m,"input")}</td><td>{amount(m,"output")}</td></tr>}
        })}</tbody></table>{if list.is_empty(){html!{<p class="muted">{"服务器未返回模型价格。"}</p>}}else{Html::default()}}</div>}
    }
    pub(super) fn environment(&self) -> Html {
        let sys = &self.snapshot["system"];
        let runtime = &sys["runtime"];
        let llm = &sys["llm"];
        html! {<div class="page"><div class="page-heading"><div class="eyebrow">{"INFRASTRUCTURE"}</div><h1>{"基础设施"}</h1></div><div class="detail-grid"><section class="panel padded"><h2>{self.runtime_name()}</h2>
        <dl class="facts">{fact("运行状态",s(runtime,"status"))}{fact("隔离就绪",match runtime["available"].as_bool(){Some(true)=>"就绪",Some(false)=>"不可用",None=>"—"})}</dl><p>{s(runtime,"message")}</p></section>
        <section class="panel padded"><h2>{"模型价格"}</h2>{self.price_table()}</section></div>
        <section class="panel padded spaced"><h2>{"模型线路与资源限制"}</h2><details class="advanced" open=true><summary>{"模型线路配置"}</summary>{raw(llm)}</details><details class="advanced" open=true><summary>{"平台资源限制"}</summary>{raw(&sys["limits"])}</details></section></div>}
    }
    pub(super) fn api_page(&self) -> Html {
        html! {<div class="page"><div class="page-heading"><div><span class="eyebrow">{"API"}</span><h1>{"接口文档"}</h1></div></div><div class="detail-grid">
        <section class="panel padded"><h2>{"生存环境与模型网关"}</h2><p class="body-copy">{"环境、出生、生命状态、报告与资源发放的调用方式。"}</p><a class="button primary" href="/llm.txt" target="_blank" rel="noopener">{"打开 llm.txt"}{icon("external")}</a></section>
        <section class="panel padded"><h2>{"OpenAPI"}</h2><p class="body-copy">{"查看服务器接口、认证方式和请求结构。"}</p><a class="button secondary" href="/openapi.json" target="_blank" rel="noopener">{"打开 OpenAPI"}{icon("external")}</a></section></div></div>}
    }
}
