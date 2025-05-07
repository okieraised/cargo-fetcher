use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH};
use reqwest::{StatusCode, Url};
use reqwest_middleware::ClientWithMiddleware;

#[derive(Debug, Clone)]
pub struct Resource {
    pub url: Url,
    pub filename: String,
}

impl Resource {
    pub fn new(url: &Url, filename: &str) -> Resource {
        Self {
            url: url.clone(),
            filename: String::from(filename),
        }
    }

    pub async fn content_length(
        &self,
        client: &ClientWithMiddleware,
    ) -> Result<Option<u64>, reqwest_middleware::Error> {
        let res = client.head(self.url.clone()).send().await?;
        let headers = res.headers();
        match headers.get(CONTENT_LENGTH) {
            None => Ok(None),
            Some(header_value) => match header_value.to_str() {
                Ok(v) => match v.to_string().parse::<u64>() {
                    Ok(v) => Ok(Some(v)),
                    Err(_) => Ok(None),
                },
                Err(_) => Ok(None),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Fail(String),
    NotStarted,
    Skipped(String),
    Success,
}

#[derive(Debug, Clone)]
pub struct Summary {
    download: Resource,
    status_code: StatusCode,
    size: u64,
    status: Status,
}

impl Summary {
    pub fn new(download: Resource, status_code: StatusCode, size: u64) -> Self {
        Self {
            download,
            status_code,
            size,
            status: Status::NotStarted,
        }
    }

    pub fn with_status(self, status: Status) -> Self {
        Self { status, ..self }
    }

    pub fn status_code(&self) -> StatusCode {
        self.status_code
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn download(&self) -> &Resource {
        &self.download
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn fail(self, msg: impl std::fmt::Display) -> Self {
        Self {
            status: Status::Fail(format!("{}", msg)),
            ..self
        }
    }
}
