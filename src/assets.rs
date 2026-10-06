//! Access to the template images, fonts, emoji and static files.
//!
//! Paths are relative to the asset root, e.g. `templates/fry/default.jpg`.
//! Rendering reads synchronously through [`Source`]; servers load the files a
//! render needs up front through [`Assets`], which can be asynchronous (on
//! Cloudflare Workers the files come from the static assets binding).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Synchronous file access used while rendering.
pub trait Source: Sync {
    fn read(&self, path: &str) -> Option<Bytes>;
}

/// Files loaded ahead of time, keyed by path.
pub type Files = HashMap<String, Bytes>;

impl Source for Files {
    fn read(&self, path: &str) -> Option<Bytes> {
        self.get(path).cloned()
    }
}

/// Where a server loads assets from.
pub trait Assets: Send + Sync {
    /// Load the given files, leaving out any that don't exist.
    fn load(&self, paths: Vec<String>) -> BoxFuture<'_, Files>;

    /// Synchronous access, if available; renders then read files on demand
    /// instead of loading them up front.
    fn source(&self) -> Option<&dyn Source> {
        None
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use directory::Directory;

#[cfg(not(target_arch = "wasm32"))]
mod directory {
    use std::path::PathBuf;

    use super::*;

    /// Assets in a local directory (the repository root by default).
    #[derive(Debug, Clone)]
    pub struct Directory {
        root: PathBuf,
    }

    impl Directory {
        pub fn new(root: impl Into<PathBuf>) -> Self {
            Self { root: root.into() }
        }

        pub fn root(&self) -> &std::path::Path {
            &self.root
        }

        pub fn read_all(&self, paths: &[String]) -> Files {
            paths
                .iter()
                .filter_map(|path| Some((path.clone(), self.read(path)?)))
                .collect()
        }
    }

    impl Source for Directory {
        fn read(&self, path: &str) -> Option<Bytes> {
            std::fs::read(self.root.join(path)).ok().map(Bytes::from)
        }
    }

    impl Assets for Directory {
        fn load(&self, paths: Vec<String>) -> BoxFuture<'_, Files> {
            let directory = self.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || directory.read_all(&paths))
                    .await
                    .unwrap_or_default()
            })
        }

        fn source(&self) -> Option<&dyn Source> {
            Some(self)
        }
    }
}
