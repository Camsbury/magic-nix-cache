use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

use crate::error::{Error, Result};
use crate::telemetry;
use async_compression::tokio::bufread::ZstdEncoder;
use attic::nix_store::{NixStore, StorePath, ValidPathInfo};
use attic_server::narinfo::{Compression, NarInfo};
use futures::stream::TryStreamExt;
use gha_cache::{api as gha_api, Api, Credentials};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    RwLock,
};
use tokio_util::compat::FuturesAsyncReadCompatExt;

pub struct GhaCache {
    /// The GitHub Actions Cache API.
    pub api: Arc<Api>,

    /// Set of store path hashes that are not present in GHAC.
    ///
    /// We keep a local negative cache to reduce round-trips.
    narinfo_negative_cache: Arc<RwLock<HashSet<String>>>,

    /// The future from the completion of the worker.
    worker_result: RwLock<Option<tokio::task::JoinHandle<Result<()>>>>,

    channel_tx: UnboundedSender<Request>,
}

#[derive(Debug)]
enum Request {
    Shutdown,
    Upload(StorePath),
}

impl GhaCache {
    pub fn new(
        credentials: Credentials,
        cache_version: Option<String>,
        store: Arc<NixStore>,
        metrics: Arc<telemetry::TelemetryReport>,
        narinfo_negative_cache: Arc<RwLock<HashSet<String>>>,
    ) -> Result<GhaCache> {
        let cb_metrics = metrics.clone();
        let mut api = Api::new(
            credentials,
            Arc::new(Box::new(move || {
                cb_metrics
                    .tripped_429
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            })),
        )?;

        if let Some(cache_version) = &cache_version {
            api.mutate_version(cache_version.as_bytes());
        }

        let (channel_tx, channel_rx) = unbounded_channel();

        let api = Arc::new(api);

        let api2 = api.clone();

        let narinfo_negative_cache2 = narinfo_negative_cache.clone();

        let worker_result = tokio::task::spawn(async move {
            worker(
                &api2,
                store,
                channel_rx,
                metrics,
                narinfo_negative_cache2,
            )
            .await
        });

        Ok(GhaCache {
            api,
            narinfo_negative_cache,
            worker_result: RwLock::new(Some(worker_result)),
            channel_tx,
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        if let Some(worker_result) = self.worker_result.write().await.take() {
            self.channel_tx
                .send(Request::Shutdown)
                .expect("Cannot send shutdown message");
            worker_result
                .await
                .expect("failed to read result from gha worker")
        } else {
            Ok(())
        }
    }

    pub async fn enqueue_paths(
        &self,
        store: Arc<NixStore>,
        store_paths: Vec<StorePath>,
    ) -> Result<()> {
        #[derive(Default)]
        struct TraversalStats {
            roots: usize,
            visited: usize,
            enqueued_for_upload: usize,
            references_queued: usize,

            known_present_hits: usize,
            known_missing_hits: usize,
            negative_cache_hints: usize,

            remote_checks: usize,
            remote_present: usize,
            remote_missing: usize,
            remote_check_errors: usize,
        }

        let mut stats = TraversalStats {
            roots: store_paths.len(),
            ..Default::default()
        };

        tracing::info!(
            roots = stats.roots,
            "GHA cache traversal starting (remote-aware closure)"
        );

        // If the API has already tripped its circuit breaker (429/resource exhaustion),
        // avoid doing any additional work. The worker will skip uploads as well.
        if self.api.circuit_breaker_tripped() {
            tracing::debug!("GHA cache circuit breaker already tripped; skipping traversal");
            return Ok(());
        }

        // Instead of eagerly computing a full filesystem closure (which often expands
        // to huge portions of /nix/store), walk the graph and stop descending once we
        // hit a store path that already exists in the GitHub Actions Cache.
        //
        // This keeps the upload set to “what's missing remotely” rather than “the
        // entire closure of everything built locally.”
        let mut queue: VecDeque<StorePath> = VecDeque::from(store_paths);
        let mut visited: HashSet<StorePath> = HashSet::new();

        // Cache existence checks to avoid repeated API round-trips.
        let mut known_present: HashSet<String> = HashSet::new();
        let mut known_missing: HashSet<String> = HashSet::new();

        while let Some(path) = queue.pop_front() {
            if !visited.insert(path.clone()) {
                continue;
            }

            stats.visited += 1;
            if stats.visited % 250 == 0 {
                tracing::debug!(
                    visited = stats.visited,
                    queue_len = queue.len(),
                    enqueued_for_upload = stats.enqueued_for_upload,
                    "GHA cache traversal progress"
                );
            }

            if self.api.circuit_breaker_tripped() {
                tracing::debug!(
                    visited = stats.visited,
                    "GHA cache circuit breaker tripped during traversal; stopping"
                );
                return Ok(());
            }

            let store_path_hash = path.to_hash().to_string();

            // Fast-path: if the binary cache has already proven this is missing on this run.
            // (We treat the negative cache as a *hint* only; it may be empty.)
            let exists = if known_present.contains(&store_path_hash) {
                stats.known_present_hits += 1;
                true
            } else if known_missing.contains(&store_path_hash) {
                stats.known_missing_hits += 1;
                false
            } else if self
                .narinfo_negative_cache
                .read()
                .await
                .contains(&store_path_hash)
            {
                stats.negative_cache_hints += 1;
                false
            } else {
                stats.remote_checks += 1;
                let key = format!("{store_path_hash}.narinfo");
                match self.api.get_file_url(&[key.as_str()]).await {
                    Ok(Some(_)) => {
                        stats.remote_present += 1;
                        true
                    }
                    Ok(None) => {
                        stats.remote_missing += 1;
                        false
                    }
                    Err(e) => {
                        stats.remote_check_errors += 1;

                        // If we got rate-limited, the circuit breaker will be tripped.
                        // Bail out quietly to avoid turning this into a failure.
                        if self.api.circuit_breaker_tripped()
                            || matches!(
                                e,
                                gha_api::Error::CircuitBreakerTripped
                                    | gha_api::Error::ApiError {
                                        status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                                        ..
                                    }
                            )
                        {
                            tracing::trace!(%e, "Disabling GitHub Actions Cache due to rate limiting");
                            return Ok(());
                        }

                        // For other errors, assume the path is missing so we can still attempt
                        // an upload (preserving prior behavior where individual uploads may fail
                        // without failing the entire run).
                        tracing::warn!(
                            %e,
                            store_path_hash,
                            store_path = %store.get_full_path(&path).display(),
                            "Failed to check for existing narinfo in GHA cache; assuming missing"
                        );
                        false
                    }
                }
            };

            if exists {
                tracing::debug!(
                    store_path_hash,
                    store_path = %store.get_full_path(&path).display(),
                    "GHA cache traversal: already present, not uploading"
                );
                known_present.insert(store_path_hash);
                continue;
            }

            tracing::debug!(
                store_path_hash,
                store_path = %store.get_full_path(&path).display(),
                "GHA cache traversal: missing, scheduling upload and descending references"
            );

            known_missing.insert(store_path_hash);

            self.channel_tx
                .send(Request::Upload(path.clone()))
                .map_err(|_| Error::Internal("Cannot send upload message".to_owned()))?;
            stats.enqueued_for_upload += 1;

            // Descend into references *only* for paths that are missing remotely.
            // If a path already exists in the cache, we assume its entire closure has
            // been uploaded previously.
            let path_info = store.query_path_info(path).await?;
            let reference_count = path_info.references.len();
            stats.references_queued += reference_count;

            tracing::debug!(
                references = reference_count,
                "GHA cache traversal: queueing references"
            );

            for reference in path_info.references {
                let reference = store.follow_store_path(reference)?;
                queue.push_back(reference);
            }
        }

        tracing::info!(
            roots = stats.roots,
            visited = stats.visited,
            enqueued_for_upload = stats.enqueued_for_upload,
            references_queued = stats.references_queued,
            known_present_hits = stats.known_present_hits,
            known_missing_hits = stats.known_missing_hits,
            negative_cache_hints = stats.negative_cache_hints,
            remote_checks = stats.remote_checks,
            remote_present = stats.remote_present,
            remote_missing = stats.remote_missing,
            remote_check_errors = stats.remote_check_errors,
            "GHA cache traversal finished"
        );

        Ok(())
    }
}

async fn worker(
    api: &Api,
    store: Arc<NixStore>,
    mut channel_rx: UnboundedReceiver<Request>,
    metrics: Arc<telemetry::TelemetryReport>,
    narinfo_negative_cache: Arc<RwLock<HashSet<String>>>,
) -> Result<()> {
    let mut done = HashSet::new();

    while let Some(req) = channel_rx.recv().await {
        match req {
            Request::Shutdown => {
                break;
            }
            Request::Upload(path) => {
                if api.circuit_breaker_tripped() {
                    tracing::trace!("GitHub Actions gave us a 429, so we're done.",);
                    continue;
                }

                if !done.insert(path.clone()) {
                    continue;
                }

                if let Err(err) = upload_path(
                    api,
                    store.clone(),
                    &path,
                    metrics.clone(),
                    narinfo_negative_cache.clone(),
                )
                .await
                {
                    tracing::error!(
                        "Upload of path '{}' failed: {}",
                        store.get_full_path(&path).display(),
                        err
                    );
                }
            }
        }
    }

    Ok(())
}

async fn upload_path(
    api: &Api,
    store: Arc<NixStore>,
    path: &StorePath,
    metrics: Arc<telemetry::TelemetryReport>,
    narinfo_negative_cache: Arc<RwLock<HashSet<String>>>,
) -> Result<()> {
    let path_info = store.query_path_info(path.clone()).await?;

    // If narinfo already exists, treat this path as already cached and do nothing.
    // This keeps repeated runs from continually uploading the whole closure.
    let narinfo_path = format!("{}.narinfo", path.to_hash().as_str());
    match api.get_file_url(&[narinfo_path.as_str()]).await {
        Ok(Some(_)) => {
            // Ensure we don't keep treating this path as missing during this run.
            narinfo_negative_cache
                .write()
                .await
                .remove(&path.to_hash().to_string());

            tracing::debug!(
                "Skipping upload of '{}' because narinfo already exists in the GHA cache",
                store.get_full_path(path).display()
            );
            return Ok(());
        }
        Ok(None) => {
            // proceed
        }
        Err(e) => {
            // If we got rate-limited, the circuit breaker will be tripped; bail out quietly.
            if api.circuit_breaker_tripped()
                || matches!(
                    e,
                    gha_api::Error::CircuitBreakerTripped
                        | gha_api::Error::ApiError {
                            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                            ..
                        }
                )
            {
                tracing::trace!(%e, "Disabling GitHub Actions Cache due to rate limiting");
                return Ok(());
            }

            tracing::warn!(
                %e,
                store_path = %store.get_full_path(path).display(),
                "Failed to check whether narinfo exists; proceeding with upload"
            );
        }
    }

    // Upload the NAR (unless it already exists).
    let nar_path = format!("{}.nar.zstd", path_info.nar_hash.to_base32());

    let nar_exists = match api.get_file_url(&[nar_path.as_str()]).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            if api.circuit_breaker_tripped() {
                return Ok(());
            }
            tracing::warn!(
                %e,
                nar_path,
                "Failed to check whether NAR exists; proceeding with upload"
            );
            false
        }
    };

    if !nar_exists {
        let nar_allocation = api.allocate_file_with_random_suffix(&nar_path).await?;

        let nar_stream = store.nar_from_path(path.clone());

        let nar_reader = nar_stream.map_err(std::io::Error::other).into_async_read();

        let nar_compressor = ZstdEncoder::new(nar_reader.compat());

        let compressed_nar_size = api.upload_file(nar_allocation, nar_compressor).await?;
        metrics.nars_uploaded.incr();

        tracing::debug!(
            "Uploaded '{}' (size {} -> {})",
            nar_path,
            path_info.nar_size,
            compressed_nar_size
        );
    } else {
        tracing::debug!("Skipping upload of existing NAR '{nar_path}'");
    }

    // Upload the narinfo.
    let narinfo_allocation = api.allocate_file_with_random_suffix(&narinfo_path).await?;

    let narinfo = path_info_to_nar_info(store.clone(), &path_info, format!("nar/{nar_path}"))
        .to_string()
        .expect("failed to convert path into to nar info");

    tracing::debug!("Uploading '{}'", narinfo_path);

    api.upload_file(narinfo_allocation, narinfo.as_bytes())
        .await?;

    metrics.narinfos_uploaded.incr();

    narinfo_negative_cache
        .write()
        .await
        .remove(&path.to_hash().to_string());

    tracing::info!(
        "Uploaded '{}' to the GitHub Action Cache",
        store.get_full_path(path).display()
    );

    Ok(())
}

// FIXME: move to attic.
fn path_info_to_nar_info(store: Arc<NixStore>, path_info: &ValidPathInfo, url: String) -> NarInfo {
    NarInfo {
        store_path: store.get_full_path(&path_info.path),
        url,
        compression: Compression::Zstd,
        file_hash: None,
        file_size: None,
        nar_hash: path_info.nar_hash.clone(),
        nar_size: path_info.nar_size as usize,
        references: path_info
            .references
            .iter()
            .map(|r| {
                r.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_else(|| {
                        panic!(
                            "failed to convert nar_info reference to string: {}",
                            r.display()
                        )
                    })
                    .to_owned()
            })
            .collect(),
        system: None,
        deriver: None,
        signature: None,
        ca: path_info.ca.clone(),
    }
}
