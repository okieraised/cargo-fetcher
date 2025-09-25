use crate::downloader::resource::{Resource, Status, Summary};
use anyhow::{anyhow, Context};
use futures::{future::try_join_all, stream, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::{
    header::{HeaderMap, HeaderValue, IntoHeaderName, ACCEPT_ENCODING, CONTENT_RANGE, RANGE},
    StatusCode,
};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use reqwest_tracing::TracingMiddleware;
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
};
use tokio::{
    fs::OpenOptions,
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::Semaphore,
    time::{timeout, Duration},
};
use std::io::SeekFrom;
use uuid::Uuid;

fn get_random_user_agent() -> &'static str {
    // You can add more UAs here if you want rotation
    const UA: &[&str] = &[
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 13_4) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.5 Safari/605.1.15",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
    ];
    // Keep it deterministic for now to avoid bringing in RNG here.
    UA[Uuid::new_v4().as_u128() as usize % UA.len()]
}

#[derive(Debug, Clone)]
pub struct Downloader {
    /// Directory where to store the downloaded files.
    directory: PathBuf,
    /// Number of retries per downloaded file.
    retries: u32,
    /// Number of maximum concurrent downloads.
    concurrent_downloads: usize,
    /// Number of chunked downloads per file.
    concurrent_chunk: usize,
    /// Size of each chunk.
    chunk_size: u64,
    /// Downloader style options.
    style_options: StyleOptions,
    /// Custom HTTP headers.
    headers: Option<HeaderMap>,
}

#[derive(Debug, Clone)]
pub struct StyleOptions {
    /// Style options for the main progress bar.
    main: ProgressBarOpts,
    /// Style options for the child progress bar(s).
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
    /// Create new [`Downloader`] [`StyleOptions`].
    pub fn new(main: ProgressBarOpts, child: ProgressBarOpts) -> Self {
        Self { main, child }
    }

    /// Set the options for the main progress bar.
    pub fn set_main(&mut self, main: ProgressBarOpts) {
        self.main = main;
    }

    /// Set the options for the child progress bar.
    pub fn set_child(&mut self, child: ProgressBarOpts) {
        self.child = child;
    }

    /// Return `false` if neither the main nor the child bar is enabled.
    pub fn is_enabled(&self) -> bool {
        self.main.enabled || self.child.enabled
    }
}

/// Define the options for a progress bar.
#[derive(Debug, Clone)]
pub struct ProgressBarOpts {
    /// Progress bar template string.
    template: Option<String>,
    /// Progression characters set.
    ///
    /// There must be at least 3 characters for the following states:
    /// "filled", "current", and "to do".
    progress_chars: Option<String>,
    /// Enable or disable the progress bar.
    enabled: bool,
    /// Clear the progress bar once completed.
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

        // Build inner reqwest client
        let mut inner_client_builder = reqwest::Client::builder();

        if let Some(proxy) = proxy {
            inner_client_builder = inner_client_builder.proxy(proxy);
        }

        // Merge default headers with enforced Accept-Encoding: identity
        let mut default_headers = self.headers.clone().unwrap_or_else(HeaderMap::new);
        default_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        inner_client_builder = inner_client_builder.default_headers(default_headers);

        if let Some(insecure) = insecure {
            inner_client_builder = inner_client_builder
                .danger_accept_invalid_certs(insecure)
                .danger_accept_invalid_hostnames(insecure);
        }

        let id = Uuid::new_v4().to_string();
        inner_client_builder = inner_client_builder.user_agent(
            format!("{}; {}", get_random_user_agent(), id + "-cargo-fetcher")
        );

        let inner_client = match inner_client_builder.build() {
            Ok(c) => c,
            Err(e) => {
                let err_msg = format!("failed to build reqwest client: {e}");
                return downloads
                    .iter()
                    .map(|d| {
                        Summary::new(d.clone(), StatusCode::BAD_REQUEST, 0)
                            .fail(anyhow::anyhow!(err_msg.clone()))
                    })
                    .collect();
            }
        };

        let client = ClientBuilder::new(inner_client)
            .with(TracingMiddleware::default())
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();

        // Progress set up
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

        // Fan out downloads
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

    async fn fetch(
        &self,
        client: &ClientWithMiddleware,
        download: &Resource,
        multi: Arc<MultiProgress>,
        main: Arc<ProgressBar>,
    ) -> Summary {
        let output = self.directory.join(&download.filename);
        let mut summary = Summary::new(download.clone(), StatusCode::BAD_REQUEST, 0);

        // Determine content length (server-reported, raw)
        let content_length = match download.content_length(client).await {
            Ok(Some(l)) => l,
            Ok(None) => 0,
            Err(e) => return summary.fail(e),
        };

        // If server didn't provide length, do a single-stream download.
        if content_length == 0 {
            let pb = multi.add(
                self.style_options
                    .child
                    .clone()
                    .to_progress_bar(0)
                    .with_position(0),
            );

            if let Some(parent) = output.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return summary.fail(e);
                }
            }

            // No append; write sequentially
            let mut file = match OpenOptions::new()
                .create(true)
                .write(true)
                .open(&output)
                .await
            {
                Ok(f) => f,
                Err(e) => return summary.fail(e),
            };

            let mut req = client.get(download.url.clone());
            if let Some(ref h) = self.headers {
                req = req.headers(h.clone());
            }
            // Enforce identity encoding
            req = req.header(ACCEPT_ENCODING, "identity");

            let res = match req.send().await {
                Ok(res) => res,
                Err(e) => return summary.fail(e),
            };

            if !res.status().is_success() {
                return summary.fail(anyhow!("HTTP {}", res.status()));
            }

            let mut stream = res.bytes_stream();
            let mut size_on_disk: u64 = 0;

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

            if self.style_options.child.clear {
                pb.finish_and_clear();
            } else {
                pb.finish();
            }

            main.inc(1);
            return Summary::new(download.clone(), StatusCode::OK, size_on_disk)
                .with_status(Status::Success);
        }

        // Known content length (ranged download path)
        // Early skip if the full file is already present.
        if output.exists() {
            let size_on_disk = match output.metadata() {
                Ok(m) => m.len(),
                Err(e) => return summary.fail(e),
            };
            if content_length == size_on_disk {
                main.inc(1);
                return summary.with_status(Status::Skipped(
                    "the file was already fully downloaded".into(),
                ));
            }
        }

        if let Some(parent) = output.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return summary.fail(e);
            }
        }

        // Pre-create and pre-size the file to enable correct ranged writes.
        {
            let f = match OpenOptions::new()
                .create(true)
                .write(true)
                .open(&output)
                .await
            {
                Ok(f) => f,
                Err(e) => return summary.fail(e),
            };
            if let Err(e) = f.set_len(content_length).await {
                return summary.fail(e);
            }
        }

        let pb = multi.add(
            self.style_options
                .child
                .clone()
                .to_progress_bar(content_length)
                .with_position(0),
        );

        // Build ranges
        let indexed_ranges: Vec<(usize, u64, u64)> = (0..content_length)
            .step_by(self.chunk_size as usize)
            .enumerate()
            .map(|(index, start)| {
                let end = (start + self.chunk_size - 1).min(content_length - 1);
                (index, start, end)
            })
            .collect();

        let permits = self.concurrent_chunk.max(1).min(indexed_ranges.len());
        let semaphore = Arc::new(Semaphore::new(permits));

        // Spawn chunk tasks
        let mut tasks = Vec::with_capacity(indexed_ranges.len());
        for (_, start, end) in indexed_ranges {
            let semaphore = semaphore.clone();
            let client = client.clone();
            let output = output.clone();
            let pb = pb.clone();
            let url = download.url.clone();
            let headers = self.headers.clone();

            let task = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.context("semaphore closed")?;

                // Construct request for this range
                let range_header = format!("bytes={}-{}", start, end);
                let mut req = client.get(url).header(RANGE, range_header);
                if let Some(h) = headers {
                    req = req.headers(h);
                }
                // Avoid auto-decompression to keep byte offsets exact.
                req = req.header(ACCEPT_ENCODING, "identity");

                let resp = req.send().await.context("range request failed")?;

                if resp.status() != StatusCode::PARTIAL_CONTENT {
                    return Err(anyhow!(
                        "server did not return 206 for range {}-{} (got {})",
                        start,
                        end,
                        resp.status()
                    ));
                }

                // Optional: sanity-check Content-Range
                if let Some(cr) = resp.headers().get(CONTENT_RANGE) {
                    let cr = cr.to_str().unwrap_or_default();
                    // Expect "bytes start-end/total"
                    if !(cr.starts_with("bytes ")
                        && cr.contains(&format!("{}-", start))
                        && cr.contains(&format!("-{}", end)))
                    {
                        return Err(anyhow!("unexpected Content-Range: {}", cr));
                    }
                }

                let mut stream = resp.bytes_stream();

                // Open a new handle without append, seek to start, and write bytes
                let mut file = OpenOptions::new()
                    .write(true)
                    .open(&output)
                    .await
                    .context("open for ranged write failed")?;

                file.seek(SeekFrom::Start(start))
                    .await
                    .context("seek failed")?;

                let mut written: u64 = 0;
                let expected = end - start + 1;

                loop {
                    match timeout(Duration::from_secs(Downloader::CHUNK_TIMEOUT_SECS), stream.next())
                        .await
                    {
                        Ok(Some(Ok(chunk))) => {
                            file.write_all(&chunk)
                                .await
                                .context("write_all failed")?;
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

        // Await all chunks; fail if any failed.
        let results = try_join_all(tasks).await;
        if let Err(join_err) = results {
            // Join error (task panic/cancel)
            if self.style_options.child.clear {
                pb.finish_and_clear();
            } else {
                pb.finish();
            }
            return summary.fail(anyhow!("join error: {}", join_err));
        }

        // Unwrap inner results
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

        if self.style_options.child.clear {
            pb.finish_and_clear();
        } else {
            pb.finish();
        }

        main.inc(1);
        Summary::new(download.clone(), StatusCode::OK, content_length).with_status(Status::Success)
    }
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
