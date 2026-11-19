use super::*;
use gloo_events::EventListener;
use gloo_net::http::{RequestBuilder, Response};
use gloo_timers::callback::Timeout;
use serde::Deserialize;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use web_sys::{
    AbortController, AbortSignal, Element, HtmlElement, MediaQueryList, RequestCache,
    RequestCredentials, RequestMode, RequestRedirect,
};

const MESSAGE_BYTES: usize = 2048;
const PAGE_SIZE: usize = 40;
const WINDOW_SIZE: usize = 240;
const POLL_MS: u32 = 3000;
const DESKTOP_KEYBOARD: &str = "(min-width: 761px) and (hover: hover) and (pointer: fine)";

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

#[derive(Deserialize)]
struct Acknowledgement {
    message: Message,
}

#[derive(Clone)]
struct Outgoing {
    text: String,
    key: String,
}

type Delivery = Result<Message, String>;

#[derive(Default)]
struct Composer {
    draft: String,
    pending: Option<Outgoing>,
    sending: bool,
    error: String,
    receipt: bool,
    listener: Option<Callback<Delivery>>,
}

impl Composer {
    fn can_send(&self) -> bool {
        self.pending.is_none() && !self.draft.trim().is_empty() && self.draft.len() <= MESSAGE_BYTES
    }

    fn submit(&mut self, retry: bool, new_key: impl FnOnce() -> String) -> Option<Outgoing> {
        if self.sending {
            return None;
        }
        if !retry {
            if !self.can_send() {
                return None;
            }
            self.pending = Some(Outgoing {
                text: std::mem::take(&mut self.draft),
                key: new_key(),
            });
        }
        let outgoing = self.pending.clone()?;
        self.sending = true;
        self.receipt = false;
        self.error.clear();
        Some(outgoing)
    }

    fn completed(&mut self, result: &Delivery) {
        self.sending = false;
        match result {
            Ok(_) => {
                self.pending = None;
                self.receipt = true;
            }
            Err(error) => self.error = error.clone(),
        }
    }
}

type ComposerSlot = Rc<RefCell<Composer>>;

#[derive(Clone, Default)]
pub(super) struct Drafts(Rc<RefCell<BTreeMap<String, ComposerSlot>>>);

impl PartialEq for Drafts {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Drafts {
    fn for_organism(&self, id: &str) -> ComposerSlot {
        self.0
            .borrow_mut()
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    fn prune(&self, id: &str) {
        let mut slots = self.0.borrow_mut();
        if slots.get(id).is_some_and(|slot| {
            let state = slot.borrow();
            state.listener.is_none() && state.pending.is_none() && state.draft.is_empty()
        }) {
            slots.remove(id);
        }
    }
}

#[derive(PartialEq, Properties)]
struct Props {
    id: String,
    token: String,
    name: String,
    stopped: bool,
    drafts: Drafts,
    unauthorized: Callback<()>,
}

#[derive(Clone, Copy, PartialEq)]
enum Read {
    Live,
    Older,
    Newer,
}

enum Scroll {
    End,
    Anchor(u64, f64),
}

struct Dialogue {
    rows: Rc<Vec<Message>>,
    // Only GET responses advance this cursor. A POST acknowledgement can overtake a GET.
    live_after: Option<u64>,
    newest: u64,
    seen: u64,
    tail_gap: bool,
    at_end: bool,
    generation: u64,
    live_request: Option<AbortController>,
    history_request: Option<(Read, AbortController)>,
    retry: Option<Timeout>,
    live_error: String,
    history_error: Option<(Read, String)>,
    composer: ComposerSlot,
    log: NodeRef,
    scroll: Option<Scroll>,
    keyboard: Option<MediaQueryList>,
    _layout: Option<EventListener>,
    _visibility: EventListener,
}

enum Action {
    Read(Read),
    Received(u64, Read, Result<MessagePage, String>),
    Visibility,
    Layout,
    Scrolled,
    Latest,
    Edit(String),
    Send(bool),
    DismissFailed,
    Delivered(Delivery),
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

async fn post_message(url: String, token: String, outgoing: Outgoing) -> Delivery {
    let controller = AbortController::new().map_err(|_| "浏览器无法发送消息。".to_string())?;
    let signal = controller.signal();
    let _timeout = Timeout::new(20_000, move || controller.abort());
    let body = json!({"text": outgoing.text, "idempotency_key": outgoing.key});
    let post = request(Request::post(&url), &token, &signal)
        .json(&body)
        .map_err(|_| "消息格式无效。".to_string())?;
    checked(post.send().await)
        .await?
        .json::<Acknowledgement>()
        .await
        .map(|ack| ack.message)
        .map_err(|_| "无法确认发送结果，请重试。".into())
}

fn excerpt(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let mut out: String = chars.by_ref().take(44).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn earlier_query(first: u64) -> Option<(u64, usize)> {
    let end = first.checked_sub(1)?;
    let count = end.min(PAGE_SIZE as u64);
    (count > 0).then_some((end - count, count as usize))
}

// Return whether newer rows were evicted. The live GET cursor can keep advancing while a
// reader stays on this window, and both sides of the window can be fetched again by sequence.
fn merge_rows(
    rows: &mut Vec<Message>,
    messages: Vec<Message>,
    kind: Read,
    anchor: Option<u64>,
) -> bool {
    for message in messages {
        if let Err(i) = rows.binary_search_by_key(&message.seq, |row| row.seq) {
            rows.insert(i, message);
        }
    }
    let excess = rows.len().saturating_sub(WINDOW_SIZE);
    if excess == 0 {
        return false;
    }
    let anchor = anchor.and_then(|seq| rows.binary_search_by_key(&seq, |m| m.seq).ok());
    let from = match kind {
        Read::Older => anchor.map_or(0, |i| (i + 1).saturating_sub(WINDOW_SIZE)),
        _ => anchor.map_or(excess, |i| i.min(excess)),
    };
    rows.drain(..from);
    let trimmed = rows.len() > WINDOW_SIZE;
    rows.truncate(WINDOW_SIZE);
    trimmed
}

impl Dialogue {
    fn cancel_reads(&mut self) {
        self.generation += 1;
        self.retry.take();
        if let Some(c) = self.live_request.take() {
            c.abort();
        }
        if let Some((_, c)) = self.history_request.take() {
            c.abort();
        }
    }

    fn schedule(&mut self, ctx: &Context<Self>, delay: u32) {
        let link = ctx.link().clone();
        self.retry = Some(Timeout::new(delay, move || {
            link.send_message(Action::Read(Read::Live))
        }));
    }

    fn start_read(&mut self, ctx: &Context<Self>, kind: Read) -> bool {
        if document_hidden() {
            return false;
        }
        let (after, limit) = match kind {
            Read::Live if self.live_request.is_none() => {
                self.retry.take();
                (self.live_after, PAGE_SIZE)
            }
            Read::Older if self.history_request.is_none() => {
                let Some((after, count)) = self.rows.first().and_then(|m| earlier_query(m.seq))
                else {
                    return false;
                };
                (Some(after), count)
            }
            Read::Newer if self.history_request.is_none() && self.tail_gap => {
                (self.rows.last().map(|m| m.seq), PAGE_SIZE)
            }
            _ => return false,
        };
        let Ok(controller) = AbortController::new() else {
            self.read_failed(ctx, kind, "浏览器无法连接对话。".into());
            return true;
        };
        let deadline = controller.clone();
        let signal = controller.signal();
        if kind == Read::Live {
            self.live_request = Some(controller);
        } else {
            self.history_request = Some((kind, controller));
            self.history_error = None;
        }
        let mut url = format!(
            "/api/v2/organisms/{}/messages?limit={limit}",
            encoded(&ctx.props().id)
        );
        if let Some(after) = after {
            url.push_str(&format!("&after={after}"));
        }
        let token = ctx.props().token.clone();
        let generation = self.generation;
        ctx.link().send_future(async move {
            let _timeout = Timeout::new(20_000, move || deadline.abort());
            let page =
                match checked(request(Request::get(&url), &token, &signal).send().await).await {
                    Ok(r) => r.json().await.map_err(|_| "对话记录格式无效。".into()),
                    Err(e) => Err(e),
                };
            Action::Received(generation, kind, page)
        });
        kind != Read::Live || !self.live_error.is_empty()
    }

    fn read_failed(&mut self, ctx: &Context<Self>, kind: Read, error: String) {
        if error == "401" {
            ctx.props().unauthorized.emit(());
        } else if kind == Read::Live {
            self.live_error = error;
            self.schedule(ctx, 2 * POLL_MS);
        } else {
            self.history_error = Some((kind, error));
        }
    }

    fn near_end(&self) -> bool {
        self.log
            .cast::<HtmlElement>()
            .is_none_or(|log| log.scroll_height() - log.scroll_top() - log.client_height() < 36)
    }

    fn visible_anchor(&self) -> Option<Scroll> {
        let log = self.log.cast::<HtmlElement>()?;
        let top = log.get_bounding_client_rect().top();
        let nodes = log.query_selector_all("[data-seq]").ok()?;
        for index in 0..nodes.length() {
            let row: Element = nodes.get(index)?.dyn_into().ok()?;
            let rect = row.get_bounding_client_rect();
            if rect.bottom() > top {
                let seq = row.get_attribute("data-seq")?.parse().ok()?;
                return Some(Scroll::Anchor(seq, rect.top() - top));
            }
        }
        None
    }

    fn merge(&mut self, messages: Vec<Message>, kind: Read, follow: bool) {
        if messages.is_empty() {
            return;
        }
        let scroll = if follow {
            Some(Scroll::End)
        } else {
            self.visible_anchor()
        };
        let anchor = match &scroll {
            Some(Scroll::Anchor(seq, _)) => Some(*seq),
            _ => None,
        };
        self.tail_gap |= merge_rows(Rc::make_mut(&mut self.rows), messages, kind, anchor);
        if follow {
            self.at_end = true;
            self.seen = self.newest;
        }
        if scroll.is_some() {
            self.scroll = scroll;
        }
    }

    fn received(&mut self, ctx: &Context<Self>, kind: Read, page: MessagePage) {
        match kind {
            Read::Live => {
                self.live_error.clear();
                let previous = self.live_after;
                self.live_after = Some(previous.unwrap_or(0).max(page.next_after));
                self.newest = self.newest.max(page.next_after);
                let follow = previous.is_none()
                    || (!self.tail_gap && self.near_end() && self.history_request.is_none());
                if previous.is_none() {
                    // Acknowledged sends may have arrived after this snapshot was taken.
                    Rc::make_mut(&mut self.rows).retain(|m| m.seq > page.next_after);
                    self.tail_gap = false;
                }
                if !self.tail_gap {
                    let mut messages = page.messages;
                    if let Some(after) = previous {
                        messages.retain(|m| m.seq > after);
                    }
                    self.merge(messages, kind, follow);
                }
                let catch_up =
                    page.has_more && previous.is_none_or(|cursor| page.next_after > cursor);
                self.schedule(ctx, if catch_up { 0 } else { POLL_MS });
            }
            Read::Older => {
                // Discard an obsolete page when a concurrent live burst moved the whole window.
                let adjoining = page
                    .messages
                    .last()
                    .zip(self.rows.first())
                    .is_some_and(|(old, first)| old.seq.checked_add(1) == Some(first.seq));
                if adjoining {
                    self.merge(page.messages, kind, false);
                }
            }
            Read::Newer => {
                // This adjacent page can overtake a slower live request. Keep one monotonic
                // cursor so that its eventual response cannot reinsert evicted history.
                self.live_after = Some(self.live_after.unwrap_or(0).max(page.next_after));
                self.newest = self.newest.max(page.next_after);
                self.tail_gap = page.has_more || self.newest > page.next_after;
                self.merge(page.messages, kind, false);
            }
        }
    }

    fn send(&mut self, ctx: &Context<Self>, retry: bool) -> bool {
        let Some(outgoing) = self.composer.borrow_mut().submit(retry, || {
            format!(
                "chat-{}-{:x}",
                js_sys::Date::now() as u64,
                js_sys::Math::random().to_bits()
            )
        }) else {
            return false;
        };
        let id = ctx.props().id.clone();
        let token = ctx.props().token.clone();
        let url = format!("/api/v2/organisms/{}/messages", encoded(&id));
        let slot = self.composer.clone();
        let drafts = ctx.props().drafts.clone();
        // The send belongs to the login session. Switching tabs or organisms can remount this view.
        wasm_bindgen_futures::spawn_local(async move {
            let result = post_message(url, token, outgoing).await;
            let listener = {
                let mut composer = slot.borrow_mut();
                composer.completed(&result);
                composer.listener.clone()
            };
            if let Some(listener) = listener {
                listener.emit(result);
            }
            drafts.prune(&id);
        });
        true
    }
}

#[derive(Properties)]
struct MessageRowsProps {
    rows: Rc<Vec<Message>>,
    name: String,
}

impl PartialEq for MessageRowsProps {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.rows, &other.rows) && self.name == other.name
    }
}

// Typing in the composer leaves the transcript and its formatted timestamps in place.
#[function_component(MessageRows)]
fn message_rows(props: &MessageRowsProps) -> Html {
    html! {<>{for props.rows.iter().map(|m| {
        let owner = m.role == Role::Creator;
        html! {<article key={m.seq} data-seq={m.seq.to_string()} class={classes!("dialogue-message",owner.then_some("outgoing"))}>
            <header><strong>{if owner {"创造者"} else {props.name.as_str()}}</strong><time title={format!("消息 #{}",m.seq)}>{date(m.created_at)}</time></header>
            {m.reply_to.map(|seq| {
                let source = props.rows.binary_search_by_key(&seq, |row| row.seq).ok().map(|i| &props.rows[i]);
                html! {<blockquote class="dialogue-quote">
                    <span>{if source.is_some() {"回复".to_owned()} else {format!("回复 #{seq}")}}</span>
                    {source.map(|row| html!{<span>{excerpt(&row.text)}</span>}).unwrap_or_default()}
                </blockquote>}
            }).unwrap_or_default()}
            <p>{&m.text}</p>
        </article>}
    })}</>}
}

impl Component for Dialogue {
    type Message = Action;
    type Properties = Props;

    fn create(ctx: &Context<Self>) -> Self {
        let window = web_sys::window().unwrap_throw();
        let document = window.document().unwrap_throw();
        let link = ctx.link().clone();
        let visibility = EventListener::new(&document, "visibilitychange", move |_| {
            link.send_message(Action::Visibility)
        });
        let keyboard = window.match_media(DESKTOP_KEYBOARD).ok().flatten();
        let link = ctx.link().clone();
        let layout = keyboard.as_ref().map(|query| {
            EventListener::new(query, "change", move |_| link.send_message(Action::Layout))
        });
        let composer = ctx.props().drafts.for_organism(&ctx.props().id);
        composer.borrow_mut().listener = Some(ctx.link().callback(Action::Delivered));
        ctx.link().send_message(Action::Read(Read::Live));
        Self {
            rows: Rc::default(),
            live_after: None,
            newest: 0,
            seen: 0,
            tail_gap: false,
            at_end: true,
            generation: 0,
            live_request: None,
            history_request: None,
            retry: None,
            live_error: String::new(),
            history_error: None,
            composer,
            log: NodeRef::default(),
            scroll: None,
            keyboard,
            _layout: layout,
            _visibility: visibility,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, action: Action) -> bool {
        match action {
            Action::Read(kind) => return self.start_read(ctx, kind),
            Action::Received(generation, kind, page) => {
                if generation != self.generation {
                    return false;
                }
                match kind {
                    Read::Live => self.live_request = None,
                    _ => self.history_request = None,
                }
                match page {
                    Ok(page) => self.received(ctx, kind, page),
                    Err(e) => self.read_failed(ctx, kind, e),
                }
            }
            Action::Visibility => {
                self.cancel_reads();
                if !document_hidden() {
                    ctx.link().send_message(Action::Read(Read::Live));
                }
            }
            Action::Layout => {}
            Action::Scrolled => {
                let at_end = !self.tail_gap && self.near_end();
                if at_end == self.at_end && (!at_end || self.seen == self.newest) {
                    return false;
                }
                self.at_end = at_end;
                if at_end {
                    self.seen = self.newest;
                }
            }
            Action::Latest => {
                if self.tail_gap || self.history_request.is_some() {
                    self.cancel_reads();
                    self.live_after = None;
                    self.history_error = None;
                    ctx.link().send_message(Action::Read(Read::Live));
                } else {
                    self.at_end = true;
                    self.seen = self.newest;
                    self.scroll = Some(Scroll::End);
                }
            }
            Action::Edit(draft) => {
                let mut composer = self.composer.borrow_mut();
                if draft == composer.draft {
                    return false;
                }
                composer.draft = draft;
                composer.receipt = false;
            }
            Action::Send(retry) => return self.send(ctx, retry),
            Action::DismissFailed => {
                let mut composer = self.composer.borrow_mut();
                if composer.sending || composer.error.is_empty() {
                    return false;
                }
                composer.pending = None;
                composer.error.clear();
            }
            Action::Delivered(result) => match result {
                Ok(message) => {
                    self.newest = self.newest.max(message.seq);
                    // A retried acknowledgement may refer to a row older than this window.
                    if !self.tail_gap && self.rows.first().is_none_or(|m| message.seq >= m.seq) {
                        let follow = self.near_end() && self.history_request.is_none();
                        self.merge(vec![message], Read::Live, follow);
                    }
                    ctx.link().send_message(Action::Read(Read::Live));
                }
                Err(e) if e == "401" => ctx.props().unauthorized.emit(()),
                Err(_) => {}
            },
        }
        true
    }

    fn rendered(&mut self, _: &Context<Self>, _: bool) {
        let Some(log) = self.log.cast::<HtmlElement>() else {
            return;
        };
        match self.scroll.take() {
            Some(Scroll::End) => log.set_scroll_top(log.scroll_height()),
            Some(Scroll::Anchor(seq, offset)) => {
                if let Ok(Some(row)) = log.query_selector(&format!("[data-seq='{seq}']")) {
                    let now =
                        row.get_bounding_client_rect().top() - log.get_bounding_client_rect().top();
                    log.set_scroll_top(log.scroll_top() + (now - offset).round() as i32);
                }
            }
            None => {}
        }
    }

    fn destroy(&mut self, ctx: &Context<Self>) {
        self.cancel_reads();
        self.composer.borrow_mut().listener = None;
        ctx.props().drafts.prune(&ctx.props().id);
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let state = self.composer.borrow();
        let stopped = ctx.props().stopped;
        let bytes = state.draft.len();
        let desktop = self.keyboard.as_ref().is_some_and(|q| q.matches());
        let loading = self.history_request.as_ref().map(|(kind, _)| *kind);
        let unseen = self.newest.saturating_sub(self.seen);
        let jump = !self.at_end || self.tail_gap || unseen > 0;
        let failed = state.pending.as_ref().filter(|_| !state.sending);
        html! {<section class="panel dialogue-panel" aria-label="与生物对话">
            <header class="panel-heading"><h2>{"与生物对话"}</h2></header>
            {if stopped {html!{<div class="dialogue-state"><p>{"生物当前未运行，可留言，待它恢复后读取。"}</p></div>}}else{Html::default()}}
            <div class="dialogue-log" ref={self.log.clone()} tabindex="0" role="log" aria-label="对话记录"
                aria-live={if self.at_end && !self.tail_gap && loading.is_none() {"polite"} else {"off"}}
                onscroll={ctx.link().callback(|_|Action::Scrolled)}>
                {if self.rows.first().is_some_and(|m| m.seq > 1) {html!{
                    <button key="older" class="text-link dialogue-older" type="button" disabled={loading.is_some() || self.live_after.is_none()}
                        onclick={ctx.link().callback(|_|Action::Read(Read::Older))}>
                        {if loading == Some(Read::Older) {"正在加载…"} else {"加载更早消息"}}
                    </button>
                }} else {Html::default()}}
                {if self.rows.is_empty() && self.live_error.is_empty() {html!{
                    <p class="dialogue-empty">{if self.live_after.is_none() {"正在读取对话…"} else {"还没有消息。你可以给这个生物写下第一条留言。"}}</p>
                }} else {Html::default()}}
                <MessageRows rows={self.rows.clone()} name={ctx.props().name.clone()}/>
                {if self.tail_gap {html!{
                    <button key="newer" type="button" class="text-link dialogue-newer" disabled={loading.is_some() || self.live_after.is_none()}
                        onclick={ctx.link().callback(|_|Action::Read(Read::Newer))}>
                        {if loading == Some(Read::Newer) {"正在加载…"} else {"加载后续消息"}}
                    </button>
                }} else {Html::default()}}
            </div>
            {if jump {html!{
                <button class="text-link dialogue-jump" type="button" onclick={ctx.link().callback(|_|Action::Latest)}>
                    {if unseen == 0 {"回到最新".into()} else {format!("{unseen} 条新消息 · 回到最新")}}
                </button>
            }} else {Html::default()}}
            {if !self.live_error.is_empty() {html!{<div class="dialogue-error" role="alert">{&self.live_error}
                <button class="text-link" type="button" disabled={self.live_request.is_some()} onclick={ctx.link().callback(|_|Action::Read(Read::Live))}>{"重新读取"}</button></div>
            }}else{Html::default()}}
            {self.history_error.as_ref().map(|(kind,error)|{
                let kind=*kind;
                html!{<div class="dialogue-error" role="alert">{error}
                    <button class="text-link" type="button" onclick={ctx.link().callback(move |_|Action::Read(kind))}>{"重试加载"}</button>
                </div>}
            }).unwrap_or_default()}
            <form class="dialogue-compose" onsubmit={ctx.link().callback(|e:SubmitEvent| {e.prevent_default(); Action::Send(false)})}>
                <label class="field"><span>{"消息"}</span>
                    <textarea id="organism-chat-composer" rows="2" placeholder="写给这个生物…" value={state.draft.clone()}
                        enterkeyhint={if desktop {"send"} else {"enter"}} aria-describedby="dialogue-key-hint dialogue-byte-count"
                        aria-invalid={(bytes>MESSAGE_BYTES).to_string()} oninput={ctx.link().callback(|e|Action::Edit(textarea_value(e)))}
                        onkeydown={ctx.link().batch_callback(move |e:KeyboardEvent|{
                            if desktop && e.key()=="Enter" && !e.shift_key() && !e.ctrl_key() && !e.alt_key() && !e.meta_key()
                                && !e.is_composing() && e.key_code()!=229 {
                                e.prevent_default();
                                return (!e.repeat()).then_some(Action::Send(false));
                            }
                            None
                        })}/>
                </label>
                <span class="dialogue-key-hint" id="dialogue-key-hint">{if desktop {"Enter 发送，Shift+Enter 换行"} else {"回车换行，点按发送"}}</span>
                {failed.map(|message| html! {<div class="dialogue-error dialogue-send-error" role="alert">
                    <span>{format!("「{}」发送未确认。{}",excerpt(&message.text),state.error)}</span>
                    <button class="text-link dialogue-retry" type="button" onclick={ctx.link().callback(|_|Action::Send(true))}>{"重试发送"}</button>
                    <button class="text-link" type="button" onclick={ctx.link().callback(|_|Action::DismissFailed)}>{"放弃重试"}</button>
                </div>}).unwrap_or_default()}
                <div class="dialogue-send">
                    <span id="dialogue-byte-count" class={classes!("small",(bytes>MESSAGE_BYTES).then_some("inline-error"))}>{format!("{bytes} / {MESSAGE_BYTES} 字节")}</span>
                    <span role="status" class="dialogue-feedback">{if state.sending {"正在发送…"} else if state.receipt {
                        if stopped {"留言已保存"} else {"消息已发送"}
                    } else {""}}</span>
                    <button class="button primary" type="submit" disabled={!state.can_send()}>{"发送"}</button>
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
        drafts={self.dialogue_drafts.clone()} stopped={s(organism,"state")!="running"} unauthorized={self.unauthorized(ctx)} />}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(seq: u64) -> Message {
        Message {
            seq,
            role: Role::Creator,
            text: seq.to_string(),
            created_at: 0,
            reply_to: None,
        }
    }

    #[test]
    fn uncertain_send_reuses_the_original_key_and_text_across_remounts() {
        let drafts = Drafts::default();
        let slot = drafts.for_organism("a");
        slot.borrow_mut().draft = "第一条".into();
        let original = slot
            .borrow_mut()
            .submit(false, || "original-key".into())
            .unwrap();
        assert_eq!(original.text, "第一条");
        assert!(slot.borrow().draft.is_empty());
        slot.borrow_mut().draft = "下一条".into();
        assert!(slot.borrow_mut().submit(false, || panic!()).is_none());
        slot.borrow_mut()
            .completed(&Err("lost acknowledgement".into()));
        drafts.prune("a");
        let returned = drafts.for_organism("a");
        assert!(Rc::ptr_eq(&slot, &returned));
        assert!(drafts.for_organism("b").borrow().draft.is_empty());
        let retry = returned.borrow_mut().submit(true, || panic!()).unwrap();
        assert_eq!((retry.key, retry.text), (original.key, original.text));
        returned.borrow_mut().completed(&Ok(message(5)));
        assert_eq!(slot.borrow().draft, "下一条");
        assert!(slot.borrow().can_send());
        assert!(
            Drafts::default()
                .for_organism("a")
                .borrow()
                .draft
                .is_empty()
        );
    }

    #[test]
    fn earlier_pages_join_the_oldest_displayed_row() {
        for first in [0, 1] {
            assert_eq!(earlier_query(first), None);
        }
        for first in [2, 5, 40, 41, 42, 100, u64::MAX] {
            let (after, limit) = earlier_query(first).unwrap();
            assert_eq!(after + limit as u64, first - 1);
            assert_eq!(
                limit,
                usize::try_from((first - 1).min(PAGE_SIZE as u64)).unwrap()
            );
        }
    }

    #[test]
    fn live_bursts_keep_the_readers_anchor_and_deduplicate_post_acknowledgements() {
        let mut rows = (1..=240).map(message).collect();
        assert!(!merge_rows(
            &mut rows,
            vec![message(239), message(240)],
            Read::Live,
            None
        ));
        assert!(merge_rows(
            &mut rows,
            (241..=280).map(message).collect(),
            Read::Live,
            Some(5)
        ));
        assert_eq!((rows[0].seq, rows.last().unwrap().seq), (5, 244));
        assert!(merge_rows(
            &mut rows,
            (1..5).map(message).collect(),
            Read::Older,
            Some(5)
        ));
        assert_eq!((rows[0].seq, rows.last().unwrap().seq), (1, 240));
        assert!(!merge_rows(
            &mut rows,
            (241..=280).map(message).collect(),
            Read::Newer,
            Some(235)
        ));
        assert_eq!((rows[0].seq, rows.last().unwrap().seq), (41, 280));
        assert_eq!(rows.len(), WINDOW_SIZE);
    }
}
