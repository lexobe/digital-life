use base64::Engine;
use gloo_net::http::Request;
use gloo_timers::callback::Interval;
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;
use web_sys::{HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement};
use yew::prelude::*;

mod demo;

#[derive(Clone, PartialEq)]
enum Page {
    Observe,
    Habitats,
    Individuals,
    Environment,
    Api,
    Habitat(String),
    Agent(String),
}
mod dashboard;
mod desktop;
mod dialogue;
mod forms;
mod organism;
mod preview;
mod preview_document;
use forms::{Action, Draft};
use organism::life;
use preview::{Content, Preview};

struct App {
    epoch: u64,
    refresh_request: u64,
    token: Option<String>,
    token_input: String,
    snapshot: Value,
    topology: Value,
    page: Page,
    selected: Option<String>,
    tab: String,
    detail: Value,
    detail_loading: bool,
    detail_error: Option<String>,
    page_cursors: Vec<Option<String>>,
    service_preview: Option<Preview>,
    preview_request: u64,
    event_chunk: Option<(String, u64, u64)>,
    detail_request: u64,
    filter_exp: String,
    filter_state: String,
    search: String,
    file_path: String,
    file_offset: u64,
    loading: bool,
    error: String,
    demo: bool,
    wizard: Option<Draft>,
    inspect: Option<(String, Value)>,
    zoom: f64,
    pan: (f64, f64),
    drag: Option<(f64, f64)>,
    mobile_nav: bool,
    focus_target: Option<String>,
    restore_focus: Option<web_sys::HtmlElement>,
    focus_restore: bool,
    _interval: Interval,
}
enum Msg {
    ForSession(u64, Box<Msg>),
    Token(String),
    Login,
    Logout,
    Unauthorized,
    Refresh,
    ForRefresh(u64, Box<Msg>),
    Snapshot(Result<Value, String>),
    Topology(Result<Value, String>),
    Navigate(Page),
    Select(String),
    Tab(String),
    Detail(u64, String, String, Result<Value, String>),
    EventChunk(String, u64),
    EventLoaded(String, Result<Value, String>),
    FilterExp(String),
    FilterState(String),
    Search(String),
    File(String, u64),
    Dna,
    RetryDetail,
    NextPage,
    PreviousPage,
    PreviewService(String, u16),
    PreviewLoaded(u64, Result<preview_document::Document, String>),
    ClosePreview,
    Zoom(f64),
    ResetGraph,
    DragStart(f64, f64),
    DragMove(f64, f64),
    DragEnd,
    OpenForm(Action, Option<String>),
    CloseWizard,
    Draft(String, String),
    DraftBool(String, bool),
    ModelSelection(Option<u64>, Option<Vec<String>>),
    Override(u64, &'static str, String),
    Submit,
    Created(Result<Value, String>),
    Inspect(String, Value),
    FetchReport(String, String),
    Inspected(String, Result<Value, String>),
    CloseInspect,
    ToggleNav,
    DismissError,
}
fn send_scoped(
    ctx: &Context<App>,
    epoch: u64,
    future: impl std::future::Future<Output = Msg> + 'static,
) {
    ctx.link()
        .send_future(async move { Msg::ForSession(epoch, Box::new(future.await)) });
}
async fn api(token: &str, url: &str) -> Result<Value, String> {
    let r = Request::get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|_| "无法连接服务器，请检查网络后重试。".to_string())?;
    let status = r.status();
    let v: Value = r
        .json()
        .await
        .map_err(|_| "服务器返回了无法读取的响应。".to_string())?;
    if status == 401 {
        return Err("401:凭据无效或已失效，请重新连接。".into());
    }
    if !(200..300).contains(&status) {
        return Err(v["error"]["message"]
            .as_str()
            .unwrap_or("请求失败")
            .to_string());
    }
    Ok(v)
}
fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn display_name(v: &Value) -> &str {
    v["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| s(v, "id"))
}
fn amount(v: &Value, key: &str) -> String {
    v[key].as_u64().map(num).unwrap_or_else(|| "—".into())
}
fn timestamp(v: &Value, key: &str) -> String {
    v[key].as_u64().map(date).unwrap_or_else(|| "—".into())
}
fn lifetime(v: &Value) -> String {
    let Some(ms) = v["remaining_lifetime_ms"].as_u64() else {
        return "—".into();
    };
    let seconds = ms.div_ceil(1000);
    format!(
        "{}h {:02}m {:02}s",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}
fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}
fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn num(v: u64) -> String {
    let raw = v.to_string();
    let len = raw.len();
    raw.chars()
        .enumerate()
        .fold(String::new(), |mut out, (i, c)| {
            if i > 0 && (len - i).is_multiple_of(3) {
                out.push(',')
            }
            out.push(c);
            out
        })
}
fn short(v: &str) -> String {
    let chars: Vec<_> = v.chars().collect();
    if chars.len() > 14 {
        format!(
            "{}…{}",
            chars[..6].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        v.into()
    }
}
fn state_label(v: &str) -> &str {
    match v {
        "running" => "运行中",
        "queued" => "排队中",
        "starting" => "启动中",
        "depleted" => "资源耗尽",
        "failed" => "执行失败",
        "suspended" => "已挂起",
        "expired" => "已过期",
        "settled" => "已结算",
        "reserved" => "预留中",
        "unknown" => "待对账",
        "released" => "已释放",
        _ => v,
    }
}
fn badge(v: &str) -> Html {
    html! {<span class={classes!("badge",format!("state-{v}"))}><i/>{state_label(v)}</span>}
}
fn date(v: u64) -> String {
    if v == 0 {
        return "—".into();
    }
    let millis = if v < 10_000_000_000 {
        v.saturating_mul(1000)
    } else {
        v
    };
    let d = js_sys::Date::new(&JsValue::from_f64(millis as f64));
    format!(
        "{:02}-{:02} {:02}:{:02}:{:02}",
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds()
    )
}
fn document_hidden() -> bool {
    web_sys::window()
        .unwrap_throw()
        .document()
        .unwrap_throw()
        .hidden()
}
fn input_value(e: InputEvent) -> String {
    e.target_unchecked_into::<HtmlInputElement>().value()
}
fn textarea_value(e: InputEvent) -> String {
    e.target_unchecked_into::<HtmlTextAreaElement>().value()
}
fn select_value(e: Event) -> String {
    e.target_unchecked_into::<HtmlSelectElement>().value()
}
fn encoded(v: &str) -> String {
    js_sys::encode_uri_component(v)
        .as_string()
        .unwrap_or_default()
}
fn icon(name: &str) -> Html {
    let d = match name {
        "network" => {
            "M12 8V4m0 16v-4M8 12H4m16 0h-4M8.5 8.5l-3-3m13 13-3-3m-7 0-3 3m13-13-3 3M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0Z"
        }
        "experiment" => "M9 3h6m-5 0v6l-6 10a1 1 0 0 0 1 2h14a1 1 0 0 0 1-2L14 9V3M8 15h8",
        "agents" => {
            "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2m20 0v-2a4 4 0 0 0-3-3.87M13 3a4 4 0 0 1 0 8M13 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0Z"
        }
        "environment" => "M4 4h16v6H4zM4 14h16v6H4zM7 7h.01M7 17h.01M11 7h6m-6 10h6",
        "code" => "m8 7-5 5 5 5m8-10 5 5-5 5m-3-14-2 18",
        "arrow" => "M5 12h14m-5-5 5 5-5 5",
        "plus" => "M12 5v14M5 12h14",
        "search" => "M21 21l-5-5M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0Z",
        "refresh" => "M20 7v5h-5M4 17v-5h5M5 7a8 8 0 0 1 13-2l2 3M4 16l2 3a8 8 0 0 0 13-2",
        "close" => "m6 6 12 12M6 18 18 6",
        "check" => "m5 12 4 4L19 6",
        "file" => "M14 2H6v20h12V6l-4-4Zm0 0v6h4M8 12h8m-8 4h8",
        "clock" => "M12 8v5l3 2M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0Z",
        "external" => "M14 3h7v7m0-7L10 14m1-11H3v18h18v-8",
        "lock" => "M6 10V7a6 6 0 0 1 12 0v3M4 10h16v12H4zM12 14v4",
        "menu" => "M4 6h16M4 12h16M4 18h16",
        _ => "M12 5v14M5 12h14",
    };
    html! {<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d={d.to_string()}/></svg>}
}
impl Component for App {
    type Message = Msg;
    type Properties = ();
    fn create(ctx: &Context<Self>) -> Self {
        let demo = web_sys::window()
            .and_then(|w| w.location().hash().ok())
            .is_some_and(|h| h == "#demo");
        let link = ctx.link().clone();
        let interval = Interval::new(8000, move || link.send_message(Msg::Refresh));
        let snapshot = if demo { demo::overview() } else { Value::Null };
        Self {
            epoch: 0,
            refresh_request: 0,
            token: None,
            token_input: String::new(),
            snapshot: snapshot.clone(),
            topology: if demo {
                demo::topology(&snapshot)
            } else {
                Value::Null
            },
            page: Page::Observe,
            selected: None,
            tab: "overview".into(),
            detail: Value::Null,
            detail_loading: false,
            detail_error: None,
            page_cursors: vec![None],
            service_preview: None,
            preview_request: 0,
            event_chunk: None,
            detail_request: 0,
            filter_exp: String::new(),
            filter_state: String::new(),
            search: String::new(),
            file_path: String::new(),
            file_offset: 0,
            loading: false,
            error: String::new(),
            demo,
            wizard: None,
            inspect: None,
            zoom: 1.,
            pan: (0., 0.),
            drag: None,
            mobile_nav: false,
            focus_target: None,
            restore_focus: None,
            focus_restore: false,
            _interval: interval,
        }
    }
    fn update(&mut self, ctx: &Context<Self>, msg: Msg) -> bool {
        match msg {
            Msg::ForSession(epoch, message) => {
                if epoch != self.epoch {
                    return false;
                }
                return <Self as Component>::update(self, ctx, *message);
            }
            Msg::ForRefresh(request, msg) => {
                if request != self.refresh_request {
                    return false;
                }
                return <Self as Component>::update(self, ctx, *msg);
            }
            Msg::Token(v) => self.token_input = v,
            Msg::Login => {
                if self.token_input.trim().is_empty() {
                    self.error = "请输入创建者访问凭据。".into()
                } else {
                    self.epoch += 1;
                    self.token = Some(self.token_input.trim().into());
                    self.token_input.clear();
                    self.error.clear();
                    self.fetch(ctx);
                }
            }
            Msg::Logout => {
                self.loading = false;
                self.detail_loading = false;
                self.epoch += 1;
                self.detail_request += 1;
                self.token = None;
                self.snapshot = Value::Null;
                self.topology = Value::Null;
                self.detail = Value::Null;
                self.inspect = None;
                self.service_preview = None;
                self.wizard = None;
                self.selected = None;
                self.page = Page::Observe;
                self.demo = false;
                self.filter_exp.clear();
                self.filter_state.clear();
                self.search.clear();
                self.error.clear();
                if let Some(w) = web_sys::window() {
                    let _ = w.location().set_hash("");
                }
            }
            Msg::Unauthorized => self.report_error("401:凭据已失效，请重新连接。".into()),
            Msg::Refresh => {
                if !self.loading && !self.demo && self.token.is_some() && !document_hidden() {
                    self.fetch(ctx);
                } else {
                    return false;
                }
            }
            Msg::Snapshot(result) => {
                self.loading = false;
                match result {
                    Ok(v) => {
                        self.snapshot = v;
                        self.error.clear()
                    }
                    Err(e) => {
                        self.report_error(e);
                    }
                }
            }
            Msg::Topology(r) => match r {
                Ok(v) => self.topology = v,
                Err(e) => self.report_error(e),
            },
            Msg::Navigate(p) => {
                self.service_preview = None;
                self.event_chunk = None;
                self.detail_loading = false;
                self.detail_error = None;
                self.detail_request += 1;
                self.page = p.clone();
                self.mobile_nav = false;
                self.search.clear();
                self.filter_state.clear();
                if let Page::Agent(id) = p {
                    self.selected = Some(id);
                    self.tab = "overview".into();
                    self.detail = Value::Null;
                }
            }
            Msg::Select(id) => self.selected = Some(id),
            Msg::Tab(tab) => {
                self.service_preview = None;
                self.event_chunk = None;
                self.tab = tab;
                self.page_cursors = vec![None];
                self.detail = Value::Null;
                self.file_path.clear();
                self.file_offset = 0;
                self.fetch_detail(ctx);
            }
            Msg::Detail(request, id, tab, r) => {
                if request != self.detail_request {
                    return false;
                }
                if self.selected.as_ref() != Some(&id) || self.tab != tab {
                    return false;
                }
                self.detail_loading = false;
                match r {
                    Ok(v) => self.detail = v,
                    Err(e) if e.starts_with("401:") => self.report_error(e),
                    Err(e) => self.detail_error = Some(e),
                }
            }
            Msg::RetryDetail => self.fetch_detail(ctx),
            Msg::NextPage => {
                if let Some(cursor) = self.detail["next_cursor"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                {
                    self.page_cursors.push(Some(cursor.to_string()));
                    self.fetch_detail(ctx);
                }
            }
            Msg::PreviousPage => {
                if self.page_cursors.len() > 1 {
                    self.page_cursors.pop();
                    self.fetch_detail(ctx);
                }
            }
            Msg::EventChunk(id, offset) => {
                if self.demo {
                    self.inspect = Some((
                        "完整事件记录".into(),
                        json!({"id":id,"data":{"note":"演示记录"}}),
                    ));
                } else if let Some(agent) = &self.selected {
                    let token = self.token.clone().unwrap_or_default();
                    let url = format!(
                        "/api/v2/organisms/{agent}/events/{id}?offset={offset}&limit=65536"
                    );
                    send_scoped(ctx, self.epoch, async move {
                        Msg::EventLoaded(id, api(&token, &url).await)
                    });
                }
            }
            Msg::EventLoaded(id, result) => match result {
                Ok(v) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(s(&v, "data"))
                        .unwrap_or_default();
                    self.service_preview = Some(Preview {
                        title: "完整事件记录".into(),
                        content: Content::Event(String::from_utf8_lossy(&bytes).to_string()),
                    });
                    self.event_chunk = Some((id, n(&v, "next_offset"), n(&v, "total_bytes")));
                }
                Err(e) => self.report_error(e),
            },
            Msg::PreviewService(name, port) => self.open_preview(ctx, name, port),
            Msg::PreviewLoaded(request, result) => {
                let Some(preview) = &mut self.service_preview else {
                    return false;
                };
                if self.preview_request != request
                    || !matches!(preview.content, Content::Loading(_))
                {
                    return false;
                }
                preview.content = match result {
                    Ok(doc) => Content::Service(doc),
                    Err(err) => Content::Error(err),
                };
            }
            Msg::ClosePreview => {
                self.focus_restore = true;
                self.service_preview = None;
                self.event_chunk = None;
            }
            Msg::FilterExp(v) => {
                self.filter_exp = v;
                self.selected = None;
                self.zoom = 1.;
                self.pan = (0., 0.);
            }
            Msg::FilterState(v) => self.filter_state = v,
            Msg::Search(v) => self.search = v,
            Msg::Dna => {
                self.tab = "files".into();
                self.file_path = "dna.md".into();
                self.file_offset = 0;
                self.fetch_detail(ctx);
            }
            Msg::File(path, offset) => {
                self.file_path = path;
                self.file_offset = offset;
                self.fetch_detail(ctx);
            }
            Msg::Zoom(d) => self.zoom = (self.zoom + d).clamp(0.5, 2.5),
            Msg::ResetGraph => {
                self.zoom = 1.;
                self.pan = (0., 0.);
            }
            Msg::DragStart(x, y) => self.drag = Some((x, y)),
            Msg::DragMove(x, y) => {
                if let Some((px, py)) = self.drag {
                    self.pan.0 += x - px;
                    self.pan.1 += y - py;
                    self.drag = Some((x, y));
                } else {
                    return false;
                }
            }
            Msg::DragEnd => self.drag = None,
            Msg::OpenForm(action, report) => {
                self.remember_focus();
                self.focus_target = Some(".wizard input".into());
                let mut d = Draft::new(action);
                if let Some(id) = report {
                    d.fields["report_id"] = json!(id);
                }
                self.wizard = Some(d);
            }
            Msg::CloseWizard => {
                if !self.wizard.as_ref().is_some_and(|d| d.submitting) {
                    self.wizard = None;
                    self.focus_restore = true;
                }
            }
            Msg::Draft(key, value) => {
                if let Some(d) = self.wizard.as_mut().filter(|d| !d.submitting) {
                    d.fields[&key] = json!(value);
                    d.error.clear();
                }
            }
            Msg::Override(slot, key, value) => {
                if let Some(d) = self.wizard.as_mut().filter(|d| !d.submitting) {
                    d.fields["overrides"][slot.to_string()][key] = json!(value);
                    d.error.clear();
                }
            }
            Msg::ModelSelection(slot, ids) => {
                if let Some(draft) = self.wizard.as_mut().filter(|d| !d.submitting) {
                    let inherited = draft.fields["allowed_models"].clone();
                    let fields = match slot {
                        Some(n) => &mut draft.fields["overrides"][n.to_string()],
                        None => &mut draft.fields,
                    };
                    fields["allowed_models"] = json!(ids);
                    let effective = ids
                        .as_ref()
                        .map(|_| &fields["allowed_models"])
                        .or_else(|| slot.map(|_| &inherited));
                    if effective
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.contains(&fields["default_model"]))
                    {
                        fields["default_model"] = json!("");
                    }
                    draft.error.clear();
                }
            }
            Msg::DraftBool(key, value) => {
                if let Some(d) = self.wizard.as_mut().filter(|d| !d.submitting) {
                    d.fields[&key] = json!(value);
                    d.error.clear();
                }
            }
            Msg::Submit => {
                if self.demo {
                    return false;
                }
                let Some(d) = self.wizard.as_mut().filter(|d| !d.submitting) else {
                    return false;
                };
                let payload = match d.payload() {
                    Ok(v) => v,
                    Err(e) => {
                        d.error = e;
                        return true;
                    }
                };
                let Some(token) = self.token.clone() else {
                    return false;
                };
                d.submitting = true;
                let url = d.url();
                send_scoped(ctx, self.epoch, async move {
                    let result = async {
                        let r = Request::post(&url)
                            .header("Authorization", &format!("Bearer {token}"))
                            .json(&payload)
                            .map_err(|e| e.to_string())?
                            .send()
                            .await
                            .map_err(|e| e.to_string())?;
                        let status = r.status();
                        let v: Value = r.json().await.map_err(|e| e.to_string())?;
                        if status == 401 {
                            return Err("401:凭据已失效，请重新连接。".to_string());
                        }
                        if !(200..300).contains(&status) {
                            return Err(v["error"]["message"]
                                .as_str()
                                .unwrap_or("请求失败")
                                .to_string());
                        }
                        Ok(v)
                    }
                    .await;
                    Msg::Created(result)
                });
            }
            Msg::Created(result) => match result {
                Ok(response) => {
                    let Some(d) = self.wizard.take() else {
                        return false;
                    };
                    match d.action {
                        Action::Environment => {
                            self.page = Page::Habitat(s(&response["environment"], "id").into())
                        }
                        Action::Birth(id) => self.page = Page::Habitat(id),
                        Action::Grant(id) => {
                            self.page = Page::Agent(id.clone());
                            self.selected = Some(id);
                            self.tab = "grants".into();
                            self.page_cursors = vec![None];
                            self.fetch_detail(ctx);
                        }
                    }
                    self.fetch(ctx);
                }
                Err(e) if e.starts_with("401:") => self.report_error(e),
                Err(e) => {
                    if let Some(d) = self.wizard.as_mut() {
                        d.submitting = false;
                        d.error = e
                    }
                }
            },
            Msg::Inspect(title, v) => {
                self.remember_focus();
                self.focus_target = Some(".inspect-modal button".into());
                self.inspect = Some((title, v));
            }
            Msg::FetchReport(id, organism) => {
                let title = "报告与提交时快照".to_string();
                let url = format!("/api/v2/organisms/{organism}/reports/{id}");
                self.remember_focus();
                self.focus_target = Some(".inspect-modal button".into());
                if self.demo {
                    self.inspect = Some((title, demo::report(&organism)))
                } else {
                    let token = self.token.clone().unwrap_or_default();
                    send_scoped(ctx, self.epoch, async move {
                        Msg::Inspected(title, api(&token, &url).await)
                    });
                }
            }
            Msg::Inspected(title, result) => match result {
                Ok(v) => {
                    self.focus_target = Some(".inspect-modal button".into());
                    self.inspect = Some((title, v));
                }
                Err(e) => self.report_error(e),
            },
            Msg::CloseInspect => {
                self.inspect = None;
                self.focus_restore = true;
            }
            Msg::ToggleNav => {
                self.mobile_nav = !self.mobile_nav;
                self.focus_target = Some(
                    if self.mobile_nav {
                        ".sidebar .nav-item"
                    } else {
                        ".mobile-menu"
                    }
                    .into(),
                );
            }
            Msg::DismissError => self.error.clear(),
        }
        true
    }
    fn rendered(&mut self, _ctx: &Context<Self>, _first: bool) {
        if let Some(selector) = self.focus_target.take()
            && let Some(element) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.query_selector(&selector).ok())
                .flatten()
                .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = element.focus();
        }
        if self.focus_restore {
            self.focus_restore = false;
            if let Some(element) = self.restore_focus.take() {
                let _ = element.focus();
            }
        }
    }
    fn view(&self, ctx: &Context<Self>) -> Html {
        html! {<div class="shell">
            {self.sidebar(ctx)}{if self.mobile_nav{html!{<button class="mobile-nav-backdrop" aria-label="关闭导航" onclick={ctx.link().callback(|_|Msg::ToggleNav)}/>}}else{Html::default()}}<main class="main"><header class="topbar"><button class="icon-button mobile-menu" aria-label="打开导航" onclick={ctx.link().callback(|_|Msg::ToggleNav)}>{icon("menu")}</button><div class="breadcrumb"><span>{"WORKSPACE"}</span><b>{if self.demo{"演示空间"}else{s(&self.snapshot,"creator_id")}}</b></div><div class="topbar-right"><span class="environment-chip"><i/>{"SELF-HOSTED"}</span><span class="avatar">{if self.demo{"D"}else{"S"}}</span></div></header>
            {if self.demo{html!{<div class="demo-banner"><span>{"演示模式 · 以下为合成数据，未连接生产环境"}</span><button onclick={ctx.link().callback(|_|Msg::Logout)}>{"连接真实环境 →"}</button></div>}}else{Html::default()}}
            {if !self.error.is_empty(){html!{<div class="error-banner" role="alert"><span>{&self.error}</span><button aria-label="关闭提示" onclick={ctx.link().callback(|_|Msg::DismissError)}>{icon("close")}</button></div>}}else{Html::default()}}
            {if self.token.is_none()&&!self.demo{self.login(ctx)}else if self.snapshot.is_null(){html!{<div class="loading-screen"><span class="spinner"/><h2>{"正在读取生存环境"}</h2><p>{"连接个体、账本与公共资源…"}</p></div>}}else{match &self.page{Page::Observe=>self.observe(ctx),Page::Habitats=>self.habitats(ctx),Page::Individuals=>self.individuals(ctx),Page::Environment=>self.environment(),Page::Api=>self.api_page(),Page::Habitat(id)=>self.habitat(ctx,id),Page::Agent(id)=>self.agent_page(ctx,id)}}}
            <footer class="footer"><span>{"SURVIVE"}<i>{"/"}</i>{"Observatory"}</span><span>{format!("RUST + {}",self.runtime_name().to_uppercase())}</span></footer></main>
            {self.wizard.as_ref().map(|d|self.wizard_view(ctx,d)).unwrap_or_default()}
            {self.service_preview.as_ref().map(|preview|self.preview_view(ctx, preview)).unwrap_or_default()}
            {self.inspect.as_ref().map(|(title,v)|html!{<div class="modal-backdrop"><section class="modal inspect-modal" role="dialog" aria-modal="true" aria-label={title.clone()} onkeydown={ctx.link().batch_callback(|e:KeyboardEvent|{trap_focus(&e);if e.key()=="Escape"{Some(Msg::CloseInspect)}else{None}})}><div class="modal-heading"><div><span class="eyebrow">{"READ-ONLY RECORD"}</span><h2>{title}</h2></div><button class="icon-button" aria-label="关闭详情" onclick={ctx.link().callback(|_|Msg::CloseInspect)}>{icon("close")}</button></div><div class="inspect-body">{Self::inspection(title,v)}</div></section></div>}).unwrap_or_default()}
        </div>}
    }
}
impl App {
    fn remember_focus(&mut self) {
        self.restore_focus = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.active_element())
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
    }
    fn runtime_name(&self) -> &str {
        match s(&self.snapshot["system"]["runtime"], "backend") {
            "youki" | "" => "youki",
            other => other,
        }
    }
    fn report_error(&mut self, error: String) {
        if error.starts_with("401:") {
            self.epoch += 1;
            self.detail_request += 1;
            self.token = None;
            self.snapshot = Value::Null;
            self.topology = Value::Null;
            self.detail = Value::Null;
            self.inspect = None;
            self.service_preview = None;
            self.wizard = None;
            self.loading = false;
        }
        self.error = error.trim_start_matches("401:").into();
    }
    fn fetch(&mut self, ctx: &Context<Self>) {
        self.refresh_request += 1;
        let request = self.refresh_request;
        self.loading = true;
        let token = self.token.clone().unwrap_or_default();
        let token2 = token.clone();
        send_scoped(ctx, self.epoch, async move {
            Msg::ForRefresh(
                request,
                Box::new(Msg::Snapshot(api(&token, "/api/v2/overview").await)),
            )
        });
        send_scoped(ctx, self.epoch, async move {
            Msg::ForRefresh(
                request,
                Box::new(Msg::Topology(api(&token2, "/api/v2/topology").await)),
            )
        });
    }
    fn unauthorized(&self, ctx: &Context<Self>) -> Callback<()> {
        let epoch = self.epoch;
        ctx.link()
            .callback(move |()| Msg::ForSession(epoch, Box::new(Msg::Unauthorized)))
    }
    fn fetch_detail(&mut self, ctx: &Context<Self>) {
        self.detail_error = None;
        let Some(id) = self.selected.clone() else {
            return;
        };
        if matches!(self.tab.as_str(), "overview" | "desktop") {
            self.detail_loading = false;
            return;
        }
        self.detail_loading = true;
        self.detail_request += 1;
        let request = self.detail_request;
        if self.demo {
            self.detail = demo::detail(&id, &self.tab, &self.file_path, self.file_offset);
            self.detail_loading = false;
            return;
        }
        let tab = self.tab.clone();
        let suffix = if tab == "files" {
            format!(
                "files?path={}&offset={}&limit=65536",
                encoded(&self.file_path),
                self.file_offset
            )
        } else {
            format!(
                "{tab}?limit=100{}",
                self.page_cursors
                    .last()
                    .and_then(|v| v.as_ref())
                    .map(|c| format!("&after={}", encoded(c)))
                    .unwrap_or_default()
            )
        };
        let url = format!("/api/v2/organisms/{id}/{suffix}");
        let token = self.token.clone().unwrap_or_default();
        send_scoped(ctx, self.epoch, async move {
            Msg::Detail(request, id, tab, api(&token, &url).await)
        });
    }
    fn agent(&self, id: &str) -> Value {
        arr(&self.snapshot, "organisms")
            .iter()
            .find(|a| s(a, "id") == id)
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn exp(&self, id: &str) -> Value {
        arr(&self.snapshot, "environments")
            .iter()
            .find(|e| s(e, "id") == id)
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn filtered_agents(&self) -> Vec<Value> {
        arr(&self.snapshot, "organisms")
            .iter()
            .filter(|a| {
                (self.filter_exp.is_empty() || s(a, "environment_id") == self.filter_exp)
                    && (self.filter_state.is_empty() || s(a, "state") == self.filter_state)
                    && (self.search.is_empty()
                        || s(a, "id").contains(&self.search)
                        || display_name(a).contains(&self.search))
            })
            .cloned()
            .collect()
    }
    fn sidebar(&self, ctx: &Context<Self>) -> Html {
        let nav_open = self.mobile_nav;
        let nav = [
            (Page::Observe, "network", "观测总览"),
            (Page::Habitats, "experiment", "生存环境"),
            (Page::Individuals, "agents", "生物档案"),
            (Page::Environment, "environment", "基础设施"),
            (Page::Api, "code", "API 文档"),
        ];
        html! {<aside class={classes!("sidebar",self.mobile_nav.then_some("open"))}
            onkeydown={ctx.link().batch_callback(move|e:KeyboardEvent|(nav_open && e.key()=="Escape").then_some(Msg::ToggleNav))}>
            <a class="brand" href="/console/"><span class="brand-mark">{"S"}</span><div><strong>{"SURVIVE"}</strong><span>{"OBSERVATORY"}</span></div></a>
            <nav>{for nav.into_iter().map(|(page,symbol,label)|{
                let active=self.page==page||matches!((&self.page,&page),(Page::Agent(_),Page::Individuals)|(Page::Habitat(_),Page::Habitats));
                html!{<button class={classes!("nav-item",active.then_some("active"))} aria-label={label} title={label} onclick={ctx.link().callback(move|_|Msg::Navigate(page.clone()))}>{icon(symbol)}<span>{label}</span></button>}
            })}</nav>
            <div class="sidebar-bottom">{if self.token.is_some()||self.demo{html!{<button class="logout" onclick={ctx.link().callback(|_|Msg::Logout)}>{"断开连接"}<span>{"↗"}</span></button>}}else{Html::default()}}</div>
        </aside>}
    }
    fn heading(
        &self,
        ctx: &Context<Self>,
        eyebrow: &str,
        title: &str,
        description: &str,
        create: bool,
    ) -> Html {
        html! {<div class="page-heading"><div><div class="eyebrow">{eyebrow}</div><h1>{title}</h1><p>{description}</p></div><div class="heading-actions"><button class={classes!("button","secondary",self.loading.then_some("busy"))} onclick={ctx.link().callback(|_|Msg::Refresh)} disabled={self.loading||self.demo}>{icon("refresh")}<span>{"刷新"}</span></button>{if create{html!{<button class="button primary" onclick={ctx.link().callback(|_|Msg::OpenForm(Action::Environment,None))}>{icon("plus")}{"创建环境"}</button>}}else{Html::default()}}</div></div>}
    }
    fn login(&self, ctx: &Context<Self>) -> Html {
        html! {<div class="login-page"><section class="login-story"><h1>{"数字生命"}<br/><em>{"观测台"}</em></h1>
        <p>{"创建生存环境，观察生物的能量、寿命与报告。"}</p>
        <div class="login-orbit" aria-hidden="true"><div class="orbit o1"/><div class="orbit o2"/><div class="orbit o3"/><span class="orbit-core">{"S"}</span><i class="satellite s1"/><i class="satellite s2"/><i class="satellite s3"/><i class="satellite s4"/></div>
        </section><section class="login-card"><div class="login-card-icon">{icon("lock")}</div><h2>{"连接你的生存环境"}</h2>
        <p>{"使用创建者凭据访问自己的环境和生物。"}</p>
        <form onsubmit={ctx.link().callback(|e:SubmitEvent|{e.prevent_default();Msg::Login})}>
        <label class="field"><span>{"创建者访问凭据"}</span><input type="password" autocomplete="off" placeholder="输入 Creator token" value={self.token_input.clone()} oninput={ctx.link().callback(|e|Msg::Token(input_value(e)))} required=true autofocus=true/></label>
        <button type="submit" class="button primary full" disabled={self.loading}>{if self.loading{html!{<span class="spinner"/>}}else{icon("arrow")}}{if self.loading{"正在连接…"}else{"进入观测台"}}</button></form>
        <div class="privacy-note">{icon("lock")}<p>{"凭据仅保存在当前页面内存，刷新或退出后清除。"}</p></div>
        <a class="text-link" href="/llm.txt" target="_blank" rel="noopener">{"查看接入文档"}{icon("external")}</a></section></div>}
    }
    fn pagination(&self, ctx: &Context<Self>) -> Html {
        html! {<div class="pagination"><span>{format!("第 {} 页",self.page_cursors.len())}</span><div><button class="button secondary" disabled={self.page_cursors.len()<=1||self.detail_loading} onclick={ctx.link().callback(|_|Msg::PreviousPage)}>{"← 上一页"}</button><button class="button secondary" disabled={self.demo||self.detail_loading||self.detail["next_cursor"].as_str().is_none_or(str::is_empty)} onclick={ctx.link().callback(|_|Msg::NextPage)}>{"下一页 →"}</button></div></div>}
    }
    fn events_view(&self, ctx: &Context<Self>) -> Html {
        let rows = arr(&self.detail, "events");
        html! {<section class="panel"><div class="panel-heading"><h2>{"事件记录"}</h2><span class="muted small">{"最近读取的一页 · 按记录顺序"}</span></div>{if rows.is_empty(){empty("尚无事件记录","个体开始运行后，可以查看状态与行为事件。")}else{html!{<div class="record-list">{for rows.iter().map(|e|{let raw=e.clone();html!{<button class="record-row" onclick={ctx.link().callback(move|_|if raw["data"]["truncated"].as_bool().unwrap_or(false){Msg::EventChunk(s(&raw,"id").to_string(),0)}else{Msg::Inspect("事件详情".into(),raw.clone())})}><span class="activity-icon">{icon("clock")}</span><div><strong>{event_label(s(e,"kind"))}</strong><code>{short(s(e,"id"))}</code></div><time>{date(n(e,"created_at"))}</time><span>{"↗"}</span></button>}})}</div>}}}</section>}
    }
    fn files_view(&self, ctx: &Context<Self>) -> Html {
        let parent = self
            .file_path
            .rsplit_once('/')
            .map(|(p, _)| p)
            .unwrap_or("")
            .to_string();
        let current = self.file_path.clone();
        let entries = arr(&self.detail, "entries");
        let is_dir = self.detail.get("entries").is_some();
        let data = base64::engine::general_purpose::STANDARD
            .decode(s(&self.detail, "data"))
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&data).to_string();
        let next = n(&self.detail, "next_offset");
        html! {<section class="panel"><div class="file-toolbar"><button class="icon-button" aria-label="返回上级目录" disabled={self.file_path.is_empty()} onclick={ctx.link().callback(move|_|Msg::File(parent.clone(),0))}>{"↑"}</button><code>{format!("/work/{}",self.file_path)}</code><span class="label-tag">{"READ ONLY"}</span></div>{if is_dir {if entries.is_empty(){empty("目录为空","个体还没有在这里创建文件。 ")}else{html!{<div class="file-list">{for entries.iter().filter_map(Value::as_str).map(|name|{let path=if self.file_path.is_empty(){name.to_string()}else{format!("{}/{}",self.file_path,name)};html!{<button onclick={ctx.link().callback(move|_|Msg::File(path.clone(),0))}>{icon("file")}<span>{name}</span><span>{"→"}</span></button>}})}</div>}}}else{html!{<><pre class="file-content">{text}</pre><div class="table-footer"><span>{format!("偏移 {} · 读取 {} 字节",self.file_offset,data.len())}</span>{if data.len()==65536{html!{<button class="text-link" onclick={ctx.link().callback(move|_|Msg::File(current.clone(),next))}>{"读取下一段 →"}</button>}}else{html!{<span>{"文件末尾"}</span>}}}</div></>}}}</section>}
    }
    fn services_view(&self, ctx: &Context<Self>) -> Html {
        let rows = arr(&self.detail, "services");
        html! {<section class="panel"><div class="panel-heading"><h2>{"个体发布的服务"}</h2><button class="text-link" disabled={self.demo} onclick={ctx.link().callback(|_|Msg::RetryDetail)}>{"刷新服务"}</button></div>{if rows.is_empty(){empty("尚未发布服务","生物注册服务后，入口会显示在这里。")}else{html!{<div class="services-grid">{for rows.iter().map(|r|{
            let name=s(r,"name").to_string();
            let port=n(r,"port") as u16;
            let mode=s(&r["preview"],"mode");
            let status=match mode{"live"=>"在线服务","archive"=>"已归档","unavailable"=>"离线 · 尚无预览文件",_=>"等待连接"};
            let value=json!({"name":r["name"],"port":port,"path":format!("/services/{}/{port}/",self.selected.as_deref().unwrap_or_default()),"usage":"以创建者 Bearer 凭据发送 GET/HEAD 请求。凭据不可放在 URL 中。"});
            html!{<article class="service-card"><span class="experiment-icon">{icon("code")}</span><h3>{&name}</h3>
            <code>{format!("PORT {port} · {status}")}</code>
            <button class="button secondary" disabled={mode=="unavailable"} onclick={ctx.link().callback(move|_|Msg::PreviewService(name.clone(),port))}>{"预览服务"}{icon("external")}</button>
            <button class="text-link" onclick={ctx.link().callback(move|_|Msg::Inspect("服务访问说明".into(),value.clone()))}>{"查看访问方式"}{icon("external")}</button></article>}
        })}</div>}}}</section>}
    }
}
fn trap_focus(event: &KeyboardEvent) {
    if event.key() != "Tab" {
        return;
    }
    let Some(root) = event
        .target()
        .and_then(|e| e.dyn_into::<web_sys::Element>().ok())
        .and_then(|e| e.closest("[role=dialog]").ok())
        .flatten()
    else {
        return;
    };
    let Ok(nodes)=root.query_selector_all("button:not([disabled]), a[href], input:not([disabled]), textarea:not([disabled]), select:not([disabled]), [tabindex='0']") else{return};
    let elements: Vec<web_sys::HtmlElement> = (0..nodes.length())
        .filter_map(|i| nodes.item(i))
        .filter_map(|n| n.dyn_into().ok())
        .filter(|e: &web_sys::HtmlElement| e.offset_width() > 0 || e.offset_height() > 0)
        .collect();
    let (Some(first), Some(last)) = (elements.first(), elements.last()) else {
        return;
    };
    let active = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element());
    if event.shift_key() && active.as_ref().is_some_and(|e| e == first.as_ref()) {
        event.prevent_default();
        let _ = last.focus();
    } else if !event.shift_key() && active.as_ref().is_some_and(|e| e == last.as_ref()) {
        event.prevent_default();
        let _ = first.focus();
    }
}
fn empty(title: &str, description: &str) -> Html {
    html! {<div class="empty-state"><span>{icon("network")}</span><h3>{title}</h3><p>{description}</p></div>}
}
fn event_label(kind: &str) -> &str {
    match kind {
        "agent_state" | "state_changed" => "个体状态更新",
        "usage_settled" | "settled" => "模型调用已结算",
        "grant" | "grant_created" => "创造者发放资源",
        "report" | "report_created" => "生物提交了报告",
        "heartbeat" => "生命心跳",
        "reservation" | "usage_reserved" => "模型调用资源预留",
        "restart" | "self_restart" => "个体自主重启",
        "created" => "新个体加入环境",
        _ => kind,
    }
}
#[wasm_bindgen(start)]
pub fn start() {
    let root = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id("app")
        .unwrap();
    yew::Renderer::<App>::with_root(root).render();
}

#[cfg(test)]
mod tests;
