//! Build a self-contained guest document. Parsing here uses an inert Rust DOM,
//! so parsing and resolving a guest URL cannot send any browser subrequests.
use super::*;
use gloo_net::http::Response;
use kuchiki::{NodeRef, traits::TendrilSink};
use regex::Regex;
use std::{collections::HashMap, rc::Rc, sync::LazyLock};
use url::Url;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AbortSignal, ReadableStreamDefaultReader, ReferrerPolicy, RequestCache, RequestCredentials,
    RequestMode, RequestRedirect,
};

const FILE_LIMIT: usize = 16 * 1024 * 1024;
const BUNDLE_LIMIT: usize = 32 * 1024 * 1024;
const FILE_COUNT: usize = 48;
static VIRTUAL_ROOT: LazyLock<Url> =
    LazyLock::new(|| Url::parse("https://preview.invalid/").unwrap());
static CSS_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i-u:url)\([ \t\r\n]*(?:"([^"]*)"|'([^']*)'|([^)'"]+?))[ \t\r\n]*\)"#).unwrap()
});
static CLOSE_STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new("(?i-u)</style").unwrap());
static CLOSE_SCRIPT: LazyLock<Regex> = LazyLock::new(|| Regex::new("(?i-u)</script").unwrap());

pub(super) struct Document {
    pub mode: String,
    pub partial: bool,
    pub missing: usize,
    pub body: PreviewBody,
}

pub(super) enum PreviewBody {
    Html(AttrValue),
    Text(String),
}

struct File {
    mime: String,
    bytes: Vec<u8>,
}

impl File {
    fn data_url(&self, mime: &str) -> String {
        format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }
}

fn path_for(raw: &str, base: &Url) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.starts_with(['#', '\\'])
        || raw.starts_with("//")
        || raw.contains('\\')
        || raw.chars().any(char::is_control)
        || Url::parse(raw).is_ok()
    {
        return None;
    }
    let mut url = base.join(raw).ok()?;
    if url.origin() != VIRTUAL_ROOT.origin() {
        return None;
    }
    // Reject encoded separators, double escaping, and control characters at the
    // same boundary as plain path segments. Dot segments are normalized by Url.
    for segment in url.path_segments()? {
        let lower = segment.to_ascii_lowercase();
        if ["%2f", "%5c", "%25", "%00", "%0d", "%0a"]
            .iter()
            .any(|part| lower.contains(part))
        {
            return None;
        }
    }
    url.set_fragment(None);
    Some(url)
}

struct Service<'a> {
    route: String,
    token: &'a str,
    signal: &'a AbortSignal,
    cache: HashMap<Url, Option<Rc<File>>>,
    fetch_remaining: usize,
    embed_remaining: usize,
    missing: usize,
}

impl Service<'_> {
    async fn get(&self, url: &Url, limit: usize) -> Result<(File, String, bool), String> {
        // The configured route, including id and port, is always fixed by the console.
        let path = &url[url::Position::BeforePath..];
        let request = format!("{}{path}", self.route);
        let response = Request::get(&request)
            .header("Authorization", &format!("Bearer {}", self.token))
            .credentials(RequestCredentials::Omit)
            .mode(RequestMode::SameOrigin)
            .redirect(RequestRedirect::Error)
            .referrer_policy(ReferrerPolicy::NoReferrer)
            .cache(RequestCache::NoStore)
            .abort_signal(Some(self.signal))
            .send()
            .await
            .map_err(|_| "无法读取服务，或请求已超时。".to_string())?;
        let status = response.status();
        let headers = response.headers();
        let mime = headers
            .get("content-type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !response.ok() {
            let reason = if mime == "application/json" {
                let message = read_bytes(&response, 8192)
                    .await
                    .ok()
                    .and_then(|data| serde_json::from_slice::<Value>(&data).ok());
                message.and_then(|v| {
                    v["error"]["message"]
                        .as_str()
                        .map(|s| s.chars().take(300).collect::<String>())
                })
            } else {
                None
            };
            return Err(reason.map_or_else(
                || format!("HTTP {status}"),
                |s| format!("HTTP {status} · {s}"),
            ));
        }
        let mode = headers
            .get("x-survive-preview")
            .unwrap_or_else(|| "live".into());
        let partial = headers.get("x-survive-preview-detail").as_deref() == Some("partial-capture");
        Ok((
            File {
                mime,
                bytes: read_bytes(&response, limit).await?,
            },
            mode,
            partial,
        ))
    }

    async fn asset(&mut self, raw: &str, base: &Url) -> Option<(Url, Rc<File>)> {
        let url = path_for(raw, base)?;
        let file = if let Some(hit) = self.cache.get(&url) {
            hit.clone()
        } else {
            if self.cache.len() >= FILE_COUNT || self.fetch_remaining == 0 || self.signal.aborted()
            {
                self.missing += 1;
                return None;
            }
            let file = self
                .get(&url, self.fetch_remaining.min(FILE_LIMIT))
                .await
                .ok()
                .map(|(f, _, _)| Rc::new(f));
            if let Some(f) = &file {
                self.fetch_remaining -= f.bytes.len();
            } else {
                self.missing += 1;
            }
            self.cache.insert(url.clone(), file.clone());
            file
        };
        // Pay for each embedding as well as each fetch, even for repeated URLs.
        let file = file?;
        if self.embed_remaining < file.bytes.len() {
            self.missing += 1;
            return None;
        }
        self.embed_remaining -= file.bytes.len();
        Some((url, file))
    }

    async fn image(&mut self, raw: &str, base: &Url) -> Option<String> {
        let (_, file) = self.asset(raw, base).await?;
        image_mime(&file.mime).then(|| file.data_url(&file.mime))
    }

    async fn styles(&mut self, css: &str, base: &Url) -> String {
        let mut output = String::new();
        let mut end = 0;
        for m in CSS_URL.captures_iter(css) {
            let whole = m.get(0).unwrap();
            output.push_str(&css[end..whole.start()]);
            let src = m.iter().skip(1).flatten().next().unwrap().as_str();
            if src.starts_with("data:") || src.starts_with('#') {
                output.push_str(whole.as_str());
            } else if let Some(data) = self.image(src, base).await {
                output.push_str("url(\"");
                output.push_str(&data);
                output.push_str("\")");
            } else {
                output.push_str("url(\"\")");
            }
            end = whole.end();
        }
        output.push_str(&css[end..]);
        output
    }

    async fn render(&mut self, html: &str) -> String {
        let doc = kuchiki::parse_html().one(html);
        let body = doc.select_first("body").ok().map(|e| e.as_node().clone());
        for node in doc.descendants().collect::<Vec<_>>() {
            let Some(el) = node.as_element() else {
                continue;
            };
            let attr = |key: &str| el.attributes.borrow().get(key).map(str::to_owned);
            el.attributes.borrow_mut().remove("srcset");
            if let Some(style) = attr("style")
                && style.to_ascii_lowercase().contains("url(")
            {
                let css = self.styles(&style, &VIRTUAL_ROOT).await;
                el.attributes.borrow_mut().insert("style", css);
            }
            match el.name.local.as_ref() {
                "base" | "iframe" | "object" | "embed" => node.detach(),
                "meta" if attr("http-equiv").is_some_and(|v| v.eq_ignore_ascii_case("refresh")) => {
                    node.detach()
                }
                "style" => {
                    let css = node.text_contents();
                    if css.to_ascii_lowercase().contains("url(") {
                        let css = self.styles(&css, &VIRTUAL_ROOT).await;
                        replace_text(
                            &node,
                            CLOSE_STYLE.replace_all(&css, r"<\/style").into_owned(),
                        );
                    }
                }
                "link" => {
                    if attr("rel").is_some_and(|v| {
                        v.split_whitespace()
                            .any(|s| s.eq_ignore_ascii_case("stylesheet"))
                    }) && let Some(href) = attr("href")
                        && let Some((base, file)) = self.asset(&href, &VIRTUAL_ROOT).await
                        && file.mime == "text/css"
                    {
                        let css = self
                            .styles(&String::from_utf8_lossy(&file.bytes), &base)
                            .await;
                        let mut name = el.name.clone();
                        name.local = "style".into();
                        let style = NodeRef::new_element(name, []);
                        if let Some(media) = attr("media") {
                            style
                                .as_element()
                                .unwrap()
                                .attributes
                                .borrow_mut()
                                .insert("media", media);
                        }
                        style.append(NodeRef::new_text(
                            CLOSE_STYLE.replace_all(&css, r"<\/style").into_owned(),
                        ));
                        node.insert_before(style);
                    }
                    node.detach();
                }
                "img" => {
                    if let Some(src) = attr("src")
                        && !src.starts_with("data:image/")
                    {
                        let url = self.image(&src, &VIRTUAL_ROOT).await;
                        let mut attrs = el.attributes.borrow_mut();
                        if let Some(data) = url {
                            attrs.insert("src", data);
                        } else {
                            attrs.remove("src");
                        }
                    }
                }
                "script" => {
                    if let Some(src) = attr("src") {
                        if let Some((_, file)) = self.asset(&src, &VIRTUAL_ROOT).await
                            && (file.mime.contains("javascript")
                                || file.mime.contains("ecmascript"))
                        {
                            el.attributes.borrow_mut().remove("src");
                            let js = String::from_utf8_lossy(&file.bytes);
                            replace_text(
                                &node,
                                CLOSE_SCRIPT.replace_all(&js, r"<\/script").into_owned(),
                            );
                            if attr("defer").is_some()
                                && let Some(body) = &body
                            {
                                body.append(node);
                            }
                        } else {
                            node.detach();
                        }
                    }
                }
                "a" | "area" => {
                    let Some(href) = attr("href") else { continue };
                    if href.starts_with('#') {
                        continue;
                    }
                    let path = href.split(['?', '#']).next().unwrap_or("");
                    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                    let downloadable = matches!(
                        ext.as_str(),
                        "zip"
                            | "tar"
                            | "gz"
                            | "tgz"
                            | "txt"
                            | "md"
                            | "csv"
                            | "tsv"
                            | "json"
                            | "pdf"
                            | "xls"
                            | "xlsx"
                            | "py"
                    );
                    if (attr("download").is_some() || downloadable)
                        && let Some((_, file)) = self.asset(&href, &VIRTUAL_ROOT).await
                    {
                        let mut attrs = el.attributes.borrow_mut();
                        attrs.insert("href", file.data_url("application/octet-stream"));
                        if attrs.get("download").is_none_or(str::is_empty) {
                            attrs.insert(
                                "download",
                                path.rsplit('/').next().unwrap_or("download").to_owned(),
                            );
                        }
                        attrs.remove("target");
                    } else {
                        let mut attrs = el.attributes.borrow_mut();
                        attrs.remove("href");
                        attrs.insert("title", "预览中未提供此链接".into());
                    }
                }
                _ => {}
            }
        }
        doc.to_string()
    }
}

fn replace_text(node: &NodeRef, text: String) {
    for child in node.children().collect::<Vec<_>>() {
        child.detach();
    }
    node.append(NodeRef::new_text(text));
}

fn image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/svg+xml"
            | "image/avif"
            | "image/x-icon"
            | "image/bmp"
    )
}

async fn read_bytes(response: &Response, limit: usize) -> Result<Vec<u8>, String> {
    let stream = response.body().ok_or("服务返回了空响应。")?;
    let err = "文件超过预览大小上限。";
    if response
        .headers()
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok())
        .is_some_and(|n| n > limit)
    {
        drop(stream.cancel());
        return Err(err.into());
    }
    let reader = stream
        .get_reader()
        .unchecked_into::<ReadableStreamDefaultReader>();
    let mut bytes = Vec::new();
    loop {
        let item = JsFuture::from(reader.read())
            .await
            .map_err(|_| "服务传输中断。")?;
        if js_sys::Reflect::get(&item, &"done".into())
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            return Ok(bytes);
        }
        let value = js_sys::Reflect::get(&item, &"value".into()).map_err(|_| "无法读取文件。")?;
        let chunk = js_sys::Uint8Array::new(&value);
        if chunk.length() as usize > limit - bytes.len() {
            drop(reader.cancel());
            return Err(err.into());
        }
        let start = bytes.len();
        bytes.resize(start + chunk.length() as usize, 0);
        chunk.copy_to(&mut bytes[start..]);
    }
}

pub(super) async fn prepare(
    id: &str,
    port: u16,
    token: &str,
    signal: &AbortSignal,
) -> Result<Document, String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        || port == 0
    {
        return Err("服务地址无效。".into());
    }
    let mut service = Service {
        route: format!("/services/{id}/{port}"),
        token,
        signal,
        cache: HashMap::new(),
        fetch_remaining: BUNDLE_LIMIT,
        embed_remaining: BUNDLE_LIMIT,
        missing: 0,
    };
    let (file, mode, partial) = service.get(&VIRTUAL_ROOT, FILE_LIMIT).await?;
    service.fetch_remaining -= file.bytes.len();
    service.embed_remaining -= file.bytes.len();
    let body = match file.mime.as_str() {
        "text/html" | "application/xhtml+xml" => PreviewBody::Html(
            service
                .render(&String::from_utf8_lossy(&file.bytes))
                .await
                .into(),
        ),
        mime if image_mime(mime) => PreviewBody::Html(
            format!(
                "<img alt='服务图片' style='max-width:100%' src='{}'>",
                file.data_url(mime)
            )
            .into(),
        ),
        mime if mime.starts_with("text/")
            || mime.ends_with("json")
            || mime.ends_with("xml")
            || mime.is_empty() =>
        {
            PreviewBody::Text(String::from_utf8_lossy(&file.bytes).into_owned())
        }
        mime => {
            return Err(format!(
                "此服务返回二进制文件（{mime}），无法作为网页预览。"
            ));
        }
    };
    if signal.aborted() {
        return Err("读取服务已取消或超时。".into());
    }
    Ok(Document {
        mode,
        partial,
        missing: service.missing,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_assets_resolve_inside_the_service_even_with_a_nested_stylesheet() {
        let style = path_for("css/site.css?v=2", &VIRTUAL_ROOT).unwrap();
        let image = path_for("../img/logo%20small.svg#shape", &style).unwrap();
        assert_eq!(image.path(), "/img/logo%20small.svg");
        assert_eq!(image.fragment(), None);
        assert_eq!(
            path_for("/assets/file.csv?q=1", &style).unwrap().query(),
            Some("q=1")
        );
        for src in [
            "https://example.org/site.css",
            "//example.org/site.css",
            "data:text/plain,test",
            "#local",
        ] {
            assert!(path_for(src, &VIRTUAL_ROOT).is_none());
        }
    }

    #[test]
    fn stylesheet_image_references_preserve_quoting_and_utf8_paths() {
        let css = ".a{background:URL('图 表.png')} .b{background:url(../icon.svg)}";
        let refs: Vec<_> = CSS_URL
            .captures_iter(css)
            .map(|cap| {
                cap.iter()
                    .skip(1)
                    .flatten()
                    .next()
                    .unwrap()
                    .as_str()
                    .to_owned()
            })
            .collect();
        assert_eq!(refs, ["图 表.png", "../icon.svg"]);
    }
}
