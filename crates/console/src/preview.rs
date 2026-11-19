use super::*;

use preview_document::{Document, PreviewBody};

fn mount_preview(event: Event, html: &str) {
    let frame = event.target_unchecked_into::<web_sys::HtmlIFrameElement>();
    // A service can navigate within its frame. Hand the document only to the
    // initial, fixed receiver, whose opaque origin requires targetOrigin "*".
    if !frame.has_attribute("data-delivered")
        && let Some(window) = frame.content_window()
    {
        let _ = frame.set_attribute("data-delivered", "");
        let _ = window.post_message(&html.into(), "*");
    }
}

pub(super) struct Preview {
    pub title: String,
    pub content: Content,
}

pub(super) enum Content {
    Loading(web_sys::AbortController),
    Service(Document),
    Error(String),
    Event(String),
}

impl Drop for Preview {
    fn drop(&mut self) {
        if let Content::Loading(controller) = &self.content {
            controller.abort();
        }
    }
}

impl App {
    pub(super) fn open_preview(&mut self, ctx: &Context<Self>, title: String, port: u16) {
        self.remember_focus();
        self.focus_target = Some(".preview-modal button".into());
        self.event_chunk = None;
        self.preview_request += 1;
        let request = self.preview_request;
        let content = if self.demo {
            Content::Service(Document { mode: "demo".into(), partial:false,missing:0,
                body:PreviewBody::Html("<style>body{font-family:system-ui;padding:2rem;color:#355a47}</style><h1>生长日志</h1><p>第一次观察已完成。</p>".into()) })
        } else if let (Some(id), Some(token), Ok(controller)) = (
            self.selected.clone(),
            self.token.clone(),
            web_sys::AbortController::new(),
        ) {
            let signal = controller.signal();
            let deadline = controller.clone();
            send_scoped(ctx, self.epoch, async move {
                let _timer = gloo_timers::callback::Timeout::new(30_000, move || deadline.abort());
                let result = preview_document::prepare(&id, port, &token, &signal).await;
                Msg::PreviewLoaded(request, result)
            });
            Content::Loading(controller)
        } else {
            Content::Error("无法连接所选服务。".into())
        };
        self.service_preview = Some(Preview { title, content });
    }

    pub(super) fn preview_view(&self, ctx: &Context<Self>, preview: &Preview) -> Html {
        let title = &preview.title;
        let (note, content) = match &preview.content {
            Content::Loading(_) => (
                "正在读取服务页面和关联文件…".into(),
                html! {<p class="preview-status" role="status">{"载入中…"}</p>},
            ),
            Content::Error(message) => (
                "服务预览不可用".into(),
                html! {<p class="preview-status" role="alert">{message}</p>},
            ),
            Content::Event(body) => (
                "事件原始数据 · 分段只读浏览".into(),
                html! {<pre class="file-content">{body}</pre>},
            ),
            Content::Service(document) => {
                let mut note = match document.mode.as_str() {
                    "archive" => "已归档 · 可操作页面内的本地工具、下载已保存的文件。需要后端的操作已离线。",
                    "demo" => "演示预览 · 本地示例",
                    _ => "在线服务 · 当前页面和文件来自运行中的服务。交互限于浏览器内，后端请求已隔离。",
                }.to_string();
                if document.partial {
                    note.push_str(" 此次归档仅保存了部分资源。");
                }
                if document.missing > 0 {
                    note.push_str(&format!(" 有 {} 项关联资源未载入。", document.missing));
                }
                let body = match &document.body {
                    PreviewBody::Text(text) => html! {<pre class="file-content">{text}</pre>},
                    PreviewBody::Html(html) => {
                        let html = html.clone();
                        html! {<iframe key={self.preview_request.to_string()} title={title.clone()} sandbox="allow-scripts allow-downloads" referrerpolicy="no-referrer" src="/console/preview.html" onload={Callback::from(move |event:Event|mount_preview(event,&html))}/>}
                    }
                };
                (note, body)
            }
        };
        html! {<div class="modal-backdrop"><section class="modal preview-modal" role="dialog" aria-modal="true" aria-label={title.clone()}
            onkeydown={ctx.link().batch_callback(|e:KeyboardEvent|{trap_focus(&e);(e.key()=="Escape").then_some(Msg::ClosePreview)})}>
            <div class="modal-heading"><h2>{title}</h2><button class="icon-button" aria-label="关闭预览" onclick={ctx.link().callback(|_|Msg::ClosePreview)}>{icon("close")}</button></div>
            <p class="preview-note">{note}</p>{content}
            {self.event_chunk.as_ref().map(|(id,next,total)|{let id=id.clone();let next=*next;html!{<div class="table-footer"><span>{format!("已读取至 {next} / {total} 字节")}</span><button class="text-link" disabled={next>=*total} onclick={ctx.link().callback(move|_|Msg::EventChunk(id.clone(),next))}>{"读取下一段 →"}</button></div>}}).unwrap_or_default()}
        </section></div>}
    }
}
