use super::*;
use gloo_events::EventListener;
use gloo_net::http::{RequestBuilder, Response};
use gloo_timers::callback::Timeout;
use serde::Deserialize;
use web_sys::{
    AbortController, AbortSignal, RequestCache, RequestCredentials, RequestMode, RequestRedirect,
};

const MESSAGE_BYTES: usize = 2048;
const RECENT_MESSAGES: usize = 100;
const POLL_MS: u32 = 3000;

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Role {
    Creator,
    Organism,
}

#[derive(Clone, Deserialize)]
struct Message {
    seq: u64,
    role: Role,
    text: String,
    created_at: u64,
    reply_to: Option<u64>,
}

#[derive(Deserialize)]
struct MessagePage {
    messages: Vec<Message>,
    next_after: u64,
    has_more: bool,
}

#[derive(PartialEq, Properties)]
struct Props {
    id: String,
    token: String,
    name: String,
    stopped: bool,
    unauthorized: Callback<()>,
}

struct Dialogue {
    rows: Vec<Message>,
    next_after: Option<u64>,
    history: Vec<u64>,
    has_more: bool,
    read_generation: u64,
    reading: Option<AbortController>,
    retry: Option<Timeout>,
    read_error: String,
    draft: String,
    // Reuse the same key when an acknowledgement is lost.
    key: Option<String>,
    sending: bool,
    send_error: String,
    receipt: bool,
    log: NodeRef,
    scroll_to_end: bool,
    _visibility: EventListener,
}

enum Action {
    Read,
    Received(u64, Result<MessagePage, String>),
    Visibility,
    First,
    Previous,
    Next,
    Latest,
    Edit(String),
    Send,
    Sent(Result<(), String>),
}

fn request(builder: RequestBuilder, token: &str, signal: &AbortSignal) -> RequestBuilder {
    builder
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/json")
        .mode(RequestMode::SameOrigin)
        .credentials(RequestCredentials::Omit)
        .redirect(RequestRedirect::Error)
        .cache(RequestCache::NoStore)
        .abort_signal(Some(signal))
}

async fn checked(result: Result<Response, gloo_net::Error>) -> Result<Response, String> {
    let response = result.map_err(|_| "网络连接中断，请重试。".to_string())?;
    match response.status() {
        200..300 => Ok(response),
        401 => Err("401".into()),
        _ => {
            let error = response.json::<Value>().await.unwrap_or_default();
            Err(error["error"]["message"]
                .as_str()
                .unwrap_or("服务器未能完成请求，请重试。")
                .into())
        }
    }
}

impl Dialogue {
    fn cancel_read(&mut self) {
        self.read_generation += 1;
        self.retry.take();
        if let Some(request) = self.reading.take() {
            request.abort();
        }
    }

    fn reload(&mut self, ctx: &Context<Self>) {
        self.cancel_read();
        self.rows.clear();
        self.next_after = None;
        self.has_more = false;
        self.read_error.clear();
        ctx.link().send_message(Action::Read);
    }

    fn schedule(&mut self, ctx: &Context<Self>, delay: u32) {
        let link = ctx.link().clone();
        self.retry = Some(Timeout::new(delay, move || link.send_message(Action::Read)));
    }

    fn valid(&self) -> bool {
        !self.sending && !self.draft.trim().is_empty() && self.draft.len() <= MESSAGE_BYTES
    }
}

impl Component for Dialogue {
    type Message = Action;
    type Properties = Props;

    fn create(ctx: &Context<Self>) -> Self {
        let document = web_sys::window().unwrap_throw().document().unwrap_throw();
        let link = ctx.link().clone();
        let visibility = EventListener::new(&document, "visibilitychange", move |_| {
            link.send_message(Action::Visibility)
        });
        ctx.link().send_message(Action::Read);
        Self {
            rows: vec![],
            next_after: None,
            history: vec![],
            has_more: false,
            reading: None,
            read_generation: 0,
            retry: None,
            read_error: String::new(),
            draft: String::new(),
            key: None,
            sending: false,
            send_error: String::new(),
            receipt: false,
            log: NodeRef::default(),
            scroll_to_end: false,
            _visibility: visibility,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, action: Action) -> bool {
        match action {
            Action::Read => {
                if document_hidden() || self.reading.is_some() {
                    return false;
                }
                self.retry.take();
                let Ok(controller) = AbortController::new() else {
                    self.read_error = "浏览器无法连接对话。".into();
                    return true;
                };
                let signal = controller.signal();
                let deadline = controller.clone();
                self.reading = Some(controller);
                let after = self.history.last().copied().or(self.next_after);
                let mut url = format!("/api/v2/organisms/{}/messages", encoded(&ctx.props().id));
                if let Some(after) = after {
                    url.push_str(&format!("?after={after}"));
                }
                let token = ctx.props().token.clone();
                let generation = self.read_generation;
                ctx.link().send_future(async move {
                    let _timeout = Timeout::new(20_000, move || deadline.abort());
                    let page =
                        match checked(request(Request::get(&url), &token, &signal).send().await)
                            .await
                        {
                            Ok(r) => r.json().await.map_err(|_| "对话记录格式无效。".into()),
                            Err(e) => Err(e),
                        };
                    Action::Received(generation, page)
                });
            }
            Action::Received(generation, result) => {
                if generation != self.read_generation {
                    return false;
                }
                self.reading = None;
                match result {
                    Err(e) if e == "401" => ctx.props().unauthorized.emit(()),
                    Err(e) => {
                        self.read_error = e;
                        if self.history.is_empty() {
                            self.schedule(ctx, 2 * POLL_MS);
                        }
                    }
                    Ok(page) => {
                        self.read_error.clear();
                        let previous = self.next_after.unwrap_or(0);
                        self.next_after = Some(page.next_after.max(previous));
                        self.has_more = page.has_more;
                        if self.history.is_empty() {
                            let initial = self.rows.is_empty();
                            let near_end =
                                self.log.cast::<web_sys::HtmlElement>().is_none_or(|log| {
                                    log.scroll_height() - log.scroll_top() - log.client_height()
                                        < 50
                                });
                            self.scroll_to_end = !page.messages.is_empty() && (near_end || initial);
                            self.rows
                                .extend(page.messages.into_iter().filter(|m| m.seq > previous));
                            self.rows
                                .drain(..self.rows.len().saturating_sub(RECENT_MESSAGES));
                            self.schedule(
                                ctx,
                                if page.has_more && page.next_after > previous {
                                    0
                                } else {
                                    POLL_MS
                                },
                            );
                        } else {
                            self.rows = page.messages;
                        }
                    }
                }
            }
            Action::Visibility => {
                self.cancel_read();
                if !document_hidden() {
                    ctx.link().send_message(Action::Read);
                }
            }
            Action::First => {
                self.history = vec![0];
                self.reload(ctx);
            }
            Action::Next if self.has_more && !self.history.is_empty() => {
                if let Some(after) = self.next_after {
                    self.history.push(after);
                    self.reload(ctx);
                }
            }
            Action::Previous if self.history.len() > 1 => {
                self.history.pop();
                self.reload(ctx);
            }
            Action::Next | Action::Previous => return false,
            Action::Latest => {
                self.history.clear();
                self.reload(ctx);
            }
            Action::Edit(draft) => {
                if self.sending || self.draft == draft {
                    return false;
                }
                self.draft = draft;
                self.key = None;
                self.send_error.clear();
                self.receipt = false;
            }
            Action::Send => {
                if !self.valid() {
                    return false;
                }
                self.sending = true;
                self.receipt = false;
                self.send_error.clear();
                let key = self.key.get_or_insert_with(|| {
                    format!(
                        "chat-{}-{:x}",
                        js_sys::Date::now() as u64,
                        js_sys::Math::random().to_bits()
                    )
                });
                let body = json!({"text":self.draft,"idempotency_key":key});
                let url = format!("/api/v2/organisms/{}/messages", encoded(&ctx.props().id));
                let token = ctx.props().token.clone();
                ctx.link().send_future(async move {
                    let result = async {
                        let controller = AbortController::new()
                            .map_err(|_| "浏览器无法发送消息。".to_string())?;
                        let signal = controller.signal();
                        let _timeout = Timeout::new(20_000, move || controller.abort());
                        let post = request(Request::post(&url), &token, &signal)
                            .json(&body)
                            .map_err(|_| "消息格式无效。".to_string())?;
                        checked(post.send().await).await.map(|_| ())
                    }
                    .await;
                    Action::Sent(result)
                });
            }
            Action::Sent(result) => {
                self.sending = false;
                match result {
                    Ok(()) => {
                        self.draft.clear();
                        self.key = None;
                        self.receipt = true;
                        if self.history.is_empty() {
                            self.cancel_read();
                            ctx.link().send_message(Action::Read);
                        } else {
                            self.history.clear();
                            self.reload(ctx);
                        }
                    }
                    Err(e) if e == "401" => ctx.props().unauthorized.emit(()),
                    Err(e) => self.send_error = e,
                }
            }
        }
        true
    }

    fn rendered(&mut self, _: &Context<Self>, _: bool) {
        if std::mem::take(&mut self.scroll_to_end)
            && let Some(log) = self.log.cast::<web_sys::HtmlElement>()
        {
            log.set_scroll_top(log.scroll_height());
        }
    }

    fn destroy(&mut self, _: &Context<Self>) {
        self.cancel_read();
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let history = !self.history.is_empty();
        let stopped = ctx.props().stopped;
        let bytes = self.draft.len();
        let title = if history {
            "历史对话"
        } else {
            "与生物对话"
        };
        html! {<section class="panel dialogue-panel" aria-label="与生物对话">
            <header class="panel-heading"><h2>{title}</h2>
                {if history {html!{<button class="text-link" onclick={ctx.link().callback(|_|Action::Latest)}>{"返回最新消息"}</button>}}
                else if self.rows.first().is_some_and(|m| m.seq > 1){html!{<button class="text-link" onclick={ctx.link().callback(|_|Action::First)}>{"从头查看"}</button>}}else{Html::default()}}
            </header>
            {if stopped {html!{<div class="dialogue-state"><p>{"生物当前未运行，可留言，待它恢复后读取。"}</p></div>}}else{Html::default()}}
            <div class="dialogue-log" ref={self.log.clone()} role="log" aria-label="对话记录" aria-live="polite">
                {if self.rows.is_empty() && self.read_error.is_empty() { html! {<p class="dialogue-empty">{if self.next_after.is_none() {"正在读取对话…"} else {"还没有消息。你可以给这个生物写下第一条留言。"}}</p>} }else{Html::default()}}
                {for self.rows.iter().map(|m| {
                    let owner = m.role == Role::Creator;
                    html! {<article key={m.seq} class={classes!("dialogue-message",owner.then_some("outgoing"))}>
                        <header><strong>{if owner {"创造者"} else {ctx.props().name.as_str()}}</strong><time>{date(m.created_at)}</time></header>
                        {if let Some(seq) = m.reply_to {html! {<small>{format!("回复 #{seq}")}</small>}} else { Html::default() }}
                        <p>{&m.text}</p><span class="message-seq">{format!("#{}",m.seq)}</span>
                    </article>}
                })}
            </div>
            {if !self.read_error.is_empty(){html!{<div class="dialogue-error" role="alert">{&self.read_error}<button class="text-link" disabled={self.reading.is_some()} onclick={ctx.link().callback(|_|Action::Read)}>{"重新读取"}</button></div>}}else{Html::default()}}
            {if history {html!{<div class="pagination"><span>{format!("第 {} 页",self.history.len())}</span><div>
                <button class="button secondary" disabled={self.history.len()<2} onclick={ctx.link().callback(|_|Action::Previous)}>{"上一页"}</button>
                <button class="button secondary" disabled={!self.has_more} onclick={ctx.link().callback(|_|Action::Next)}>{"下一页"}</button>
            </div></div>}} else {Html::default()}}
            <form class="dialogue-compose" onsubmit={ctx.link().callback(|e:SubmitEvent| {e.prevent_default(); Action::Send})}>
                <label class="field"><span>{"消息"}</span><textarea rows="3" placeholder="写给这个生物…" value={self.draft.clone()} disabled={self.sending}
                    aria-invalid={(bytes>MESSAGE_BYTES).to_string()} oninput={ctx.link().callback(|e|Action::Edit(textarea_value(e)))}/></label>
                {if !self.send_error.is_empty() {html! {<p class="inline-error" role="alert">{&self.send_error}</p>}} else {Html::default()}}
                <div class="dialogue-send"><span class={classes!("small",(bytes>MESSAGE_BYTES).then_some("inline-error"))}>{format!("{bytes} / {MESSAGE_BYTES} 字节")}</span>
                    <span role="status">{if self.receipt {if stopped {"留言已保存"} else {"消息已发送"}} else {""}}</span>
                    <button class="button primary" type="submit" disabled={!self.valid()}>{if self.sending {"发送中…"}else if !self.send_error.is_empty(){"重试发送"}else{"发送"}}</button>
                </div>
            </form>
        </section>}
    }
}

impl App {
    pub(super) fn dialogue_view(&self, ctx: &Context<Self>, organism: &Value) -> Html {
        let Some(token) = self.token.clone() else {
            return empty("演示对话", "连接真实环境后即可给生物留言并查看它的回复。");
        };
        let id = s(organism, "id");
        html! {<Dialogue key={format!("{}/{id}",self.epoch)} id={id.to_owned()} {token} name={display_name(organism).to_owned()}
        stopped={s(organism,"state")!="running"} unauthorized={self.unauthorized(ctx)} />}
    }
}
