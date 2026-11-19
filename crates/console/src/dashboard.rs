use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn orbit(index: usize, total: usize, rx: f64, ry: f64) -> (f64, f64) {
    let angle =
        std::f64::consts::TAU * index as f64 / total.max(1) as f64 - std::f64::consts::FRAC_PI_2;
    (440. + rx * angle.cos(), 280. + ry * angle.sin())
}

impl App {
    pub(super) fn observe(&self, ctx: &Context<Self>) -> Html {
        let organisms = arr(&self.snapshot, "organisms");
        let environments = arr(&self.snapshot, "environments");
        let metrics = [
            (
                "运行中的生物",
                num(organisms
                    .iter()
                    .filter(|v| s(v, "state") == "running")
                    .count() as u64),
                "agents",
            ),
            (
                "可用能量",
                num(organisms
                    .iter()
                    .map(|v| n(life(v), "energy_available"))
                    .fold(0, u64::saturating_add)),
                "network",
            ),
            (
                "剩余运行寿命",
                lifetime(
                    &json!({"remaining_lifetime_ms":organisms.iter().map(|v|n(life(v),"remaining_lifetime_ms")).fold(0,u64::saturating_add)}),
                ),
                "clock",
            ),
            ("生存环境", num(environments.len() as u64), "experiment"),
        ];
        html! {<div class="page">{self.heading(ctx,"OBSERVATORY","生命观测台","查看生物的运行状态、能量与活动记录。",true)}
            <div class="metrics-grid">{for metrics.iter().enumerate().map(|(i,(label,val,symbol))|html!{<section class={classes!("metric-card",(i==0).then_some("featured"))}><div class="metric-label">{label}{icon(symbol)}</div><strong>{val}</strong></section>})}</div>
            <div class="observation-layout"><section class="graph-panel"><div class="panel-heading"><h2>{"环境拓扑"}</h2>{self.env_filter(ctx)}</div>{self.graph(ctx)}
                <div class="graph-legend"><span><i class="dot-green"/>{"运行中"}</span><span><i class="dot-amber"/>{"排队 / 资源耗尽"}</span><span><i class="dot-red"/>{"失败"}</span><span class="edge-key">{"虚线为配置关系，实线为已记录的累计活动"}</span></div>
            </section>{self.inspector(ctx)}</div>
            <section class="panel spaced"><div class="panel-heading"><h2>{"最近事件"}</h2><time>{timestamp(&self.snapshot,"generated_at")}</time></div>{self.activity(ctx)}</section>
        </div>}
    }
    fn env_filter(&self, ctx: &Context<Self>) -> Html {
        html! {<select aria-label="筛选环境" onchange={ctx.link().callback(|e|Msg::FilterExp(select_value(e)))}><option value="" selected={self.filter_exp.is_empty()}>{"全部环境"}</option>{for arr(&self.snapshot,"environments").iter().map(|e|html!{<option value={s(e,"id").to_string()} selected={self.filter_exp==s(e,"id")}>{display_name(e)}</option>})}</select>}
    }
    fn graph(&self, ctx: &Context<Self>) -> Html {
        let nodes = arr(&self.topology, "nodes");
        let edges = arr(&self.topology, "edges");
        let organism_ids: BTreeSet<_> = arr(&self.snapshot, "organisms")
            .iter()
            .filter(|v| self.filter_exp.is_empty() || s(v, "environment_id") == self.filter_exp)
            .map(|a| s(a, "id"))
            .collect();
        let is_organism = |node: &Value| matches!(s(node, "kind"), "organism" | "agent");
        let group: Vec<_> = nodes
            .iter()
            .filter(|v| is_organism(v) && organism_ids.contains(node_owner(v)))
            .take(24)
            .collect();
        let visible: BTreeSet<_> = group.iter().map(|v| s(v, "id")).collect();
        let connected: BTreeSet<_> = edges
            .iter()
            .filter(|e| visible.contains(s(e, "source")) || visible.contains(s(e, "target")))
            .flat_map(|v| [s(v, "source"), s(v, "target")])
            .collect();
        let hubs: Vec<_> = nodes
            .iter()
            .filter(|n| {
                !is_organism(n)
                    && (connected.contains(s(n, "id"))
                        || (s(n, "kind") == "environment"
                            && (self.filter_exp.is_empty()
                                || s(n, "environment_id") == self.filter_exp)))
            })
            .take(16)
            .collect();
        let mut positions = BTreeMap::new();
        for (i, node) in group.iter().enumerate() {
            positions.insert(s(node, "id"), orbit(i, group.len(), 325., 178.));
        }
        for (i, node) in hubs.iter().enumerate() {
            positions.insert(s(node, "id"), orbit(i, hubs.len(), 143., 78.));
        }
        let activate = move |node: &Value| {
            let data = node.clone();
            if is_organism(node) {
                Msg::Select(node_owner(node).into())
            } else {
                Msg::Inspect("拓扑节点".into(), data)
            }
        };
        html! {<div class="graph-stage"><svg class="network-svg" viewBox="0 0 880 560" role="group" aria-label="生物与环境拓扑"
        onwheel={ctx.link().callback(|e:WheelEvent|{e.prevent_default();Msg::Zoom(if e.delta_y()>0.{-0.08}else{0.08})})}
        onpointerdown={ctx.link().callback(|e:PointerEvent|Msg::DragStart(e.client_x() as f64,e.client_y() as f64))}
        onpointermove={ctx.link().callback(|e:PointerEvent|Msg::DragMove(e.client_x() as f64,e.client_y() as f64))}
        onpointerup={ctx.link().callback(|_|Msg::DragEnd)} onpointerleave={ctx.link().callback(|_|Msg::DragEnd)}>
        <g transform={format!("translate({} {}) translate(440 280) scale({}) translate(-440 -280)",self.pan.0,self.pan.1,self.zoom)}>
        <ellipse cx="440" cy="280" rx="325" ry="178" class="orbit-guide"/>
        {for edges.iter().filter_map(|e|{
            let (x,y)=positions.get(s(e,"source"))?; let (tx,ty)=positions.get(s(e,"target"))?;
            Some(html!{<path class={classes!("network-edge",(s(e,"basis")=="observed").then_some("observed"))} d={format!("M {x} {y} Q {} {} {tx} {ty}",(x+tx)/2.+20.,(y+ty)/2.)}><title>{format!("{} · {} {}",s(e,"kind"),s(e,"basis"),amount(e,"count"))}</title></path>})
        })}
        {for hubs.iter().chain(group.iter()).map(|node|{
            let (x,y)=positions[s(node,"id")];
            let a=(*node).clone();let b=a.clone();let org=is_organism(node);
            let owner=node_owner(node);
            let profile=self.agent(owner);
            let label=if org && !s(&profile,"name").is_empty(){s(&profile,"name")}else{display_label(node)};
            let state=s(node,"state");
            html!{<g class={classes!(if org{"individual-node"}else{"experiment-hub"},format!("node-{state}"),(self.selected.as_deref()==Some(owner)).then_some("selected"))}
            transform={format!("translate({x} {y})")} role="button" tabindex="0" aria-label={label.to_owned()}
            onclick={ctx.link().callback(move |e:MouseEvent|{e.stop_propagation();activate(&a)})}
            onkeydown={ctx.link().batch_callback(move|e:KeyboardEvent|{if matches!(e.key().as_str(),"Enter"|" "){e.prevent_default();Some(activate(&b))}else{None}})}>
            <title>{format!("{label} · {state}")}</title>
            {if org {html!{<><circle r="32" class="node-halo"/><circle r="22" class="node-shell"/><circle r="14" class="node-inner"/><text y="5" text-anchor="middle" class="node-number">{profile["slot"].as_u64().map(|n|n.to_string()).unwrap_or_else(||"◎".into())}</text><circle cx="17" cy="-16" r="5" class="node-status"/><text y="43" text-anchor="middle" class="node-label">{truncate(label,18)}</text></>}}
            else {html!{<><rect x="-62" y="-16" width="124" height="32" rx="8"/><text y="4" text-anchor="middle">{truncate(label,16)}</text></>}}}
            </g>}
        })}
        {if positions.is_empty(){html!{<text x="440" y="285" text-anchor="middle" class="empty-graph-title">{"暂无拓扑节点"}</text>}}else{Html::default()}}
        </g></svg><div class="graph-controls"><button aria-label="缩小" onclick={ctx.link().callback(|_|Msg::Zoom(-0.2))}>{"−"}</button><span>{format!("{}%",(100.*self.zoom) as u32)}</span><button aria-label="放大" onclick={ctx.link().callback(|_|Msg::Zoom(0.2))}>{"+"}</button><button aria-label="重置视图" onclick={ctx.link().callback(|_|Msg::ResetGraph)}>{"⤢"}</button></div>
        <span class="graph-hint">{format!("显示 {} / {} 个生物",group.len(),organism_ids.len())}</span></div>}
    }
    fn inspector(&self, ctx: &Context<Self>) -> Html {
        let a = self
            .selected
            .as_ref()
            .map(|id| self.agent(id))
            .unwrap_or_default();
        if a.is_null() {
            return html! {<aside class="inspector empty-inspector"><span class="eyebrow">{"ORGANISM"}</span><h2>{"选择一个生物"}</h2><p>{"点击拓扑中的生物节点查看生命状态与档案。"}</p></aside>};
        }
        let id = s(&a, "id").to_owned();
        let desktop_id = id.clone();
        html! {<aside class="inspector"><div class="inspector-heading"><span class="eyebrow">{"ORGANISM"}</span></div><h2>{display_name(&a)}</h2><code class="agent-id">{short(&id)}</code>{badge(s(&a,"state"))}<div class="inspector-divider"/>
        <div class="balance-label">{"可用能量"}</div><div class="balance-number">{amount(life(&a),"energy_available")}</div>
        <dl class="facts"><div><dt>{"剩余运行寿命"}</dt><dd>{lifetime(life(&a))}</dd></div><div><dt>{"最近心跳"}</dt><dd>{timestamp(life(&a),"last_heartbeat_at")}</dd></div></dl>
        <button class="button primary full" onclick={ctx.link().callback(move|_|Msg::Navigate(Page::Agent(id.clone())))}>{"打开生物档案"}{icon("arrow")}</button>
        <button class="button full" onclick={ctx.link().batch_callback(move|_|vec![Msg::Navigate(Page::Agent(desktop_id.clone())),Msg::Tab("desktop".into())])}>{"桌面与对话"}{icon("arrow")}</button></aside>}
    }
    fn activity(&self, ctx: &Context<Self>) -> Html {
        let events = arr(&self.snapshot, "recent_events");
        if events.is_empty() {
            return empty("尚无事件", "运行和资源事件将显示在这里。");
        }
        html! {<div class="activity-list">{for events.iter().map(|e|{let raw=e.clone();let id=e["organism_id"].as_str().unwrap_or_else(||s(e,"agent_id"));
            html!{<button class="activity-row" onclick={ctx.link().callback(move|_|Msg::Inspect("事件记录".into(),raw.clone()))}><span class="activity-icon">{icon("clock")}</span><div><strong>{event_label(s(e,"kind"))}</strong><span>{id}</span></div><time>{timestamp(e,"created_at")}</time></button>}
        })}</div>}
    }
    pub(super) fn habitats(&self, ctx: &Context<Self>) -> Html {
        let rows = arr(&self.snapshot, "environments");
        html! {<div class="page">{self.heading(ctx,"ENVIRONMENTS","生存环境","环境描述在创建后固定。进入环境后可随时创建生物。",true)}
        {if rows.is_empty(){empty("还没有环境","创建一个环境，写下生物将面对的条件。")}else{html!{<div class="experiment-grid">{for rows.iter().map(|e|{let id=s(e,"id").to_owned();
            html!{<section class="panel padded"><span class="eyebrow">{timestamp(e,"created_at")}</span><h2>{display_name(e)}</h2><p class="body-copy">{truncate(s(e,"description"),150)}</p>
                <button class="text-link" onclick={ctx.link().callback(move|_|Msg::Navigate(Page::Habitat(id.clone())))}>{"进入环境"}{icon("arrow")}</button></section>}
        })}</div>}}}</div>}
    }
    pub(super) fn habitat(&self, ctx: &Context<Self>, id: &str) -> Html {
        let e = self.exp(id);
        let target = id.to_owned();
        let members: Vec<_> = arr(&self.snapshot, "organisms")
            .iter()
            .filter(|a| s(a, "environment_id") == id)
            .cloned()
            .collect();
        html! {<div class="page"><button class="back-link" onclick={ctx.link().callback(|_|Msg::Navigate(Page::Habitats))}>{"← 生存环境"}</button>
        <div class="page-heading"><div><span class="eyebrow">{"ENVIRONMENT"}</span><h1>{display_name(&e)}</h1><code>{id}</code></div><div class="heading-actions">
          <button class="button primary" onclick={ctx.link().callback(move|_|Msg::OpenForm(Action::Birth(target.clone()),None))}>{icon("plus")}{"创建生物"}</button></div></div>
        <section class="panel padded"><h2>{"固定环境描述"}</h2><p class="body-copy">{s(&e,"description")}</p>{if !s(&e,"note").is_empty(){html!{<><h3>{"备注"}</h3><p class="body-copy">{s(&e,"note")}</p></>}}else{Html::default()}}
        <small>{format!("创建于 {} · {}", timestamp(&e,"created_at"),s(&e,"owner"))}</small></section>
        <section class="panel padded spaced"><h2>{"环境允许的模型与价格"}</h2>{self.price_table_for(e["allowed_models"].as_array().map(Vec::as_slice))}</section>
        <section class="panel spaced"><div class="panel-heading"><h2>{format!("环境中的生物 · {}",members.len())}</h2></div>{self.agent_table(ctx,&members)}</section></div>}
    }
    pub(super) fn individuals(&self, ctx: &Context<Self>) -> Html {
        html! {<div class="page">{self.heading(ctx,"ORGANISMS","生物档案","按自取名称、ID、环境或运行状态查找生物。",false)}
        <section class="panel"><div class="table-toolbar"><label class="search-input">{icon("search")}<input aria-label="搜索生物" placeholder="搜索名称或 ID" value={self.search.clone()} oninput={ctx.link().callback(|e|Msg::Search(input_value(e)))}/></label>
        {self.env_filter(ctx)}<select aria-label="筛选状态" onchange={ctx.link().callback(|e|Msg::FilterState(select_value(e)))}><option value="" selected={self.filter_state.is_empty()}>{"所有状态"}</option>
        {for arr(&self.snapshot,"organisms").iter().map(|v|s(v,"state")).collect::<BTreeSet<_>>().iter().map(|v|html!{<option value={v.to_string()} selected={&self.filter_state==v}>{state_label(v)}</option>})}</select>
        </div>{self.agent_table(ctx,&self.filtered_agents())}</section></div>}
    }
    pub(super) fn agent_table(&self, ctx: &Context<Self>, rows: &[Value]) -> Html {
        if rows.is_empty() {
            return empty("暂无生物", "可在环境中创建生物，或调整筛选条件。");
        }
        html! {<div class="table-scroll"><table><thead><tr>{for ["生物","环境","状态","可用能量","剩余寿命","最近心跳"].iter().map(|label|html!{<th>{label}</th>})}</tr></thead><tbody>{for rows.iter().map(|a|{
            let id=s(a,"id").to_owned();let e=self.exp(s(a,"environment_id"));
            html!{<tr><td><button class="agent-cell" onclick={ctx.link().callback(move|_|Msg::Navigate(Page::Agent(id.clone())))}><span><strong>{display_name(a)}</strong><code>{short(s(a,"id"))}</code></span></button></td><td>{display_name(&e)}</td><td>{badge(s(a,"state"))}</td><td>{amount(life(a),"energy_available")}</td><td>{lifetime(life(a))}</td><td>{timestamp(life(a),"last_heartbeat_at")}</td></tr>}
        })}</tbody></table></div>}
    }
}
fn node_owner(v: &Value) -> &str {
    v["organism_id"].as_str().unwrap_or_else(|| s(v, "id"))
}
fn display_label(v: &Value) -> &str {
    v["label"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| display_name(v))
}
fn truncate(v: &str, count: usize) -> String {
    let mut chars = v.chars();
    let mut out: String = chars.by_ref().take(count).collect();
    if chars.next().is_some() {
        out.push('…')
    }
    out
}
