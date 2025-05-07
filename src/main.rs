mod downloader;

use crate::downloader::downloader::DownloaderBuilder;
use downloader::resource::Resource;
use clap::Parser;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    help_template = "{about-section}\nAuthor: {author-with-newline}\nVersion: {version} \n\n{usage-heading} {usage} \n\n{all-args} {tab}"
)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Url to download
    #[arg(short = 'u', long)]
    url: String,

    /// Skip certificate verification
    #[clap(short = 'i', long)]
    insecure: Option<bool>,

    /// Number of retries for failed download
    #[clap(short, long)]
    retries: Option<u32>,

    /// Number of concurrent downloads
    #[clap(short = 'd', long)]
    concurrent_downloads: Option<usize>,

    /// Number of concurrent chunks per download
    #[clap(short = 'c', long)]
    concurrent_chunk: Option<usize>,

    /// Chunk size for each chunked download (in bytes)
    #[clap(short = 's', long)]
    chunk_size: Option<u64>,

    /// Output directory
    #[clap(short = 'o', long)]
    directory: Option<PathBuf>,

    /// Output file name
    #[clap(short = 'f', long)]
    file_name: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let mut builder = DownloaderBuilder::new();

    if let Some(retries) = args.retries {
        builder = builder.retries(retries);
    }

    if let Some(concurrent_downloads) = args.concurrent_downloads {
        builder = builder.concurrent_downloads(concurrent_downloads);
    }

    if let Some(concurrent_chunks) = args.concurrent_chunk {
        builder = builder.concurrent_chunks(concurrent_chunks);
    }

    if let Some(chunk_size) = args.chunk_size {
        builder = builder.chunk_size(chunk_size);
    }

    if let Some(directory) = args.directory {
        builder = builder.directory(directory);
    }

    let downloader = builder.build();

    let input_url = &args.url;
    let downloadable_links = match get_downloadable_urls(input_url).await {
        Ok(l) => l,
        Err(e) => {
            println!("error retrieving downloadable link: {:?}", e);
            return;
        }
    };

    println!(
        "prepare to download {:?} resources",
        downloadable_links.len()
    );
    let mut resources = Vec::new();
    for link in downloadable_links {
        let resource = Resource::new(
            &Url::parse(input_url.as_str()).unwrap(),
            get_filename_from_url(&link).unwrap().as_str(),
        );
        resources.push(resource);
    }
    let summaries = downloader.download(&resources, args.insecure).await;
    for summary in summaries {
        println!(
            "downloaded_size: {:?} bytes | status: {:?} | url: {:?} | file_name: {:?}",
            summary.size(),
            summary.status(),
            summary.download().url.as_str(),
            summary.download().filename
        )
    }
}

fn get_filename_from_url(url: &str) -> Option<String> {
    let path = Url::parse(url).ok()?.path().to_string();
    Path::new(&path)
        .file_name()
        .map(|os_str| os_str.to_string_lossy().into_owned())
}

async fn get_downloadable_urls(url: &str) -> Result<Vec<String>, anyhow::Error> {
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    let res = match client.head(url).send().await {
        Ok(res) => res,
        Err(e) => {
            return Err(anyhow::Error::from(e));
        }
    };

    let content_type = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_type.contains("html") {
        return Ok(vec![url.to_string()]);
    }

    let res = match client.get(url).send().await {
        Ok(res) => res,
        Err(e) => {
            return Err(anyhow::Error::from(e));
        }
    };

    let body = match res.text().await {
        Ok(body) => body,
        Err(e) => {
            return Err(anyhow::Error::from(e));
        }
    };

    let doc = Html::parse_document(&body);
    let selector = Selector::parse("a").unwrap();

    let base = match Url::parse(url) {
        Ok(base) => base,
        Err(e) => {
            return Err(anyhow::Error::from(e));
        }
    };

    let mut downloadable_links = Vec::new();
    for element in doc.select(&selector) {
        if let Some(href) = element.value().attr("href") {
            if href.starts_with('#') || href.starts_with("..") || href.contains("javascript:") {
                continue;
            }

            if let Ok(full_url) = base.join(href) {
                if full_url.path().ends_with('/')
                    || full_url.as_str().ends_with(".html")
                    || href.contains("?C=")
                {
                    continue;
                }
                downloadable_links.push(full_url.to_string());
            }
        }
    }

    Ok(downloadable_links)
}
