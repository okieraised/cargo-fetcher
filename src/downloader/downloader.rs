use crate::downloader::resource::{Resource, Status, Summary};
use anyhow::{anyhow, Context, Result};
use futures::{future::try_join_all, stream, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::{
    header::{
        HeaderMap, HeaderValue, IntoHeaderName, ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_LENGTH,
        CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE,
    },
    StatusCode,
};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use reqwest_tracing::TracingMiddleware;
use std::{fs, path::PathBuf, sync::Arc};
use tokio::{
    fs::OpenOptions,
    io::{AsyncSeekExt, AsyncWriteExt, BufWriter},
    sync::Semaphore,
    time::{timeout, Duration},
};
use std::io::SeekFrom;
use uuid::Uuid;

pub struct TimeTrace;

#[derive(Debug, Clone)]
pub struct Downloader {
    directory: PathBuf,
    retries: u32,
    concurrent_downloads: usize,
    concurrent_chunk: usize,
    chunk_size: u64,
    style_options: StyleOptions,
    headers: Option<HeaderMap>,
}

#[derive(Debug, Clone)]
pub struct StyleOptions {
    main: ProgressBarOpts,
    child: ProgressBarOpts,
}

impl Default for StyleOptions {
    fn default() -> Self {
        Self {
            main: ProgressBarOpts {
                template: Some(ProgressBarOpts::TEMPLATE_BAR_WITH_POSITION.into()),
                progress_chars: Some(ProgressBarOpts::CHARS_LINE.into()),
                enabled: true,
                clear: false,
            },
            child: ProgressBarOpts::with_pip_style(),
        }
    }
}

impl StyleOptions {
    pub fn new(main: ProgressBarOpts, child: ProgressBarOpts) -> Self {
        Self { main, child }
    }
    pub fn set_main(&mut self, main: ProgressBarOpts) {
        self.main = main;
    }
    pub fn set_child(&mut self, child: ProgressBarOpts) {
        self.child = child;
    }
    pub fn is_enabled(&self) -> bool {
        self.main.enabled || self.child.enabled
    }
}

#[derive(Debug, Clone)]
pub struct ProgressBarOpts {
    template: Option<String>,
    progress_chars: Option<String>,
    enabled: bool,
    clear: bool,
}

impl Default for ProgressBarOpts {
    fn default() -> Self {
        Self {
            template: None,
            progress_chars: None,
            enabled: true,
            clear: true,
        }
    }
}

impl ProgressBarOpts {
    pub const TEMPLATE_BAR_WITH_POSITION: &'static str =
        "{bar:40.blue} {pos:>}/{len} ({percent}%) eta {eta_precise:.blue}";
    pub const TEMPLATE_PIP: &'static str =
        "{bar:40.green/black} {bytes:>11.green}/{total_bytes:<11.green} {bytes_per_sec:>13.red} eta {eta:.blue}";
    pub const CHARS_LINE: &'static str = "━╾╴─";

    pub fn new(
        template: Option<String>,
        progress_chars: Option<String>,
        enabled: bool,
        clear: bool,
    ) -> Self {
        Self {
            template,
            progress_chars,
            enabled,
            clear,
        }
    }

    pub fn to_progress_style(self) -> ProgressStyle {
        let mut style = ProgressStyle::default_bar();
        if let Some(template) = self.template {
            style = style.template(&template).unwrap();
        }
        if let Some(progress_chars) = self.progress_chars {
            style = style.progress_chars(&progress_chars);
        }
        style
    }

    pub fn to_progress_bar(self, len: u64) -> ProgressBar {
        if !self.enabled {
            return ProgressBar::hidden();
        }
        let style = self.to_progress_style();
        ProgressBar::new(len).with_style(style)
    }

    pub fn with_pip_style() -> Self {
        Self {
            template: Some(ProgressBarOpts::TEMPLATE_PIP.into()),
            progress_chars: Some(ProgressBarOpts::CHARS_LINE.into()),
            enabled: true,
            clear: true,
        }
    }

    pub fn set_clear(&mut self, clear: bool) {
        self.clear = clear;
    }

    pub fn hidden() -> Self {
        Self {
            enabled: false,
            ..ProgressBarOpts::default()
        }
    }
}

impl Downloader {
    const DEFAULT_RETRIES: u32 = 3;
    const DEFAULT_CONCURRENT_DOWNLOADS: usize = 32;
    const DEFAULT_CONCURRENT_CHUNK: usize = 8;
    const DEFAULT_CHUNK_SIZE: u64 = 10 * 1024 * 1024;
    const CHUNK_TIMEOUT_SECS: u64 = 30;

    pub async fn download(&self, downloads: &[Resource], insecure: Option<bool>) -> Vec<Summary> {
        self.download_inner(downloads, None, insecure).await
    }

    pub async fn download_with_proxy(
        &self,
        downloads: &[Resource],
        proxy: reqwest::Proxy,
        insecure: Option<bool>,
    ) -> Vec<Summary> {
        self.download_inner(downloads, Some(proxy), insecure).await
    }

    pub async fn download_inner(
        &self,
        downloads: &[Resource],
        proxy: Option<reqwest::Proxy>,
        insecure: Option<bool>,
    ) -> Vec<Summary> {
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(self.retries);

        // Build inner client with enforced identity encoding
        let mut inner_client_builder = reqwest::Client::builder();

        if let Some(proxy) = proxy {
            inner_client_builder = inner_client_builder.proxy(proxy);
        }

        let mut default_headers = self.headers.clone().unwrap_or_else(HeaderMap::new);
        default_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        inner_client_builder = inner_client_builder.default_headers(default_headers);

        if let Some(insecure) = insecure {
            inner_client_builder = inner_client_builder
                .danger_accept_invalid_certs(insecure)
                .danger_accept_invalid_hostnames(insecure);
        }

        inner_client_builder =
            inner_client_builder.user_agent(format!("cargo-fetcher/{}", Uuid::new_v4()));

        let inner_client = match inner_client_builder.build() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("failed to build reqwest client: {e}");
                return downloads
                    .iter()
                    .map(|d| {
                        Summary::new(d.clone(), StatusCode::BAD_REQUEST, 0)
                            .fail(anyhow::anyhow!(msg.clone()))
                    })
                    .collect();
            }
        };

        let client = ClientBuilder::new(inner_client)
            .with(TracingMiddleware::default())
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();

        // Progress
        let multi = if self.style_options.is_enabled() {
            Arc::new(MultiProgress::new())
        } else {
            Arc::new(MultiProgress::with_draw_target(ProgressDrawTarget::hidden()))
        };
        let main = Arc::new(
            multi.add(
                self.style_options
                    .main
                    .clone()
                    .to_progress_bar(downloads.len() as u64),
            ),
        );
        main.tick();

        let summaries = stream::iter(downloads)
            .map(|d| self.fetch(&client, d, multi.clone(), main.clone()))
            .buffer_unordered(self.concurrent_downloads)
            .collect::<Vec<_>>()
            .await;

        if self.style_options.main.clear {
            main.finish_and_clear();
        } else {
            main.finish();
        }

        summaries
    }

    async fn probe(
        &self,
        client: &ClientWithMiddleware,
        url: &str,
        headers: Option<HeaderMap>,
    ) -> Result<Probe> {
        // 1) Try a HEAD with identity to gather ETag/Last-Modified/Accept-Ranges/Content-Length
        let mut head = client.head(url).header(ACCEPT_ENCODING, "identity");
        if let Some(h) = headers.clone() {
            head = head.headers(h);
        }
        let head_res = head.send().await;

        let (mut size_from_head, mut etag, mut last_modified, mut accept_ranges_bytes) =
            (None, None, None, false);

        if let Ok(res) = head_res {
            if res.status().is_success() {
                if let Some(v) = res.headers().get(CONTENT_LENGTH) {
                    if let Ok(s) = v.to_str() {
                        size_from_head = s.parse::<u64>().ok();
                    }
                }
                if let Some(v) = res.headers().get(ETAG) {
                    etag = v.to_str().ok().map(|s| s.to_string());
                }
                if let Some(v) = res.headers().get(LAST_MODIFIED) {
                    last_modified = v.to_str().ok().map(|s| s.to_string());
                }
                if let Some(v) = res.headers().get(ACCEPT_RANGES) {
                    if v.as_bytes().eq_ignore_ascii_case(b"bytes") {
                        accept_ranges_bytes = true;
                    }
                }
            }
        }

        // 2) Probe with Range: bytes=0-0 (identity) to get total from Content-Range
        let mut probe = client
            .get(url)
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, "bytes=0-0");
        if let Some(h) = headers {
            probe = probe.headers(h);
        }

        let resp = probe.send().await.context("probe GET failed")?;

        let (supports_ranges, total_size) = if resp.status() == StatusCode::PARTIAL_CONTENT {
            // Parse "bytes 0-0/total"
            let total = parse_total_from_content_range(resp.headers().get(CONTENT_RANGE))
                .context("missing/invalid Content-Range on probe")?;
            (true, total)
        } else if resp.status().is_success() {
            // Server ignored Range → not range-friendly; fall back to single-stream.
            let total = size_from_head.unwrap_or(0);
            (false, total)
        } else {
            return Err(anyhow!("probe unexpected HTTP {}", resp.status()));
        };

        // Prefer strong ETag; if weak (W/...), prefer Last-Modified for If-Range.
        let if_range = match etag.as_deref() {
            Some(et) if !et.starts_with("W/") => Some(IfRange::ETag(et.to_string())),
            _ => last_modified.map(IfRange::LastModified),
        };

        Ok(Probe {
            total_size,
            supports_ranges: supports_ranges && accept_ranges_bytes,
            if_range,
        })
    }

    async fn fetch(
        &self,
        client: &ClientWithMiddleware,
        download: &Resource,
        multi: Arc<MultiProgress>,
        main: Arc<ProgressBar>,
    ) -> Summary {
        let output = self.directory.join(&download.filename);
        let tmp = output.with_extension("part");

        let mut summary = Summary::new(download.clone(), StatusCode::BAD_REQUEST, 0);

        // Ensure parent dir
        if let Some(parent) = output.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return summary.fail(e);
            }
        }

        // Probe the resource with identity encoding
        let probe = match self.probe(client, (&download.url).as_ref(), self.headers.clone()).await {
            Ok(p) => p,
            Err(e) => return summary.fail(e),
        };

        // Progress bar length is the *true* total size if known
        let pb = multi.add(
            self.style_options
                .child
                .clone()
                .to_progress_bar(probe.total_size)
                .with_position(0),
        );

        // If file already exists and is complete, skip
        if output.exists() {
            match output.metadata() {
                Ok(m) if m.len() == probe.total_size && probe.total_size > 0 => {
                    main.inc(1);
                    if self.style_options.child.clear {
                        pb.finish_and_clear();
                    } else {
                        pb.finish();
                    }
                    return summary.with_status(Status::Skipped(
                        "the file was already fully downloaded".into(),
                    ));
                }
                _ => {}
            }
        }

        // Always write to a .part file then rename atomically
        // SINGLE-STREAM PATH
        if !probe.supports_ranges || probe.total_size == 0 {
            let mut req = client
                .get(download.url.clone())
                .header(ACCEPT_ENCODING, "identity");
            if let Some(ref h) = self.headers {
                req = req.headers(h.clone());
            }

            // Open tmp with truncate
            let f = match OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)
                .await
            {
                Ok(f) => f,
                Err(e) => return summary.fail(e),
            };
            let mut file = BufWriter::new(f);
            let mut size_on_disk: u64 = 0;

            let res = match req.send().await {
                Ok(res) => res,
                Err(e) => return summary.fail(e),
            };
            if !res.status().is_success() {
                return summary.fail(anyhow!("HTTP {}", res.status()));
            }

            let mut stream = res.bytes_stream();
            loop {
                match timeout(Duration::from_secs(Self::CHUNK_TIMEOUT_SECS), stream.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        if let Err(e) = file.write_all(&chunk).await {
                            return summary.fail(e);
                        }
                        size_on_disk += chunk.len() as u64;
                        pb.inc(chunk.len() as u64);
                    }
                    Ok(Some(Err(e))) => return summary.fail(e),
                    Ok(None) => break,
                    Err(_) => return summary.fail(anyhow!("timeout while streaming body")),
                }
            }

            if let Err(e) = file.flush().await {
                return summary.fail(e);
            }

            // Rename into place
            if let Err(e) = tokio::fs::rename(&tmp, &output).await {
                return summary.fail(e);
            }

            if self.style_options.child.clear {
                pb.finish_and_clear();
            } else {
                pb.finish();
            }
            main.inc(1);

            return Summary::new(download.clone(), StatusCode::OK, size_on_disk)
                .with_status(Status::Success);
        }

        // RANGED PATH
        // Create/truncate tmp and set final length
        {
            let f = match OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)
                .await
            {
                Ok(f) => f,
                Err(e) => return summary.fail(e),
            };
            if let Err(e) = f.set_len(probe.total_size).await {
                return summary.fail(e);
            }
        }

        // Build ranges
        let indexed_ranges: Vec<(u64, u64)> = (0..probe.total_size)
            .step_by(self.chunk_size as usize)
            .map(|start| {
                let end = (start + self.chunk_size - 1).min(probe.total_size - 1);
                (start, end)
            })
            .collect();

        let permits = self
            .concurrent_chunk
            .max(1)
            .min(indexed_ranges.len().max(1));
        let semaphore = Arc::new(Semaphore::new(permits));

        let mut tasks = Vec::with_capacity(indexed_ranges.len());
        for (start, end) in indexed_ranges {
            let semaphore = semaphore.clone();
            let client = client.clone();
            let tmp = tmp.clone();
            let pb = pb.clone();
            let url = download.url.clone();
            let headers = self.headers.clone();
            let if_range = probe.if_range.clone();

            let task = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.context("semaphore closed")?;

                // Prepare request
                let mut req = client
                    .get(url)
                    .header(ACCEPT_ENCODING, "identity")
                    .header(RANGE, format!("bytes={}-{}", start, end));

                if let Some(h) = headers.clone() {
                    req = req.headers(h);
                }
                if let Some(ir) = if_range {
                    match ir {
                        IfRange::ETag(et) => {
                            req = req.header(IF_RANGE, et);
                        }
                        IfRange::LastModified(lm) => {
                            req = req.header(IF_RANGE, lm);
                        }
                    }
                }

                let resp = req.send().await.context("range request failed")?;

                if resp.status() != StatusCode::PARTIAL_CONTENT {
                    return Err(anyhow!(
                        "server did not return 206 for range {}-{} (got {})",
                        start,
                        end,
                        resp.status()
                    ));
                }

                // Content-Range sanity
                if let Some(cr) = resp.headers().get(CONTENT_RANGE) {
                    let crs = cr.to_str().unwrap_or_default();
                    if !(crs.starts_with("bytes ")
                        && crs.contains(&format!("{}-", start))
                        && crs.contains(&format!("-{}", end)))
                    {
                        return Err(anyhow!("unexpected Content-Range: {}", crs));
                    }
                }

                let mut stream = resp.bytes_stream();

                // Open tmp and write at offset
                let mut file = OpenOptions::new()
                    .write(true)
                    .open(&tmp)
                    .await
                    .context("open tmp for ranged write failed")?;

                file.seek(SeekFrom::Start(start))
                    .await
                    .context("seek failed")?;

                let expected = end - start + 1;
                let mut written: u64 = 0;

                loop {
                    match timeout(Duration::from_secs(Downloader::CHUNK_TIMEOUT_SECS), stream.next())
                        .await
                    {
                        Ok(Some(Ok(chunk))) => {
                            file.write_all(&chunk).await.context("write_all failed")?;
                            written += chunk.len() as u64;
                            pb.inc(chunk.len() as u64);
                        }
                        Ok(Some(Err(e))) => return Err(anyhow!(e)),
                        Ok(None) => break,
                        Err(_) => {
                            return Err(anyhow!("timeout while reading range {}-{}", start, end));
                        }
                    }
                }

                if written != expected {
                    return Err(anyhow!(
                        "short write for range {}-{}: wrote {}, expected {}",
                        start,
                        end,
                        written,
                        expected
                    ));
                }

                file.flush().await.context("flush failed")?;
                Ok::<(), anyhow::Error>(())
            });

            tasks.push(task);
        }

        let results = try_join_all(tasks).await;
        if let Err(join_err) = results {
            if self.style_options.child.clear {
                pb.finish_and_clear();
            } else {
                pb.finish();
            }
            return summary.fail(anyhow!("join error: {}", join_err));
        }
        for res in results.unwrap() {
            if let Err(e) = res {
                if self.style_options.child.clear {
                    pb.finish_and_clear();
                } else {
                    pb.finish();
                }
                return summary.fail(e);
            }
        }

        // All chunks ok → atomically move into place
        if let Err(e) = tokio::fs::rename(&tmp, &output).await {
            return summary.fail(e);
        }

        if self.style_options.child.clear {
            pb.finish_and_clear();
        } else {
            pb.finish();
        }
        main.inc(1);

        Summary::new(download.clone(), StatusCode::OK, probe.total_size).with_status(Status::Success)
    }
}

fn parse_total_from_content_range(v: Option<&HeaderValue>) -> Option<u64> {
    // "bytes START-END/TOTAL"
    let s = v?.to_str().ok()?;
    let slash = s.rsplit('/').next()?;
    slash.parse::<u64>().ok()
}

#[derive(Clone, Debug)]
struct Probe {
    total_size: u64,
    supports_ranges: bool,
    if_range: Option<IfRange>,
}

#[derive(Clone, Debug)]
enum IfRange {
    ETag(String),
    LastModified(String),
}

pub struct DownloaderBuilder(Downloader);

impl DownloaderBuilder {
    pub fn new() -> Self {
        DownloaderBuilder::default()
    }

    pub fn hidden() -> Self {
        let d = DownloaderBuilder::default();
        d.style_options(StyleOptions::new(
            ProgressBarOpts::hidden(),
            ProgressBarOpts::hidden(),
        ))
    }

    pub fn directory(mut self, directory: PathBuf) -> Self {
        self.0.directory = directory;
        self
    }

    pub fn retries(mut self, retries: u32) -> Self {
        self.0.retries = retries;
        self
    }

    pub fn concurrent_downloads(mut self, concurrent_downloads: usize) -> Self {
        self.0.concurrent_downloads = concurrent_downloads;
        self
    }

    pub fn concurrent_chunks(mut self, concurrent_chunks: usize) -> Self {
        self.0.concurrent_chunk = concurrent_chunks;
        self
    }

    pub fn chunk_size(mut self, chunk_size: u64) -> Self {
        self.0.chunk_size = chunk_size;
        self
    }

    pub fn style_options(mut self, style_options: StyleOptions) -> Self {
        self.0.style_options = style_options;
        self
    }

    fn new_header(&self) -> HeaderMap {
        match self.0.headers {
            Some(ref h) => h.to_owned(),
            _ => HeaderMap::new(),
        }
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        let mut new = self.new_header();
        new.extend(headers);
        self.0.headers = Some(new);
        self
    }

    pub fn header<K: IntoHeaderName>(mut self, name: K, value: HeaderValue) -> Self {
        let mut new = self.new_header();
        new.insert(name, value);
        self.0.headers = Some(new);
        self
    }

    pub fn build(self) -> Downloader {
        Downloader {
            directory: self.0.directory,
            retries: self.0.retries,
            concurrent_downloads: self.0.concurrent_downloads,
            concurrent_chunk: self.0.concurrent_chunk,
            chunk_size: self.0.chunk_size,
            style_options: self.0.style_options,
            headers: self.0.headers,
        }
    }
}

impl Default for DownloaderBuilder {
    fn default() -> Self {
        Self(Downloader {
            directory: std::env::current_dir().unwrap_or_default(),
            retries: Downloader::DEFAULT_RETRIES,
            concurrent_downloads: Downloader::DEFAULT_CONCURRENT_DOWNLOADS,
            concurrent_chunk: Downloader::DEFAULT_CONCURRENT_CHUNK,
            chunk_size: Downloader::DEFAULT_CHUNK_SIZE,
            style_options: StyleOptions::default(),
            headers: None,
        })
    }
}
