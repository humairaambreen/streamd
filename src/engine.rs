use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{
        header::{self as H, HeaderValue},
        HeaderMap, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use serde::Deserialize;
use std::{
    collections::{HashMap, VecDeque},
    fs::OpenOptions as StdOpenOptions,
    io::Read,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    process::Command,
    sync::{Mutex, Notify, Semaphore},
};

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CONNS: usize = 24;
const CHUNK_BYTES: u64 = 1 << 20; // 1 MiB chunks for instant seek & preview latency
const AHEAD_BYTES: u64 = 512 << 20; // 512 MiB prefetch window ahead of playhead
const READ_SLICE: usize = 256 * 1024;

#[derive(Clone, Default)]
pub struct OpenReq {
    pub url: String,
    pub referer: String,
    pub cookie: String,
    pub user_agent: String,
    pub seed_file: Option<PathBuf>,
}

#[derive(Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub media_url: String,
    pub is_local: bool,
}

#[derive(Clone, Default)]
pub struct SessionStatus {
    pub downloaded: u64,
    pub total: u64,
    pub percent: f32,
    pub speed_bps: u64,
}

#[derive(Clone, Debug)]
pub struct OnlineSub {
    pub name: String,
    pub lang: String,
    pub downloads: u64,
    pub download_url: String,
}

struct ChunkState {
    chunks: Vec<u8>,
    urgent: VecDeque<usize>,
    done_chunks: usize,
    consecutive_failures: u32,
    error: Option<String>,
}

struct Session {
    orig_url: String,
    resolved_url: Mutex<String>,
    headers: HeaderMap,
    len: AtomicU64,
    range_ok: bool,
    chunk: u64,
    ahead: u64,
    bg_base: String,
    path: PathBuf,
    file: Arc<std::fs::File>,
    state: Mutex<ChunkState>,
    seq_written: AtomicU64,
    seq_eof: AtomicBool,
    downloaded_bytes: AtomicU64,
    started_at: Instant,
    work_notify: Notify,
    ready_notify: Notify,
    head: AtomicU64,
    dead: AtomicBool,
    thumb_cache: Mutex<HashMap<u64, Bytes>>,
}

pub struct Engine {
    rt: tokio::runtime::Runtime,
    port: u16,
    http: reqwest::Client,
    dir: PathBuf,
    sess: SrvState,
    thumb_sem: Semaphore,
}

// ---------- cross-platform positional file I/O ----------
#[cfg(unix)]
fn write_all_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write zero",
            ));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof",
            ));
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

// ---------- HTTP helpers ----------
fn build_headers(o: &OpenReq) -> anyhow::Result<HeaderMap> {
    let mut hm = HeaderMap::new();
    let ua = if o.user_agent.trim().is_empty() {
        BROWSER_UA
    } else {
        o.user_agent.trim()
    };
    hm.insert(H::USER_AGENT, HeaderValue::from_str(ua)?);
    hm.insert(H::ACCEPT, HeaderValue::from_static("*/*"));
    hm.insert(
        H::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9"),
    );
    if !o.referer.trim().is_empty() {
        hm.insert(H::REFERER, HeaderValue::from_str(o.referer.trim())?);
    }
    if !o.cookie.trim().is_empty() {
        hm.insert(H::COOKIE, HeaderValue::from_str(o.cookie.trim())?);
    }
    Ok(hm)
}

fn origin_referer(url: &reqwest::Url) -> Option<HeaderValue> {
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    HeaderValue::from_str(&format!("{}://{host}{port}/", url.scheme())).ok()
}

fn parse_content_range_total(header_val: Option<&HeaderValue>) -> Option<u64> {
    let s = header_val?.to_str().ok()?;
    let total = s.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

fn is_html_response(headers: &HeaderMap) -> bool {
    headers
        .get(H::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let l = ct.to_ascii_lowercase();
            l.contains("text/html") || l.contains("application/xhtml")
        })
        .unwrap_or(false)
}

fn parse_range(range_hdr: &str, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let spec = range_hdr.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (x, y) = first.split_once('-')?;
    let x = x.trim();
    let y = y.trim();
    if x.is_empty() {
        let suffix: u64 = y.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        Some((len.saturating_sub(suffix), len - 1))
    } else {
        let lo: u64 = x.parse().ok()?;
        if lo >= len {
            return None;
        }
        let hi = if y.is_empty() {
            len - 1
        } else {
            y.parse::<u64>().ok()?.min(len - 1)
        };
        if lo > hi {
            return None;
        }
        Some((lo, hi))
    }
}

fn sanitize_snippet(raw: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(160).collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

async fn probe_url(
    http: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    custom_ua: bool,
) -> anyhow::Result<(reqwest::Response, HeaderMap)> {
    let mut active_headers = headers.clone();
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| anyhow::anyhow!("Invalid URL. Paste a full http:// or https:// link."))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "Only http:// and https:// links are supported."
    );

    let mut resp = http
        .get(url)
        .headers(active_headers.clone())
        .header(H::RANGE, "bytes=0-1023")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Could not connect: {e}"))?;

    if resp.status() == StatusCode::FORBIDDEN && !active_headers.contains_key(H::REFERER) {
        if let Some(referer) = origin_referer(&parsed) {
            let mut retry_headers = active_headers.clone();
            retry_headers.insert(H::REFERER, referer);
            if let Ok(r2) = http
                .get(url)
                .headers(retry_headers.clone())
                .header(H::RANGE, "bytes=0-1023")
                .send()
                .await
            {
                if r2.status().is_success() || r2.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                    active_headers = retry_headers;
                    resp = r2;
                }
            }
        }
    }

    if resp.status() == StatusCode::FORBIDDEN && !custom_ua {
        for ua in ["VLC/3.0.21 LibVLC/3.0.21", "curl/8.7.1"] {
            let mut retry_headers = HeaderMap::new();
            retry_headers.insert(H::USER_AGENT, HeaderValue::from_static(ua));
            retry_headers.insert(H::ACCEPT, HeaderValue::from_static("*/*"));
            if let Some(r) = active_headers.get(H::REFERER) {
                retry_headers.insert(H::REFERER, r.clone());
            }
            if let Some(c) = active_headers.get(H::COOKIE) {
                retry_headers.insert(H::COOKIE, c.clone());
            }
            if let Ok(r2) = http
                .get(url)
                .headers(retry_headers.clone())
                .header(H::RANGE, "bytes=0-1023")
                .send()
                .await
            {
                if r2.status().is_success() || r2.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                    active_headers = retry_headers;
                    resp = r2;
                    break;
                }
            }
        }
    }

    if matches!(
        resp.status(),
        StatusCode::FORBIDDEN | StatusCode::RANGE_NOT_SATISFIABLE | StatusCode::BAD_REQUEST
    ) {
        if let Ok(r2) = http.get(url).headers(active_headers.clone()).send().await {
            if r2.status().is_success() || resp.status() != StatusCode::FORBIDDEN {
                resp = r2;
            }
        }
    }

    Ok((resp, active_headers))
}

async fn fetch_chunk(
    http: &reqwest::Client,
    s: &Session,
    lo: u64,
    hi: u64,
) -> anyhow::Result<Bytes> {
    let expected = hi - lo + 1;
    let url = s.resolved_url.lock().await.clone();
    let resp = http
        .get(&url)
        .headers(s.headers.clone())
        .header(H::RANGE, format!("bytes={lo}-{hi}"))
        .send()
        .await?;

    let resp =
        if matches!(resp.status(), StatusCode::FORBIDDEN | StatusCode::GONE) && url != s.orig_url {
            let r2 = http
                .get(&s.orig_url)
                .headers(s.headers.clone())
                .header(H::RANGE, format!("bytes={lo}-{hi}"))
                .send()
                .await?;
            if r2.status() == StatusCode::PARTIAL_CONTENT {
                *s.resolved_url.lock().await = r2.url().to_string();
            }
            r2
        } else {
            resp
        };

    anyhow::ensure!(
        resp.status() == StatusCode::PARTIAL_CONTENT,
        "expected HTTP 206, got {}",
        resp.status()
    );
    let data = resp.bytes().await?;
    anyhow::ensure!(data.len() as u64 == expected, "short chunk");
    Ok(data)
}

async fn worker(http: reqwest::Client, s: Arc<Session>) {
    let total_len = s.len.load(Relaxed);
    while !s.dead.load(Relaxed) {
        let pick = {
            let mut st = s.state.lock().await;
            let mut chosen = None;
            while let Some(u) = st.urgent.pop_front() {
                if u < st.chunks.len() && st.chunks[u] == 0 {
                    st.chunks[u] = 1;
                    chosen = Some(u);
                    break;
                }
            }
            if chosen.is_none() {
                let h = (s.head.load(Relaxed) / s.chunk) as usize;
                let end = (h + (s.ahead / s.chunk).max(1) as usize).min(st.chunks.len());
                if let Some(i) = (h..end).find(|&i| st.chunks[i] == 0) {
                    st.chunks[i] = 1;
                    chosen = Some(i);
                }
            }
            chosen
        };

        let Some(i) = pick else {
            let notified = s.work_notify.notified();
            let _ = tokio::time::timeout(Duration::from_millis(250), notified).await;
            continue;
        };

        let lo = i as u64 * s.chunk;
        let hi = (lo + s.chunk).min(total_len) - 1;
        let mut ok = false;
        let mut last_err = String::new();

        for attempt in 0..5u64 {
            if s.dead.load(Relaxed) {
                return;
            }
            match fetch_chunk(&http, &s, lo, hi).await {
                Ok(data) => {
                    let file = s.file.clone();
                    match tokio::task::spawn_blocking(move || write_all_at(&file, &data, lo)).await
                    {
                        Ok(Ok(())) => {
                            ok = true;
                            break;
                        }
                        Ok(Err(e)) => last_err = format!("disk write: {e}"),
                        Err(e) => last_err = format!("write task: {e}"),
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
            let delay = (250 * (1u64 << attempt.min(3))).min(2500);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        {
            let mut st = s.state.lock().await;
            if ok {
                st.chunks[i] = 2;
                st.done_chunks += 1;
                st.consecutive_failures = 0;
                s.downloaded_bytes.fetch_add(hi - lo + 1, Relaxed);
            } else {
                st.chunks[i] = 0;
                st.consecutive_failures += 1;
                if st.consecutive_failures >= 12 && st.error.is_none() {
                    st.error = Some(format!("Download failed: {last_err}"));
                }
            }
        }
        s.ready_notify.notify_waiters();
    }
}

async fn sequential_downloader(s: Arc<Session>, resp: reqwest::Response) {
    let mut stream = resp.bytes_stream();
    let mut offset: u64 = 0;
    let mut since_notify: u64 = 0;
    let mut err_msg: Option<String> = None;

    while !s.dead.load(Relaxed) {
        match stream.next().await {
            Some(Ok(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                let n = chunk.len() as u64;
                let file = s.file.clone();
                match tokio::task::spawn_blocking(move || write_all_at(&file, &chunk, offset)).await
                {
                    Ok(Ok(())) => {
                        offset += n;
                        since_notify += n;
                        s.seq_written.store(offset, Relaxed);
                        s.downloaded_bytes.store(offset, Relaxed);
                        if since_notify >= 128 * 1024 {
                            since_notify = 0;
                            s.ready_notify.notify_waiters();
                        }
                    }
                    Ok(Err(e)) => {
                        err_msg = Some(format!("disk write: {e}"));
                        break;
                    }
                    Err(e) => {
                        err_msg = Some(format!("write task: {e}"));
                        break;
                    }
                }
            }
            Some(Err(e)) => {
                err_msg = Some(format!("stream error: {e}"));
                break;
            }
            None => break,
        }
    }

    if let Some(e) = err_msg {
        let mut st = s.state.lock().await;
        if st.error.is_none() {
            st.error = Some(e);
        }
    } else if s.len.load(Relaxed) == 0 {
        s.len.store(offset, Relaxed);
    }
    s.seq_eof.store(true, Relaxed);
    s.ready_notify.notify_waiters();
}

// ---------- loopback range HTTP server (feeds libmpv directly) ----------
#[derive(Deserialize, Default)]
struct MediaQuery {
    bg: Option<u8>,
}

type SrvState = Arc<Mutex<HashMap<String, Arc<Session>>>>;

async fn media_head(State(sess): State<SrvState>, Path(id): Path<String>) -> Response {
    let Some(s) = sess.lock().await.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = s.len.load(Relaxed);
    let mut rb = Response::builder()
        .status(StatusCode::OK)
        .header(H::ACCEPT_RANGES, "bytes");
    if len > 0 {
        rb = rb.header(H::CONTENT_LENGTH, len);
    }
    rb.body(Body::empty()).unwrap()
}

async fn media_get(
    State(sess): State<SrvState>,
    Path(id): Path<String>,
    Query(q): Query<MediaQuery>,
    h: HeaderMap,
) -> Response {
    let Some(s) = sess.lock().await.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let is_bg = q.bg.unwrap_or(0) != 0;
    let known_len = s.len.load(Relaxed);
    let range_hdr = h.get(H::RANGE).and_then(|v| v.to_str().ok());

    let (lo, hi, is_partial) = if known_len > 0 {
        if let Some(rh) = range_hdr {
            match parse_range(rh, known_len) {
                Some((l, r)) => (l, r, true),
                None => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(H::CONTENT_RANGE, format!("bytes */{known_len}"))
                        .body(Body::empty())
                        .unwrap();
                }
            }
        } else {
            (0, known_len - 1, false)
        }
    } else {
        (0, u64::MAX - 1, false)
    };

    let st = futures_util::stream::unfold((s.clone(), lo), move |(s, pos)| async move {
        if pos > hi || s.dead.load(Relaxed) {
            return None;
        }
        if !is_bg {
            s.head.store(pos, Relaxed);
        }

        if s.range_ok {
            let i = (pos / s.chunk) as usize;
            loop {
                let notified = s.ready_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                if s.dead.load(Relaxed) {
                    return None;
                }
                let need_wake = {
                    let mut st = s.state.lock().await;
                    if st.error.is_some() {
                        let msg = st.error.clone().unwrap_or_default();
                        drop(st);
                        return Some((
                            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, msg)),
                            (s.clone(), hi.saturating_add(1)),
                        ));
                    }
                    if i >= st.chunks.len() {
                        return None;
                    }
                    if st.chunks[i] == 2 {
                        break;
                    }
                    if st.chunks[i] == 0 && !st.urgent.contains(&i) {
                        st.urgent.push_back(i);
                        true
                    } else {
                        false
                    }
                };
                if need_wake {
                    s.work_notify.notify_one();
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), notified).await;
            }
            if !is_bg {
                s.work_notify.notify_waiters();
            }

            let chunk_end = ((i as u64 + 1) * s.chunk - 1).min(hi);
            let slice_end = (pos + READ_SLICE as u64 - 1).min(chunk_end);
            let count = (slice_end - pos + 1) as usize;
            let file = s.file.clone();
            let read_res = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; count];
                read_exact_at(&file, &mut buf, pos)?;
                Ok::<_, std::io::Error>(buf)
            })
            .await;

            match read_res {
                Ok(Ok(buf)) => Some((Ok(Bytes::from(buf)), (s, slice_end + 1))),
                _ => None,
            }
        } else {
            let avail_end = loop {
                let notified = s.ready_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                if s.dead.load(Relaxed) {
                    return None;
                }
                let written = s.seq_written.load(Relaxed);
                let eof = s.seq_eof.load(Relaxed);
                if written > pos {
                    break (written - 1).min(hi);
                }
                if eof {
                    return None;
                }
                if s.state.lock().await.error.is_some() {
                    return None;
                }
                let _ = tokio::time::timeout(Duration::from_secs(1), notified).await;
            };

            let slice_end = (pos + READ_SLICE as u64 - 1).min(avail_end);
            let count = (slice_end - pos + 1) as usize;
            let file = s.file.clone();
            let read_res = tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; count];
                read_exact_at(&file, &mut buf, pos)?;
                Ok::<_, std::io::Error>(buf)
            })
            .await;

            match read_res {
                Ok(Ok(buf)) => Some((Ok(Bytes::from(buf)), (s, slice_end + 1))),
                _ => None,
            }
        }
    });

    let mut rb = Response::builder().header(H::ACCEPT_RANGES, "bytes");
    if known_len > 0 {
        rb = rb.header(H::CONTENT_LENGTH, hi - lo + 1);
    }
    if is_partial {
        rb = rb
            .status(StatusCode::PARTIAL_CONTENT)
            .header(H::CONTENT_RANGE, format!("bytes {lo}-{hi}/{known_len}"));
    }
    rb.body(Body::from_stream(st)).unwrap()
}

impl Engine {
    pub fn start() -> anyhow::Result<Arc<Self>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let dir = std::env::temp_dir().join("streamd");
        std::fs::create_dir_all(&dir)?;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                std::fs::remove_file(e.path()).ok();
            }
        }

        let http = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .cookie_store(true)
            .connect_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(CONNS)
            .build()?;

        let sess_map: SrvState = Arc::new(Mutex::new(HashMap::new()));
        let port = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
            let port = listener.local_addr()?.port();
            let app = Router::new()
                .route("/s/{id}/media", get(media_get).head(media_head))
                .with_state(sess_map.clone());
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            anyhow::Ok(port)
        })?;

        Ok(Arc::new(Self {
            rt,
            port,
            http,
            dir,
            sess: sess_map,
            thumb_sem: Semaphore::new(3),
        }))
    }

    pub fn open(&self, req: OpenReq) -> anyhow::Result<SessionMeta> {
        let http = self.http.clone();
        let dir = self.dir.clone();
        let port = self.port;
        let sess_map = self.sess.clone();

        self.rt.block_on(async move {
            let url = req.url.trim().to_string();
            let custom_ua = !req.user_agent.trim().is_empty();
            let init_headers = build_headers(&req)?;
            let (resp, active_headers) = probe_url(&http, &url, &init_headers, custom_ua).await?;

            let status = resp.status();
            if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
                let server = resp
                    .headers()
                    .get(H::SERVER)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("unknown")
                    .to_string();
                let snippet = sanitize_snippet(&resp.text().await.unwrap_or_default());
                let mut detail = format!("HTTP {status} (server: {server}).");
                if !snippet.is_empty() {
                    detail += &format!(" \"{snippet}\"");
                }
                anyhow::bail!("{detail}");
            }
            if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
                anyhow::bail!("Video not found or link expired (HTTP {status}).");
            }
            anyhow::ensure!(
                status == StatusCode::PARTIAL_CONTENT || status == StatusCode::OK,
                "Server returned HTTP {status}"
            );
            if is_html_response(resp.headers()) {
                anyhow::bail!("URL returned an HTML webpage instead of a direct video file.");
            }

            let resolved_url = resp.url().to_string();
            let range_ok = status == StatusCode::PARTIAL_CONTENT;

            let total_len: u64 = if range_ok {
                if let Some(t) = parse_content_range_total(resp.headers().get(H::CONTENT_RANGE)) {
                    t
                } else {
                    let head_r = http
                        .head(&resolved_url)
                        .headers(active_headers.clone())
                        .send()
                        .await?;
                    head_r
                        .headers()
                        .get(H::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or_else(|| anyhow::anyhow!("Server did not report file size"))?
                }
            } else {
                resp.headers()
                    .get(H::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
            };

            if range_ok {
                anyhow::ensure!(total_len > 0, "Remote file is empty");
            }

            let id = format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            );
            let path = dir.join(format!("{id}.part"));
            let std_file = StdOpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            if range_ok {
                std_file.set_len(total_len)?;
            }

            let num_chunks = if range_ok {
                total_len.div_ceil(CHUNK_BYTES) as usize
            } else {
                0
            };
            let mut chunks_vec = vec![0u8; num_chunks];
            let mut done_chunks = 0usize;
            let mut seeded_bytes = 0u64;

            // If user provided a partial/cancelled download file (.part / .crdownload), seed complete chunks!
            if range_ok {
                if let Some(seed_path) = &req.seed_file {
                    if let Ok(mut src) = std::fs::File::open(seed_path) {
                        let seed_len = src.metadata().map(|m| m.len()).unwrap_or(0).min(total_len);
                        let full_chunks = (seed_len / CHUNK_BYTES) as usize;
                        let mut buf = vec![0u8; CHUNK_BYTES as usize];
                        for ci in 0..full_chunks.min(num_chunks) {
                            if src.read_exact(&mut buf).is_ok() {
                                let off = ci as u64 * CHUNK_BYTES;
                                if write_all_at(&std_file, &buf, off).is_ok() {
                                    chunks_vec[ci] = 2;
                                    done_chunks += 1;
                                    seeded_bytes += CHUNK_BYTES;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
            }

            let mut urgent = VecDeque::new();
            if range_ok && num_chunks > 0 {
                if chunks_vec[0] == 0 {
                    urgent.push_back(0);
                }
                if num_chunks > 1 && chunks_vec[num_chunks - 1] == 0 {
                    urgent.push_back(num_chunks - 1);
                }
            }

            let media_url = format!("http://127.0.0.1:{port}/s/{id}/media");
            let bg_base = format!("{media_url}?bg=1");

            let s = Arc::new(Session {
                orig_url: url.clone(),
                resolved_url: Mutex::new(resolved_url),
                headers: active_headers,
                len: AtomicU64::new(total_len),
                range_ok,
                chunk: CHUNK_BYTES,
                ahead: AHEAD_BYTES,
                bg_base,
                path,
                file: Arc::new(std_file),
                state: Mutex::new(ChunkState {
                    chunks: chunks_vec,
                    urgent,
                    done_chunks,
                    consecutive_failures: 0,
                    error: None,
                }),
                seq_written: AtomicU64::new(0),
                seq_eof: AtomicBool::new(false),
                downloaded_bytes: AtomicU64::new(seeded_bytes),
                started_at: Instant::now(),
                work_notify: Notify::new(),
                ready_notify: Notify::new(),
                head: AtomicU64::new(0),
                dead: AtomicBool::new(false),
                thumb_cache: Mutex::new(HashMap::new()),
            });

            sess_map.lock().await.insert(id.clone(), s.clone());

            if range_ok {
                drop(resp);
                for _ in 0..CONNS {
                    tokio::spawn(worker(http.clone(), s.clone()));
                }
            } else {
                tokio::spawn(sequential_downloader(s.clone(), resp));
            }

            let raw_name = url
                .split('?')
                .next()
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("video");
            let name = percent_decode(raw_name);

            Ok(SessionMeta {
                id,
                name,
                media_url,
                is_local: false,
            })
        })
    }

    pub fn open_local(&self, path: PathBuf) -> anyhow::Result<SessionMeta> {
        anyhow::ensure!(path.is_file(), "Selected file does not exist.");
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
            .to_string();
        Ok(SessionMeta {
            id: format!(
                "local_{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ),
            name,
            media_url: path.to_string_lossy().into_owned(),
            is_local: true,
        })
    }

    /// Free, zero-API-key online subtitle search via rest.opensubtitles.org.
    pub fn search_subs(&self, query: &str, lang: &str) -> anyhow::Result<Vec<OnlineSub>> {
        let http = self.http.clone();
        let q = query.trim().replace(' ', "%20");
        let l = match lang.trim().to_ascii_lowercase().as_str() {
            "en" | "eng" | "" => "eng",
            "hi" | "hin" => "hin",
            "es" | "spa" => "spa",
            "fr" | "fre" => "fre",
            "de" | "ger" => "ger",
            "ar" | "ara" => "ara",
            "ja" | "jpn" => "jpn",
            "ko" | "kor" => "kor",
            "pt" | "por" => "por",
            other => other,
        }
        .to_string();

        self.rt.block_on(async move {
            let url = format!("https://rest.opensubtitles.org/search/query-{q}/sublanguageid-{l}");
            let arr: Vec<serde_json::Value> = http
                .get(&url)
                .header(H::USER_AGENT, "TemporaryUserAgent")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let mut out: Vec<OnlineSub> = arr
                .into_iter()
                .filter_map(|v| {
                    let dl = v["SubDownloadLink"].as_str()?.to_string();
                    let name = v["SubFileName"]
                        .as_str()
                        .unwrap_or("subtitle.srt")
                        .to_string();
                    let lang = v["ISO639"].as_str().unwrap_or(&l).to_string();
                    let downloads = v["SubDownloadsCnt"]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    Some(OnlineSub {
                        name,
                        lang,
                        downloads,
                        download_url: dl,
                    })
                })
                .collect();
            out.sort_by(|a, b| b.downloads.cmp(&a.downloads));
            out.truncate(12);
            Ok(out)
        })
    }

    /// Downloads and decompresses a `.gz` subtitle file into the temp directory (auto-wiped on exit).
    pub fn download_sub(&self, download_url: &str) -> anyhow::Result<PathBuf> {
        let http = self.http.clone();
        let dir = self.dir.clone();
        let url = download_url.to_string();
        self.rt.block_on(async move {
            let raw = http
                .get(&url)
                .header(H::USER_AGENT, "TemporaryUserAgent")
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            let mut decoded = Vec::new();
            if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
                GzDecoder::new(&raw[..]).read_to_end(&mut decoded)?;
            } else {
                decoded.extend_from_slice(&raw);
            }
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let path = dir.join(format!("sub_{ts:x}.srt"));
            tokio::fs::write(&path, &decoded).await?;
            Ok(path)
        })
    }

    pub fn status(&self, id: &str) -> Option<SessionStatus> {
        let sess_map = self.sess.clone();
        let id = id.to_string();
        self.rt.block_on(async move {
            let s = sess_map.lock().await.get(&id)?.clone();
            let downloaded = s.downloaded_bytes.load(Relaxed);
            let total = s.len.load(Relaxed);
            let elapsed = s.started_at.elapsed().as_secs_f64().max(0.1);
            let speed_bps = (downloaded as f64 / elapsed) as u64;
            let percent = if total > 0 {
                ((downloaded as f64 / total as f64) * 100.0).min(100.0) as f32
            } else if s.seq_eof.load(Relaxed) {
                100.0
            } else {
                0.0
            };
            Some(SessionStatus {
                downloaded,
                total,
                percent,
                speed_bps,
            })
        })
    }

    /// Fast keyframe thumbnail extractor (works for both remote sessions and local files).
    pub fn fetch_thumb(
        &self,
        id: &str,
        local_path: Option<&str>,
        bucket_sec: u64,
    ) -> Option<Bytes> {
        let sess_map = self.sess.clone();
        let id = id.to_string();
        let local_owned = local_path.map(str::to_string);
        self.rt.block_on(async move {
            let (input_src, sess_opt) = if let Some(lp) = local_owned {
                (lp, None)
            } else {
                let s = sess_map.lock().await.get(&id)?.clone();
                if let Some(hit) = s.thumb_cache.lock().await.get(&bucket_sec).cloned() {
                    return Some(hit);
                }
                let fully_cached = if s.range_ok {
                    let st = s.state.lock().await;
                    !st.chunks.is_empty() && st.done_chunks == st.chunks.len()
                } else {
                    s.seq_eof.load(Relaxed)
                };
                let src = if fully_cached {
                    s.path.to_string_lossy().into_owned()
                } else {
                    s.bg_base.clone()
                };
                (src, Some(s))
            };

            let _permit = self.thumb_sem.acquire().await.ok()?;
            let mut cmd = Command::new("ffmpeg");
            cmd.args([
                "-v",
                "error",
                "-noaccurate_seek",
                "-analyzeduration",
                "200000",
                "-probesize",
                "300000",
                "-ss",
                &bucket_sec.to_string(),
                "-i",
                &input_src,
                "-an",
                "-sn",
                "-dn",
                "-frames:v",
                "1",
                "-q:v",
                "9",
                "-vf",
                "scale=240:-2",
                "-f",
                "mjpeg",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true);
            let out = cmd.output().await.ok()?;
            if out.status.success() && !out.stdout.is_empty() {
                let b = Bytes::from(out.stdout);
                if let Some(s) = sess_opt {
                    s.thumb_cache.lock().await.insert(bucket_sec, b.clone());
                }
                Some(b)
            } else {
                None
            }
        })
    }

    pub fn close(&self, id: &str) {
        let sess_map = self.sess.clone();
        let id = id.to_string();
        self.rt.block_on(async move {
            if let Some(s) = sess_map.lock().await.remove(&id) {
                s.dead.store(true, Relaxed);
                s.work_notify.notify_waiters();
                s.ready_notify.notify_waiters();
                tokio::fs::remove_file(&s.path).await.ok();
            }
        });
    }

    pub fn close_all(&self) {
        let sess_map = self.sess.clone();
        let dir = self.dir.clone();
        self.rt.block_on(async move {
            let items: Vec<Arc<Session>> = sess_map.lock().await.drain().map(|(_, v)| v).collect();
            for s in items {
                s.dead.store(true, Relaxed);
                s.work_notify.notify_waiters();
                s.ready_notify.notify_waiters();
                tokio::fs::remove_file(&s.path).await.ok();
            }
            // Sweep any downloaded temp subtitles too
            if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    tokio::fs::remove_file(e.path()).await.ok();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_range() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=1000-1005", 1000), None);
    }

    #[test]
    fn test_percent_decode() {
        assert_eq!(
            percent_decode("Sample%20Video%201080p.mkv"),
            "Sample Video 1080p.mkv"
        );
    }
}
