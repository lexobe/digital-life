use super::*;
use gloo_events::EventListener;
use gloo_file::ObjectUrl;
use gloo_timers::callback::Timeout;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AbortController, AbortSignal, RequestCache, RequestCredentials, RequestMode, RequestRedirect,
};

#[derive(PartialEq, Properties)]
pub(super) struct Props {
    pub id: String,
    pub token: String,
    pub running: bool,
    pub unauthorized: Callback<()>,
    pub children: Children,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    Live,
    Snapshot,
}

pub(super) struct Desktop {
    frame: Option<(ObjectUrl, Source, Option<u64>)>,
    preview: NodeRef,
    status: &'static str,
    saved_status: &'static str,
    missed: u8,
    checked_snapshot: bool,
    retry: Option<Timeout>,
    request: Option<AbortController>,
    generation: u64,
    _visibility: EventListener,
}

pub(super) enum Frame {
    Image(ObjectUrl, Option<u64>),
    Waiting,
    Stopped,
    Unauthorized,
    Unavailable,
}

pub(super) enum Action {
    Poll(Source),
    VisibleChanged,
    Received(u64, Source, Frame),
    ImageError,
    Enlarge,
    ClosePreview,
}

async fn request_frame(id: &str, token: &str, source: Source, signal: &AbortSignal) -> Frame {
    let mut path = format!("/api/v2/organisms/{}/desktop", encoded(id));
    if source == Source::Snapshot {
        path.push_str("/snapshot");
    }
    let Ok(response) = Request::get(&path)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "image/jpeg")
        .mode(RequestMode::SameOrigin)
        .credentials(RequestCredentials::Omit)
        .redirect(RequestRedirect::Error)
        .cache(RequestCache::NoStore)
        .abort_signal(Some(signal))
        .send()
        .await
    else {
        return Frame::Unavailable;
    };
    match response.status() {
        204 => Frame::Waiting,
        409 if source == Source::Live => Frame::Stopped,
        401 => Frame::Unauthorized,
        200 if response.headers().get("content-type").as_deref() == Some("image/jpeg") => {
            let time = response
                .headers()
                .get("x-survive-captured-at")
                .and_then(|v| v.parse().ok());
            let raw = web_sys::Response::from(response);
            let Ok(promise) = raw.blob() else {
                return Frame::Unavailable;
            };
            let Ok(value) = JsFuture::from(promise).await else {
                return Frame::Unavailable;
            };
            let blob = web_sys::Blob::from(value);
            if blob.size() > 0.0 {
                Frame::Image(ObjectUrl::from(blob), time)
            } else {
                Frame::Unavailable
            }
        }
        _ => Frame::Unavailable,
    }
}

impl Desktop {
    fn cancel(&mut self) {
        self.generation += 1;
        self.retry.take();
        if let Some(request) = self.request.take() {
            request.abort();
        }
    }

    fn first(props: &Props) -> Source {
        if props.running {
            Source::Live
        } else {
            Source::Snapshot
        }
    }
}

impl Component for Desktop {
    type Message = Action;
    type Properties = Props;

    fn create(ctx: &Context<Self>) -> Self {
        let document = web_sys::window().unwrap_throw().document().unwrap_throw();
        let link = ctx.link().clone();
        let visibility = EventListener::new(&document, "visibilitychange", move |_| {
            link.send_message(Action::VisibleChanged);
        });
        ctx.link()
            .send_message(Action::Poll(Self::first(ctx.props())));
        Self {
            frame: None,
            preview: NodeRef::default(),
            status: if ctx.props().running {
                "正在连接实时桌面…"
            } else {
                "生物当前未运行"
            },
            saved_status: "",
            missed: 0,
            checked_snapshot: false,
            retry: None,
            request: None,
            generation: 0,
            _visibility: visibility,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, action: Action) -> bool {
        match action {
            Action::Enlarge => {
                if let Some(dialog) = self.preview.cast::<web_sys::HtmlDialogElement>() {
                    let _ = dialog.show_modal();
                }
                false
            }
            Action::ClosePreview => {
                if let Some(dialog) = self.preview.cast::<web_sys::HtmlDialogElement>() {
                    dialog.close();
                }
                false
            }
            Action::Poll(source) => {
                if document_hidden() || self.request.is_some() {
                    return false;
                }
                let Ok(controller) = AbortController::new() else {
                    self.status = "此浏览器无法建立桌面连接。";
                    return true;
                };
                let signal = controller.signal();
                let deadline = controller.clone();
                self.request = Some(controller);
                let generation = self.generation;
                let id = ctx.props().id.clone();
                let token = ctx.props().token.clone();
                ctx.link().send_future(async move {
                    let _timeout = Timeout::new(30_000, move || deadline.abort());
                    Action::Received(
                        generation,
                        source,
                        request_frame(&id, &token, source, &signal).await,
                    )
                });
                false
            }
            Action::VisibleChanged => {
                self.cancel();
                self.frame = None;
                self.missed = 0;
                self.checked_snapshot = false;
                self.saved_status = "";
                self.status = if ctx.props().running {
                    "正在连接实时桌面…"
                } else {
                    "生物当前未运行"
                };
                ctx.link()
                    .send_message(Action::Poll(Self::first(ctx.props())));
                true
            }
            Action::Received(generation, source, frame) => {
                if generation != self.generation {
                    return false;
                }
                self.request = None;
                match (source, frame) {
                    (_, Frame::Unauthorized) => {
                        ctx.props().unauthorized.emit(());
                        return false;
                    }
                    (Source::Live, Frame::Image(url, captured)) => {
                        self.frame = Some((
                            url,
                            source,
                            captured.or_else(|| Some(js_sys::Date::now() as u64)),
                        ));
                        self.status = "实时";
                        self.saved_status = "";
                        self.missed = 0;
                        self.checked_snapshot = false;
                    }
                    (Source::Snapshot, Frame::Image(url, time)) => {
                        self.frame = Some((url, source, time));
                        self.saved_status = "";
                    }
                    (Source::Live, result) => {
                        self.missed = self.missed.saturating_add(1);
                        self.status = match result {
                            Frame::Stopped => {
                                self.missed = 2;
                                "实时桌面已停止"
                            }
                            Frame::Waiting => "尚未收到实时画面，正在等待桌面观察进程。",
                            _ => "实时连接暂时中断，正在重试…",
                        };
                    }
                    (Source::Snapshot, Frame::Waiting) => {
                        self.saved_status = "尚无已保存的桌面画面。"
                    }
                    (Source::Snapshot, _) => self.saved_status = "未能读取已保存的桌面画面。",
                }
                if source == Source::Snapshot {
                    self.checked_snapshot = true;
                }
                if self.missed >= 2 && !self.checked_snapshot {
                    ctx.link().send_message(Action::Poll(Source::Snapshot));
                } else if ctx.props().running {
                    let link = ctx.link().clone();
                    self.retry = Some(Timeout::new(1000, move || {
                        link.send_message(Action::Poll(Source::Live))
                    }));
                }
                true
            }
            Action::ImageError => {
                if let Some((_, source, _)) = self.frame.take() {
                    match source {
                        Source::Snapshot => self.saved_status = "已保存的画面无法显示。",
                        Source::Live => {
                            self.status = "实时画面无法显示。";
                            self.missed = 1;
                        }
                    }
                }
                true
            }
        }
    }

    fn destroy(&mut self, _: &Context<Self>) {
        self.cancel();
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let fallback = !ctx.props().running || self.missed > 0;
        html! {
            <section class="panel desktop-panel" aria-label="生物桌面">
                <header class="panel-heading"><h2>{"只读桌面"}</h2>
                    <div class="desktop-tools">
                        <span role="status" class={classes!("desktop-status", (self.status=="实时").then_some("live"))}>{self.status}</span>
                        <button type="button" class="text-link desktop-chat-shortcut" onclick={Callback::from(|_| {
                            if let Some(input) = web_sys::window().and_then(|w| w.document())
                                .and_then(|d| d.get_element_by_id("organism-chat-composer"))
                                .and_then(|el| el.dyn_into::<HtmlTextAreaElement>().ok())
                            {
                                let _ = input.focus();
                            }
                        })}>{"写消息"}{icon("arrow")}</button>
                    </div>
                </header>
                {if let Some((frame, source, time)) = &self.frame {
                    let caption = match source {
                        Source::Live if self.missed == 0 => "最近接收",
                        Source::Live => "上次实时画面",
                        Source::Snapshot => "已保存的桌面快照",
                    };
                    let url = frame.to_string();
                    html! {<>
                        <button type="button" class="desktop-screen" aria-label="放大桌面" aria-haspopup="dialog"
                            onclick={ctx.link().callback(|_|Action::Enlarge)}>
                            <img alt={caption} src={url.clone()} draggable="false" onerror={ctx.link().callback(|_|Action::ImageError)} />
                            <span class="desktop-expand">{icon("expand")}{"放大"}</span>
                        </button>
                        <div class="desktop-updated">{format!("{caption}{}", time.map(|v|format!(" · {}",date(v))).unwrap_or_default())}</div>
                        <dialog ref={self.preview.clone()} class="desktop-preview" aria-label="桌面大图"
                            onclick={ctx.link().batch_callback(|e:MouseEvent|
                                (e.target() == e.current_target()).then_some(Action::ClosePreview))}>
                            <div class="desktop-preview-body">
                                <header><h2>{"只读桌面"}</h2><button class="icon-button" aria-label="关闭大图" type="button"
                                    onclick={ctx.link().callback(|_|Action::ClosePreview)}>{icon("close")}</button></header>
                                <img alt={caption} src={url} draggable="false" />
                            </div>
                        </dialog>
                    </>}
                } else if !fallback {
                    html! {<div class="desktop-placeholder">{"正在等待第一帧桌面画面…"}</div>}
                } else {Html::default()}}
                {if fallback {html!{<div class="desktop-fallback">
                    {if !self.saved_status.is_empty() {html!{<p role="status">{self.saved_status}</p>}}
                        else if self.frame.is_none() && (!ctx.props().running || self.missed>=2) {html!{<p>{"正在查找已保存的桌面画面…"}</p>}}
                        else {Html::default()}}
                    {for ctx.props().children.iter()}
                    {if self.checked_snapshot && !ctx.props().running && !self.saved_status.is_empty() {
                        html!{<button class="text-link" disabled={self.request.is_some()} onclick={ctx.link().callback(|_|Action::Poll(Source::Snapshot))}>{"重新读取桌面快照"}</button>}
                    }else{Html::default()}}
                </div>}}else{Html::default()}}
            </section>
        }
    }
}

impl App {
    pub(super) fn desktop_view(&self, ctx: &Context<Self>, organism: &Value) -> Html {
        if self.demo {
            return empty("演示桌面", "桌面画面来自已连接的生物。");
        }
        let Some(token) = self.token.clone() else {
            return Html::default();
        };
        let id = s(organism, "id");
        let running = s(organism, "state") == "running";
        let key = format!(
            "{}/{id}/{}/{running}",
            self.epoch,
            n(organism, "generation")
        );
        html! {<Desktop {key} id={id.to_owned()} {token} {running} unauthorized={self.unauthorized(ctx)}>
            {self.resource_status(ctx, organism)}
            <div class="desktop-links">
                {for [("services","查看服务"),("files","浏览工作区"),("reports","查看报告")].map(|(tab,label)|html! {
                    <button class="button secondary" onclick={ctx.link().callback(move |_| Msg::Tab(tab.into()))}>{label}{icon("arrow")}</button>
                })}
            </div>
        </Desktop>}
    }
}
