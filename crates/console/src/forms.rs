use super::*;

/// The creator may start with a blank description or edit this example before saving.
pub const OPEN_ENVIRONMENT: &str =
    "获得经创造者确认的收入可获能量与延寿。你可以向创造者报告收入并附上凭据。";

#[derive(Clone)]
pub enum Action {
    Environment,
    Birth(String),
    Grant(String),
}

#[derive(Clone)]
pub struct Draft {
    pub action: Action,
    pub fields: Value,
    pub error: String,
    pub submitting: bool,
}

fn put_text(target: &mut Value, key: &str, fields: &Value) {
    let value = s(fields, key);
    if !value.trim().is_empty() {
        target[key] = json!(value);
    }
}

fn integer(fields: &Value, key: &str) -> Result<u64, String> {
    s(fields, key)
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{key} 需要非负整数。"))
}

fn put_model_policy(target: &mut Value, fields: &Value) -> Result<(), String> {
    if let Some(ids) = fields["allowed_models"].as_array() {
        if ids.is_empty() {
            return Err("请至少选择一个允许的模型，或恢复继承的模型范围。".into());
        }
        target["allowed_models"] = json!(ids);
    }
    Ok(())
}

fn check_default(default: &str, permitted: &Value) -> Result<(), String> {
    if !default.is_empty()
        && permitted
            .as_array()
            .is_some_and(|ids| !ids.iter().any(|v| v == default))
    {
        return Err(format!("默认模型 {default} 未包含在所选模型范围内。"));
    }
    Ok(())
}

impl Draft {
    pub fn new(action: Action) -> Self {
        let mut fields = json!({});
        match &action {
            Action::Environment => {}
            Action::Birth(_) => {
                fields = json!({
                    "count":"1","initial_energy":"50000","lifetime_seconds":"3600","default_model":"",
                    "memory":"768","cpu_millis":"1000","pids":"384","internet":true,"peers":true,"services":true,
                    "selected_slot":"1", "overrides":{}
                })
            }
            Action::Grant(_) => {
                // A new decision gets its own key. Retries within this form reuse the same key.
                fields["idempotency_key"] = json!(format!(
                    "console-{}-{:x}",
                    js_sys::Date::now() as u64,
                    js_sys::Math::random().to_bits()
                ));
            }
        }
        Self {
            action,
            fields,
            submitting: false,
            error: String::new(),
        }
    }
    pub fn url(&self) -> String {
        match &self.action {
            Action::Environment => "/api/v2/environments".into(),
            Action::Birth(id) => format!("/api/v2/environments/{id}/organisms"),
            Action::Grant(id) => format!("/api/v2/organisms/{id}/grants"),
        }
    }
    pub fn payload(&self) -> Result<Value, String> {
        let f = &self.fields;
        match &self.action {
            Action::Environment => {
                if s(f, "description").trim().is_empty() {
                    return Err("请填写固定环境描述。".into());
                }
                let mut v = json!({"description":s(f,"description")});
                for k in ["name", "note"] {
                    put_text(&mut v, k, f);
                }
                put_model_policy(&mut v, f)?;
                Ok(v)
            }
            Action::Grant(_) => {
                if s(f, "idempotency_key").trim().is_empty() {
                    return Err("请填写幂等键。".into());
                }
                let mut v = json!({"idempotency_key":s(f,"idempotency_key")});
                for k in ["energy", "lifetime_seconds"] {
                    if !s(f, k).trim().is_empty() {
                        v[k] = json!(integer(f, k)?);
                    }
                }
                if n(&v, "energy") == 0 && n(&v, "lifetime_seconds") == 0 {
                    return Err("能量或延寿秒数至少有一项大于零。".into());
                }
                for k in ["note", "report_id"] {
                    put_text(&mut v, k, f);
                }
                Ok(v)
            }
            Action::Birth(_) => {
                let count = integer(f, "count")?;
                let energy = integer(f, "initial_energy")?;
                let time = integer(f, "lifetime_seconds")?;
                if !(1..=1000).contains(&count) || energy == 0 || time == 0 {
                    return Err("数量需在 1 至 1000 之间，能量和寿命都需要大于零。".into());
                }
                let memory = integer(f, "memory")?
                    .checked_mul(1024 * 1024)
                    .ok_or("内存值超出范围。")?;
                let mut defaults = json!({
                    "initial_energy":energy, "lifetime_seconds":time,
                    "limits":{"memory_bytes":memory,"cpu_millis":integer(f,"cpu_millis")?,"pids":integer(f,"pids")?},
                    "network":{"internet":f["internet"],"peers":f["peers"],"services":f["services"]}
                });
                // Whitespace and line breaks are meaningful to the organism's DNA.
                if !s(f, "dna").is_empty() {
                    defaults["dna"] = json!(s(f, "dna"));
                }
                put_text(&mut defaults, "default_model", f);
                put_model_policy(&mut defaults, f)?;
                check_default(s(f, "default_model"), &defaults["allowed_models"])?;
                let overrides = birth_overrides(count, &defaults, &f["overrides"])?;
                Ok(json!({"count":count,"defaults":defaults,"overrides":overrides}))
            }
        }
    }
}

// When an individual excludes the batch's explicit default, its first permitted
// model is the visible fallback. A blank batch default is resolved by the server.
fn slot_fallback<'a>(batch_default: &str, slot_policy: &'a Value) -> Option<&'a str> {
    let ids = slot_policy.as_array()?;
    if batch_default.is_empty() || ids.iter().any(|id| id == batch_default) {
        return None;
    }
    ids.first()?.as_str()
}

fn birth_overrides(count: u64, defaults: &Value, edits: &Value) -> Result<Vec<Value>, String> {
    let mut overrides = Vec::with_capacity(count as usize);
    let mut changed = false;
    for slot in 1..=count {
        let fields = &edits[slot.to_string()];
        let mut row = json!({});
        for key in ["initial_energy", "lifetime_seconds"] {
            if !s(fields, key).is_empty() {
                row[key] = json!(integer(fields, key)?);
            }
        }
        if s(fields, "dna_mode") == "custom" {
            row["dna"] = json!(s(fields, "dna"));
        }
        put_text(&mut row, "default_model", fields);
        put_model_policy(&mut row, fields)
            .map_err(|message| format!("第 {slot} 个生物：{message}"))?;
        if row["default_model"].is_null()
            && let Some(model) = slot_fallback(s(defaults, "default_model"), &row["allowed_models"])
        {
            row["default_model"] = json!(model);
        }
        let preferred = row["default_model"]
            .as_str()
            .unwrap_or_else(|| s(defaults, "default_model"));
        let policy = row
            .get("allowed_models")
            .unwrap_or(&defaults["allowed_models"]);
        check_default(preferred, policy)
            .map_err(|message| format!("第 {slot} 个生物：{message}"))?;
        for key in ["memory", "cpu_millis", "pids"] {
            if !s(fields, key).is_empty() {
                if row["limits"].is_null() {
                    row["limits"] = defaults["limits"].clone();
                }
                let number = integer(fields, key)?;
                if key == "memory" {
                    row["limits"]["memory_bytes"] =
                        json!(number.checked_mul(1024 * 1024).ok_or("内存值超出范围。")?);
                } else {
                    row["limits"][key] = json!(number);
                }
            }
        }
        for key in ["internet", "peers", "services"] {
            if !s(fields, key).is_empty() {
                if row["network"].is_null() {
                    row["network"] = defaults["network"].clone();
                }
                row["network"][key] = json!(s(fields, key) == "true");
            }
        }
        changed |= row.as_object().is_some_and(|o| !o.is_empty());
        overrides.push(row);
    }
    if !changed {
        overrides.clear();
    }
    Ok(overrides)
}

impl App {
    fn field(
        &self,
        ctx: &Context<Self>,
        d: &Draft,
        key: &'static str,
        label: &str,
        kind: &str,
        hint: &str,
    ) -> Html {
        let value = s(&d.fields, key).to_string();
        html! {<label class="field"><span>{label}</span>
            {if kind=="textarea"{html!{<textarea rows={if key=="description"||key=="overrides"{"5"}else{"3"}} value={value} oninput={ctx.link().callback(move|e|Msg::Draft(key.into(),textarea_value(e)))}/>}}
            else{html!{<input type={kind.to_string()} min="0" step="1" value={value} oninput={ctx.link().callback(move|e|Msg::Draft(key.into(),input_value(e)))}/>}}}
            {if !hint.is_empty(){html!{<small>{hint}</small>}}else{Html::default()}}
        </label>}
    }
    fn model_editor(&self, ctx: &Context<Self>, draft: &Draft, slot: Option<u64>) -> Html {
        let environment = match &draft.action {
            Action::Birth(id) => self.exp(id),
            _ => Value::Null,
        };
        let fields = slot.map_or(&draft.fields, |n| &draft.fields["overrides"][n.to_string()]);
        let configured = fields["allowed_models"].as_array();
        let inherited = if slot.is_some() {
            draft.fields["allowed_models"].as_array()
        } else {
            None
        };
        let effective = configured.or(inherited);
        let models = arr(&self.snapshot["system"], "models")
            .iter()
            .filter(|m| {
                environment["allowed_models"]
                    .as_array()
                    .is_none_or(|ids| ids.contains(&m["id"]))
            })
            .collect::<Vec<_>>();
        let current: Vec<String> = models
            .iter()
            .filter(|m| effective.is_none_or(|ids| ids.contains(&m["id"])))
            .map(|m| s(m, "id").to_owned())
            .collect();
        let inherit_label = match (&draft.action, slot) {
            (Action::Environment, _) => "使用平台全部模型",
            (_, Some(_)) => "沿用批次模型范围",
            _ => "使用环境全部模型",
        };
        let fallback = if slot.is_some()
            && let Some(model) =
                slot_fallback(s(&draft.fields, "default_model"), &fields["allowed_models"])
        {
            format!("自动选择（{model}）")
        } else if slot.is_some() && !s(&draft.fields, "default_model").is_empty() {
            format!("沿用批次默认模型（{}）", s(&draft.fields, "default_model"))
        } else {
            "自动选择可用模型".to_owned()
        };
        html! {<div class="model-editor"><div class="model-editor-heading"><strong>{"允许使用的模型"}</strong>
            <button type="button" class="text-link" disabled={configured.is_none()} onclick={ctx.link().callback(move|_|Msg::ModelSelection(slot,None))}>{inherit_label}</button></div>
            {if models.is_empty(){html!{<p class="muted">{"暂无已配置的模型。"}</p>}}else{Html::default()}}
            <div class="model-options">{for models.iter().map(|m|{
                let id=s(m,"id").to_owned();
                let selected=current.contains(&id);
                let mut chosen=current.clone();
                if selected{chosen.retain(|v|v!=&id);}else{chosen.push(id.clone());}
                html!{<label class="model-option"><input type="checkbox" checked={selected}
                    onchange={ctx.link().callback(move|_|Msg::ModelSelection(slot,Some(chosen.clone())))}/>
                    <span><b>{id}</b><small>{format!("输入 {} · 输出 {} 能量 / token",amount(m,"input"),amount(m,"output"))}</small></span>
                    {if m["available"]==false {html!{<small>{"线路未就绪"}</small>}}else{Html::default()}}
                </label>}
            })}</div>
            {if matches!(draft.action, Action::Birth(_)) {
                html!{<label class="field"><span>{if slot.is_some(){"此生物的出生默认模型"}else{"出生默认模型"}}</span>
                    <select onchange={ctx.link().callback(move|e| if let Some(n)=slot { Msg::Override(n,"default_model",select_value(e)) }
                        else {Msg::Draft("default_model".into(),select_value(e))})}>
                    <option value="" selected={s(fields,"default_model").is_empty()}>{fallback}</option>
                    {for models.iter().filter(|m|current.iter().any(|id|id==s(m,"id"))).map(|m|html!{
                        <option value={s(m,"id").to_owned()} selected={m["id"]==fields["default_model"]}>{s(m,"id")}</option>
                    })}</select><small>{"出生后，生物可在此范围内通过 /work/model.txt 自主切换模型。"}</small></label>}
            }else{Html::default()}}
        </div>}
    }
    fn overrides_editor(&self, ctx: &Context<Self>, d: &Draft) -> Html {
        let count = integer(&d.fields, "count").unwrap_or_default().min(1000);
        if count == 0 {
            return Html::default();
        }
        let slot = integer(&d.fields, "selected_slot")
            .unwrap_or(1)
            .clamp(1, count);
        let overrides = &d.fields["overrides"][slot.to_string()];
        let set = |key: &'static str| {
            ctx.link()
                .callback(move |e| Msg::Override(slot, key, input_value(e)))
        };
        let select = |key: &'static str| {
            ctx.link()
                .callback(move |e| Msg::Override(slot, key, select_value(e)))
        };
        let field = |key: &'static str, label, kind| html! {<label class="field"><span>{label}</span><input type={kind} min="0" value={s(overrides,key).to_owned()} placeholder="沿用默认值" oninput={set(key)}/></label>};
        html! {<details class="advanced override-editor"><summary>{"逐个生物覆盖"}</summary>
         <div><label class="field"><span>{"选择本批次中的生物"}</span><select onchange={ctx.link().callback(|e|Msg::Draft("selected_slot".into(),select_value(e)))}>
         {for (1..=count).map(|n|html!{<option value={n.to_string()} selected={n==slot}>{format!("第 {n} 个生物")}</option>})}</select></label>
         <div class="form-grid">{field("initial_energy","覆盖初始能量","number")}{field("lifetime_seconds","覆盖寿命（秒）","number")}</div>
         <label class="field"><span>{"DNA 设置"}</span><select onchange={select("dna_mode")}><option value="" selected={s(overrides,"dna_mode")!="custom"}>{"沿用默认 DNA"}</option><option value="custom" selected={s(overrides,"dna_mode")=="custom"}>{"独立设置（可为空）"}</option></select></label>
         {if s(overrides,"dna_mode")=="custom"{html!{<label class="field"><span>{"当前生物的初始 DNA"}</span><textarea rows="3" value={s(overrides,"dna").to_owned()} oninput={ctx.link().callback(move|e|Msg::Override(slot,"dna",textarea_value(e)))}/></label>}}else{Html::default()}}
         {self.model_editor(ctx, d, Some(slot))}
         <div class="form-grid thirds">{field("memory","覆盖内存（MiB）","number")}{field("cpu_millis","覆盖 CPU（毫核）","number")}{field("pids","覆盖进程数","number")}</div>
         <div class="form-grid thirds">{for [("internet","互联网策略"),("peers","同环境通信策略"),("services","服务发布策略")].map(|(key,label)|html!{<label class="field"><span>{label}</span>
             <select onchange={select(key)}>{for [("","沿用默认值"),("true","允许"),("false","关闭")].map(|(val,label)|html!{<option value={val} selected={s(overrides,key)==val}>{label}</option>})}</select></label>})}</div>
        </div></details>}
    }
    pub(super) fn wizard_view(&self, ctx: &Context<Self>, d: &Draft) -> Html {
        let field = |key, label, kind, hint| self.field(ctx, d, key, label, kind, hint);
        let (title, body, submit) = match &d.action {
            Action::Environment => (
                "创建环境",
                html! {<>
                    {field("name","环境名称（可选）","text","")}
                    <div class="template-toolbar"><button class="text-link" disabled={d.submitting} onclick={ctx.link().callback(|_|Msg::Draft("description".into(),OPEN_ENVIRONMENT.into()))}>{"使用开放环境模板"}</button></div>
                    {field("description","固定环境描述 · 必填","textarea","描述生物可以做什么，以及你会如何回应报告。保存后固定，创建时环境为空。")}
                    {field("note","备注（可选）","textarea","")}{self.model_editor(ctx,d,None)}
                </>},
                "创建环境",
            ),
            Action::Birth(eid) => (
                "创建生物",
                html! {<>
                    <p class="form-context">{format!("环境 · {}",display_name(&self.exp(eid)))}</p>
                    <div class="form-grid thirds">{field("count","生物数量","number","")}{field("initial_energy","每个生物的初始能量","number","")}{field("lifetime_seconds","运行寿命（秒）","number","仅沙盒运行时消耗。")}</div>
                    {field("dna","初始 DNA（可选）","textarea","生物可在出生后修改 /work/dna.md。")}
                    {self.model_editor(ctx,d,None)}
                    <h3>{"每个沙盒的资源限制"}</h3>
                    <div class="form-grid thirds">{field("memory","内存（MiB）","number","")}{field("cpu_millis","CPU（毫核）","number","")}{field("pids","最大进程数","number","")}</div>
                    <div class="permission-grid">{for [("internet","互联网"),("peers","同环境通信"),("services","发布服务")].map(|(key,label)|html!{<label class="checkbox-card"><input type="checkbox" checked={d.fields[key].as_bool().unwrap_or(false)} onchange={ctx.link().callback(move|e:Event|Msg::DraftBool(key.into(),e.target_unchecked_into::<HtmlInputElement>().checked()))}/>{label}</label>})}</div>
                    {self.overrides_editor(ctx,d)}
                </>},
                "创建生物",
            ),
            Action::Grant(id) => (
                "发放资源",
                html! {<>
                    <p class="form-context">{format!("接收生物 · {} ({})",display_name(&self.agent(id)),id)}</p>
                    <div class="form-grid">{field("energy","增加能量","number","")}{field("lifetime_seconds","增加运行寿命（秒）","number","")}</div>
                    {field("note","发放备注（可选）","textarea","")}
                    {field("report_id","关联报告 ID（可选）","text","从报告发放时自动填入。")}
                    {field("idempotency_key","幂等键","text","每笔发放使用一个键。请求结果不确定时，用同一键及相同内容重试。")}
                </>},
                "确认发放",
            ),
        };
        html! {<div class="modal-backdrop"><section class="modal wizard" role="dialog" aria-modal="true" aria-label={title}
          onkeydown={ctx.link().batch_callback(|e:KeyboardEvent|{trap_focus(&e);(e.key()=="Escape").then_some(Msg::CloseWizard)})}>
          <div class="modal-heading"><h2>{title}</h2><button class="icon-button" disabled={d.submitting} aria-label="关闭" onclick={ctx.link().callback(|_|Msg::CloseWizard)}>{icon("close")}</button></div>
          <div class="wizard-body"><fieldset disabled={d.submitting}>{body}</fieldset>
            {if !d.error.is_empty(){ html!{<p class="inline-error" role="alert">{&d.error}</p>} }else{Html::default()}}
          </div><div class="wizard-footer"><span>{if self.demo{"演示模式 · 仅预览表单"}else{""}}</span><div>
          <button class="button secondary" disabled={d.submitting} onclick={ctx.link().callback(|_|Msg::CloseWizard)}>{"取消"}</button>
          <button class="button primary" disabled={self.demo||d.submitting} onclick={ctx.link().callback(|_|Msg::Submit)}>{if d.submitting{"提交中…"}else{submit}}</button></div></div>
        </section></div>}
    }
}
