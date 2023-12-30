use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::result::Result;
use std::time::Duration;
use std::{error, fmt};

use flate2::read::GzDecoder;
use tar::Archive;
use ureq;

static USER_AGENT: &str = concat!("typstd/{}", env!("CARGO_PKG_VERSION"));

static NAMESPACE: &str = "preview";

#[derive(Debug)]
pub enum Error {
    RequestError(String),
    ExtractError(String),
}

impl error::Error for Error {}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::RequestError(err) => {
                write!(f, "failed to make HTTP request: {err}")
            }
            Self::ExtractError(err) => {
                write!(f, "failed to extract archive: {err}")
            }
        }
    }
}

/// Fetch package tarball from remote and untar it locally.
fn fetch(url: &str, r#where: &Path) -> Result<(), Error> {
    let mut builder = ureq::AgentBuilder::new()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(5));

    // Get the network proxy config from the environment.
    if let Some(proxy) = env_proxy::for_url_str(url)
        .to_url()
        .and_then(|url| ureq::Proxy::new(url).ok())
    {
        builder = builder.proxy(proxy);
    }

    let agent = builder.build();
    let reader = agent
        .get(url)
        .call()
        .map_err(|err| Error::RequestError(err.to_string()))?
        .into_reader();

    let inflated = GzDecoder::new(reader);
    Archive::new(inflated).unpack(r#where).map_err(|err| {
        fs::remove_dir_all(r#where).ok();
        Error::ExtractError(err.to_string())
    })
}

pub fn prepare_package(name: &str, version: &str) -> Result<PathBuf, Error> {
    // Search cache directory (or locally) for package. If there is a
    // directory at the path then return it.
    let cache_dir = match dirs::cache_dir() {
        Some(cache_dir) => cache_dir,
        None => PathBuf::new(),
    };
    let r#where = format!("typstd/packages/{NAMESPACE}/{name}/{version}");
    let r#where = cache_dir.join(r#where);
    if r#where.exists() {
        log::info!("package {}:{} found at {:?}", name, version, r#where);
        return Ok(r#where);
    }

    let url = format!(
        "https://packages.typst.org/{NAMESPACE}/{name}-{version}.tar.gz",
    );
    log::info!("download package {}:{} to {:?}", name, version, r#where);
    fetch(&url, &r#where).map(|()| r#where)
}
