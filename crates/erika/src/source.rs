#[cfg(target_os = "android")]
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "android")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
#[cfg(target_os = "android")]
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::core::MediaSourceHint;
use crate::trace;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("io error: {0}")]
    Io(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("unsupported source URI: {0}")]
    Unsupported(String),
    #[error("invalid owned file descriptor URI: {0}")]
    InvalidFileDescriptorUri(String),
}

pub type Result<T> = std::result::Result<T, SourceError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub length: Option<u64>,
}

impl ByteRange {
    pub fn suffix_from(start: u64) -> Self {
        Self {
            start,
            length: None,
        }
    }
}

pub trait MediaSource: Send {
    fn uri(&self) -> &str;
    fn len(&mut self) -> Result<Option<u64>>;
    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>>;
}

#[derive(Debug)]
pub struct LocalFileSource {
    uri: String,
    path: PathBuf,
}

impl LocalFileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let uri = format!("file://{}", path.display());
        Ok(Self { uri, path })
    }
}

impl MediaSource for LocalFileSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        let metadata =
            std::fs::metadata(&self.path).map_err(|error| SourceError::Io(error.to_string()))?;
        Ok(Some(metadata.len()))
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let mut file =
            File::open(&self.path).map_err(|error| SourceError::Io(error.to_string()))?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let mut reader: Box<dyn Read> = match range.length {
            Some(length) => Box::new(file.take(length)),
            None => Box::new(file),
        };
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| SourceError::Io(error.to_string()))?;
        Ok(bytes)
    }
}

/// A seekable Android content descriptor owned by the media source.
///
/// The descriptor is closed automatically when this value is dropped. `offset`
/// and `length` expose an `AssetFileDescriptor` slice as a zero-based media file.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct OwnedFileDescriptorSource {
    uri: String,
    file: File,
    offset: u64,
    length: Option<u64>,
}

/// Keeps an Android-owned descriptor registered until a synchronous native
/// source call either adopts it or returns an error.
///
/// Dropping the registration closes the descriptor when no `MediaSource`
/// consumed it. This closes the ownership gap between JNI validation and the
/// point where playback constructs `OwnedFileDescriptorSource`.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct AndroidOwnedFdRegistration {
    fd: RawFd,
}

#[cfg(target_os = "android")]
impl Drop for AndroidOwnedFdRegistration {
    fn drop(&mut self) {
        if let Ok(mut registry) = android_owned_fd_registry().lock() {
            let _ = registry.remove(&self.fd);
        }
    }
}

/// Registers a descriptor transferred by the Android host for one synchronous
/// native invocation. `source_from_uri` consumes the registered `File`; if the
/// invocation fails before that boundary, the returned guard closes it.
#[cfg(target_os = "android")]
pub fn register_android_owned_fd(file: File) -> Result<AndroidOwnedFdRegistration> {
    let fd = file.as_raw_fd();
    if fd < 0 {
        return Err(SourceError::InvalidFileDescriptorUri(format!(
            "negative descriptor {fd}"
        )));
    }
    let mut registry = android_owned_fd_registry()
        .lock()
        .map_err(|_| SourceError::Io("Android owned-fd registry mutex poisoned".to_string()))?;
    if registry.contains_key(&fd) {
        // The existing entry already owns this raw descriptor. Closing a second
        // File wrapper here would invalidate that entry, so discard only the
        // duplicate wrapper and preserve the original ownership.
        std::mem::forget(file);
        return Err(SourceError::InvalidFileDescriptorUri(format!(
            "descriptor {fd} is already awaiting source adoption"
        )));
    }
    registry.insert(fd, file);
    Ok(AndroidOwnedFdRegistration { fd })
}

#[cfg(target_os = "android")]
fn android_owned_fd_registry() -> &'static Mutex<HashMap<RawFd, File>> {
    static REGISTRY: OnceLock<Mutex<HashMap<RawFd, File>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "android")]
fn take_registered_android_owned_fd(fd: RawFd) -> Option<File> {
    android_owned_fd_registry().lock().ok()?.remove(&fd)
}

#[cfg(target_os = "android")]
impl OwnedFileDescriptorSource {
    /// Takes ownership of `fd`; callers must not close or reuse it afterwards.
    ///
    /// # Safety
    ///
    /// `fd` must be a valid, uniquely-owned, seekable descriptor.
    pub unsafe fn from_owned_fd(
        fd: RawFd,
        offset: u64,
        length: Option<u64>,
        uri: impl Into<String>,
    ) -> Result<Self> {
        if fd < 0 {
            return Err(SourceError::InvalidFileDescriptorUri(format!(
                "negative descriptor {fd}"
            )));
        }
        // SAFETY: ownership is transferred by the function contract.
        let file = unsafe { File::from_raw_fd(fd) };
        Self::from_owned_file(file, offset, length, uri.into())
    }

    fn from_owned_file(file: File, offset: u64, length: Option<u64>, uri: String) -> Result<Self> {
        let metadata = file
            .metadata()
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let length = length.or_else(|| metadata.len().checked_sub(offset));
        Ok(Self {
            uri,
            file,
            offset,
            length,
        })
    }

    unsafe fn open_uri(uri: &str) -> Result<Self> {
        let fd = parse_owned_fd(uri)?;
        // Safe URI dispatch may only consume descriptors registered by the JNI
        // transferred-fd contract. Never adopt a registry miss by raw number:
        // that could seize or double-close an unrelated process descriptor.
        // Direct native callers with unique ownership must use `from_owned_fd`.
        let file = take_registered_android_owned_fd(fd).ok_or_else(|| {
            SourceError::InvalidFileDescriptorUri(format!(
                "{uri} (descriptor was not explicitly transferred)"
            ))
        })?;
        let spec = parse_fd_uri(uri)?;
        Self::from_owned_file(file, spec.offset, spec.length, uri.to_string())
    }
}

#[cfg(target_os = "android")]
impl MediaSource for OwnedFileDescriptorSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        Ok(self.length)
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let length = match self.length {
            Some(total) if range.start >= total => return Ok(Vec::new()),
            Some(total) => Some(
                range
                    .length
                    .unwrap_or_else(|| total.saturating_sub(range.start))
                    .min(total.saturating_sub(range.start)),
            ),
            None => range.length,
        };
        let absolute_start = self.offset.checked_add(range.start).ok_or_else(|| {
            SourceError::Io("owned descriptor seek offset overflowed u64".to_string())
        })?;
        self.file
            .seek(SeekFrom::Start(absolute_start))
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        match length {
            Some(length) => (&mut self.file)
                .take(length)
                .read_to_end(&mut bytes)
                .map_err(|error| SourceError::Io(error.to_string()))?,
            None => self
                .file
                .read_to_end(&mut bytes)
                .map_err(|error| SourceError::Io(error.to_string()))?,
        };
        Ok(bytes)
    }
}

#[cfg(any(target_os = "android", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnedFdUri {
    fd: i32,
    offset: u64,
    length: Option<u64>,
}

#[cfg(any(target_os = "android", test))]
fn parse_fd_uri(uri: &str) -> Result<OwnedFdUri> {
    let body = uri
        .strip_prefix("fd://")
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
    let (fd, query) = body.split_once('?').unwrap_or((body, ""));
    let fd = parse_owned_fd_value(fd, uri)?;
    let mut offset = None;
    let mut length = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
        match key {
            "offset" => {
                if offset.is_some() {
                    return Err(SourceError::InvalidFileDescriptorUri(uri.to_string()));
                }
                offset = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| SourceError::InvalidFileDescriptorUri(uri.to_string()))?,
                );
            }
            "length" => {
                if length.is_some() {
                    return Err(SourceError::InvalidFileDescriptorUri(uri.to_string()));
                }
                length = Some(if value.is_empty() || value == "-1" {
                    None
                } else {
                    Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| SourceError::InvalidFileDescriptorUri(uri.to_string()))?,
                    )
                });
            }
            // Display names/URIs may be appended by the Android host for diagnostics.
            "name" | "display_uri" => {}
            _ => return Err(SourceError::InvalidFileDescriptorUri(uri.to_string())),
        }
    }
    Ok(OwnedFdUri {
        fd,
        offset: offset.unwrap_or(0),
        length: length.flatten(),
    })
}

#[cfg(target_os = "android")]
fn parse_owned_fd(uri: &str) -> Result<i32> {
    let body = uri
        .strip_prefix("fd://")
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
    let fd = body.split_once(['?', '/', '#']).map_or(body, |(fd, _)| fd);
    parse_owned_fd_value(fd, uri)
}

#[cfg(any(target_os = "android", test))]
fn parse_owned_fd_value(value: &str, uri: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))
}

pub struct HttpRangeSource {
    uri: String,
    agent: ureq::Agent,
    http_headers: Vec<(String, String)>,
    content_length: Option<u64>,
    cache_start: u64,
    cache_bytes: Vec<u8>,
    /// Cache depth target: how much *unread* data to keep buffered ahead of the
    /// reader. Deeper windows ride out longer origin hiccups; the cost is
    /// memory. This is deliberately not the on-the-wire request size -- see
    /// `HTTP_REQUEST_MAX_BYTES`.
    read_ahead_bytes: u64,
    /// Largest body a single request may ask for: `read_ahead_bytes` capped by
    /// `HTTP_REQUEST_MAX_BYTES`. The window is filled by successive requests of
    /// at most this size instead of one request for the whole window.
    request_bytes: u64,
    prefetch: Option<PendingHttpFetch>,
    /// Consecutive failed background prefetches. A failed prefetch is never
    /// fatal (the read path falls back to a synchronous fetch), but retrying on
    /// every read would hammer a sick origin, so the chain parks after a few and
    /// resumes once a synchronous fetch proves the origin is alive.
    prefetch_failures: u32,
}

struct PendingHttpFetch {
    range: ByteRange,
    handle: JoinHandle<Result<HttpRangeResponse>>,
}

/// Bytes fetched for one HTTP range request plus the resource total reported
/// by the server (`Content-Range` on 206, `Content-Length` on a whole-file
/// 200). The total lets callers backfill `content_length` when HEAD is
/// unavailable (e.g. servers answering HEAD with 405).
struct HttpRangeResponse {
    bytes: Vec<u8>,
    total_length: Option<u64>,
}

impl HttpRangeSource {
    const DEFAULT_READ_AHEAD_BYTES: u64 = 2 * 1024 * 1024;

    pub fn new(uri: impl Into<String>) -> Self {
        Self::with_http_headers(uri, Vec::new())
    }

    pub fn with_http_headers(uri: impl Into<String>, http_headers: Vec<(String, String)>) -> Self {
        Self::with_http_headers_and_read_ahead(uri, http_headers, None)
    }

    /// `read_ahead`: explicit read-ahead window in bytes; `None` (or `Some(0)`)
    /// falls back to the `ERIKA_HTTP_READAHEAD_BYTES` env override, then the
    /// 2 MiB engine default.
    pub fn with_http_headers_and_read_ahead(
        uri: impl Into<String>,
        http_headers: Vec<(String, String)>,
        read_ahead: Option<u64>,
    ) -> Self {
        let agent = http_agent();
        Self {
            uri: uri.into(),
            agent,
            http_headers,
            content_length: None,
            cache_start: 0,
            cache_bytes: Vec::new(),
            read_ahead_bytes: read_ahead
                .filter(|bytes| *bytes > 0)
                .unwrap_or_else(http_read_ahead_bytes),
            request_bytes: read_ahead
                .filter(|bytes| *bytes > 0)
                .unwrap_or_else(http_read_ahead_bytes)
                .min(HTTP_REQUEST_MAX_BYTES),
            prefetch: None,
            prefetch_failures: 0,
        }
    }

    fn cache_end(&self) -> u64 {
        self.cache_start
            .saturating_add(self.cache_bytes.len() as u64)
    }

    fn cached_slice(&self, range: ByteRange) -> Option<Vec<u8>> {
        let length = range.length?;
        let end = range.start.checked_add(length)?;
        if range.start < self.cache_start || end > self.cache_end() {
            return None;
        }
        let start_index = usize::try_from(range.start - self.cache_start).ok()?;
        let length = usize::try_from(length).ok()?;
        let end_index = start_index.checked_add(length)?;
        Some(self.cache_bytes[start_index..end_index].to_vec())
    }

    /// Cut the part of the cache that is further than `HTTP_CACHE_RETAIN_BYTES`
    /// behind the reader.
    ///
    /// The retained tail is what turns a small rewind into a cache hit instead
    /// of a fresh download. Trimming only once the tail is
    /// `HTTP_CACHE_TRIM_SLACK` over budget keeps the O(n) buffer move rare
    /// (once per few MiB of playback) instead of once per read.
    fn trim_cache(&mut self, range: ByteRange) {
        let Some(length) = range.length else {
            return;
        };
        // Measured from the *start* of the current read, never past it: the tail
        // is what a rewind can hit, but nothing the current read needs may be
        // dropped (a read larger than the retention budget still has to be
        // served from the cache it started in).
        let retain_floor = range.start.saturating_sub(HTTP_CACHE_RETAIN_BYTES);
        let drop = retain_floor.saturating_sub(self.cache_start);
        if drop < HTTP_CACHE_TRIM_SLACK {
            return;
        }
        let drop = drop.min(self.cache_bytes.len() as u64) as usize;
        self.cache_bytes.drain(..drop);
        self.cache_start = self.cache_start.saturating_add(drop as u64);
        http_trace_log(format!(
            "{{\"event\":\"http_cache_trim\",\"dropped\":{},\"cache_start\":{},\"cache_end\":{}}}",
            drop,
            self.cache_start,
            self.cache_end(),
        ));
    }

    fn fetch_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let response = fetch_http_range(
            &self.agent,
            &self.uri,
            &self.http_headers,
            range,
            "http_range",
        )?;
        if self.content_length.is_none() {
            self.content_length = response.total_length;
        }
        // A synchronous fetch that worked proves the origin is alive: let the
        // background prefetch chain resume.
        self.prefetch_failures = 0;
        Ok(response.bytes)
    }

    fn fetch_length(&mut self, range: ByteRange) -> Result<Option<u64>> {
        Ok(match range.length {
            Some(length) => {
                // At least the caller's request with a capped piece of look-ahead,
                // and never more than one request body may carry. A caller asking
                // for more than the cap gets a short read (the read loops re-issue
                // the rest), which keeps "no request body exceeds the cap" an
                // invariant instead of a property of today's callers.
                let mut wanted = length.max(self.request_bytes).min(HTTP_REQUEST_MAX_BYTES);
                if let Some(total) = self.content_length.or_else(|| self.len().ok().flatten()) {
                    if range.start >= total {
                        return Ok(Some(0));
                    }
                    // Never ask past the end of the resource either.
                    wanted = wanted.min(total.saturating_sub(range.start));
                }
                Some(wanted)
            }
            None => None,
        })
    }

    /// Join a pending prefetch and fold its bytes into the cache.
    ///
    /// A prefetch always starts at `cache_end`, so its bytes are contiguous
    /// with the cache and can simply be appended -- absorbing a finished piece
    /// is what lets the next one start. A failure is recorded, never returned:
    /// a dead background fetch must not end the read, the caller falls back to
    /// a synchronous fetch.
    fn absorb_prefetch(&mut self, pending: PendingHttpFetch) {
        let start = pending.range.start;
        match pending.join() {
            Ok(response) => {
                if self.content_length.is_none() {
                    self.content_length = response.total_length;
                }
                if !response.bytes.is_empty() && start == self.cache_end() {
                    self.cache_bytes.extend_from_slice(&response.bytes);
                    self.prefetch_failures = 0;
                    http_trace_log(format!(
                        "{{\"event\":\"http_prefetch_absorbed\",\"start\":{},\"bytes\":{},\"cache_end\":{}}}",
                        start,
                        response.bytes.len(),
                        self.cache_end(),
                    ));
                } else if !response.bytes.is_empty() {
                    // The reader moved (the window was re-anchored) while this
                    // piece was in flight, so it no longer fits the cache.
                    http_trace_log(format!(
                        "{{\"event\":\"http_prefetch_dropped\",\"start\":{},\"cache_start\":{},\"cache_end\":{},\"bytes\":{}}}",
                        start,
                        self.cache_start,
                        self.cache_end(),
                        response.bytes.len(),
                    ));
                }
            }
            Err(error) => {
                self.prefetch_failures = self.prefetch_failures.saturating_add(1);
                http_trace_log(format!(
                    "{{\"event\":\"http_prefetch_failed\",\"start\":{},\"failures\":{},\"error\":\"{}\"}}",
                    start,
                    self.prefetch_failures,
                    json_escape(&error.to_string()),
                ));
            }
        }
    }

    /// Fold in a finished prefetch without blocking. Called before the hit
    /// checks so they see everything that has already arrived.
    fn settle_finished_prefetch(&mut self) {
        if !self
            .prefetch
            .as_ref()
            .is_some_and(PendingHttpFetch::is_finished)
        {
            return;
        }
        if let Some(pending) = self.prefetch.take() {
            self.absorb_prefetch(pending);
        }
    }

    /// Resolve the in-flight prefetch against an incoming read.
    ///
    /// * the piece covers the read, or the read needs bytes at/past the cache
    ///   end (which is exactly where the piece starts) -> join it; the reader is
    ///   waiting, and issuing a synchronous fetch of the same bytes would
    ///   download the range twice;
    /// * the read jumped past it -> drop the handle: its bytes sit behind the
    ///   new position (the detached thread finishes its request and is thrown
    ///   away);
    /// * otherwise (a rewind, or a read inside the cache) -> leave it running:
    ///   it holds *forward* data this read still wants. The previous code threw
    ///   an in-flight piece away here, which is one reason a rewind used to
    ///   re-download everything.
    fn settle_prefetch_for(&mut self, range: ByteRange) {
        let Some(pending) = self.prefetch.as_ref().map(|pending| pending.range) else {
            return;
        };
        let covers = range_contains(pending, range);
        let stale = pending
            .length
            .is_some_and(|length| range.start >= pending.start.saturating_add(length));
        let needs_this_piece = range
            .length
            .is_some_and(|length| range.start.saturating_add(length) > self.cache_end());
        if (covers || needs_this_piece) && !stale {
            if !self
                .prefetch
                .as_ref()
                .is_some_and(PendingHttpFetch::is_finished)
            {
                // Joining is cheaper than issuing a duplicate download of bytes
                // that are already on the wire.
                http_trace_log(format!(
                    "{{\"event\":\"http_prefetch_pending\",\"decision\":\"join\",\"start\":{},\"requested_start\":{}}}",
                    pending.start, range.start,
                ));
            }
            if let Some(pending) = self.prefetch.take() {
                self.absorb_prefetch(pending);
            }
        } else if stale {
            if let Some(pending) = self.prefetch.take() {
                http_trace_log(format!(
                    "{{\"event\":\"http_prefetch_stale\",\"start\":{},\"requested_start\":{}}}",
                    pending.range.start, range.start,
                ));
            }
        }
    }

    /// Keep the cache near its configured depth, one capped piece at a time.
    ///
    /// Each call first folds in a finished piece and then, if the cache is below
    /// the refill threshold, starts the next one from `cache_end`. So the window
    /// still fills to `read_ahead_bytes`, but no single request ever asks for
    /// more than `request_bytes` -- which is what keeps a slow origin inside the
    /// client's body deadline.
    fn maybe_start_prefetch(&mut self, range: ByteRange) {
        self.settle_finished_prefetch();
        if self.prefetch.is_some() || self.cache_bytes.is_empty() {
            return;
        }
        if self.prefetch_failures >= HTTP_PREFETCH_MAX_FAILURES {
            return;
        }
        let Some(length) = range.length else {
            return;
        };
        let Some(total) = self.content_length else {
            return;
        };
        let Some(end) = range.start.checked_add(length) else {
            return;
        };
        let cache_end = self.cache_end();
        if end > cache_end || cache_end >= total {
            return;
        }
        let remaining = cache_end.saturating_sub(end);
        // Refill towards the configured depth, not towards half of it: the knob
        // is the cushion the reader keeps ahead, and pieces arrive one at a time
        // now, so stopping at half of it would leave the setting unmet (a 32 MiB
        // window used to settle around `read_ahead / 2 + one piece`).
        if remaining >= self.read_ahead_bytes {
            return;
        }
        let length = self.request_bytes.min(total.saturating_sub(cache_end));
        if length == 0 {
            return;
        }
        let prefetch_range = ByteRange {
            start: cache_end,
            length: Some(length),
        };
        self.prefetch = Some(PendingHttpFetch::spawn(
            self.uri.clone(),
            self.http_headers.clone(),
            prefetch_range,
        ));
    }

    /// Make the cache cover `range`, downloading only what is missing.
    fn fetch_missing(&mut self, range: ByteRange) -> Result<()> {
        let Some(length) = range.length else {
            // Open-ended read (whole sidecar files): stream to EOF in capped
            // pieces. The previous shape asked for the entire tail in one
            // request, which meets the same body deadline on a slow link.
            return self.stream_to_eof(range.start);
        };
        let end = range.start.saturating_add(length);
        if self.cache_bytes.is_empty() {
            // Nothing is buffered, so there is no anchor to continue from:
            // start the window at the read. A request from byte zero is also the
            // only shape whose 200 answer may legitimately carry a whole-file
            // payload, so a mid-file read must not be widened into one.
            return self.reanchor_window(range);
        }
        if range.start < self.cache_start {
            // A rewind past the retained tail: nothing in the cache sits before
            // the read. Re-anchor the window here -- this is the one path that
            // still discards the buffered future, and it matches what every
            // player does once a seek lands past its back buffer.
            return self.reanchor_window(range);
        }
        if range.start > self.cache_end().saturating_add(HTTP_REQUEST_MAX_BYTES) {
            // A forward jump beyond the window: downloading the gap would cost
            // more than starting over at the read.
            return self.reanchor_window(range);
        }
        let mut pieces = 0;
        while self.cache_end() < end {
            if pieces >= HTTP_FETCH_MAX_PIECES_PER_READ {
                // Walking away here would make the caller see a short (or empty,
                // i.e. EOF) read for bytes that exist, so fail loudly instead.
                return Err(SourceError::Http(format!(
                    "origin answered {pieces} short pieces without covering bytes {}..{end}",
                    range.start,
                )));
            }
            pieces += 1;
            let start = self.cache_end();
            let Some(request_length) = self.fetch_length(ByteRange {
                start,
                length: Some(end - start),
            })?
            else {
                break;
            };
            if request_length == 0 {
                break;
            }
            let fetched = self.fetch_range(ByteRange {
                start,
                length: Some(request_length),
            })?;
            if fetched.is_empty() {
                // EOF, or an origin that answered with no payload.
                break;
            }
            self.cache_bytes.extend_from_slice(&fetched);
            // A short answer (a server that caps response sizes, or EOF before
            // the read is covered) just means the loop owes another piece.
        }
        Ok(())
    }

    /// Start a fresh window at the read position, discarding what the cache held
    /// (including any in-flight prefetch anchored to the old cache end).
    fn reanchor_window(&mut self, range: ByteRange) -> Result<()> {
        if let Some(pending) = self.prefetch.take() {
            http_trace_log(format!(
                "{{\"event\":\"http_prefetch_stale\",\"start\":{},\"requested_start\":{}}}",
                pending.range.start, range.start,
            ));
        }
        let Some(request_length) = self.fetch_length(range)? else {
            return Ok(());
        };
        if request_length == 0 {
            self.cache_start = range.start;
            self.cache_bytes.clear();
            return Ok(());
        }
        let fetched = self.fetch_range(ByteRange {
            start: range.start,
            length: Some(request_length),
        })?;
        self.cache_start = range.start;
        self.cache_bytes = fetched;
        http_trace_log(format!(
            "{{\"event\":\"http_cache_reanchored\",\"start\":{},\"bytes\":{}}}",
            range.start,
            self.cache_bytes.len(),
        ));
        Ok(())
    }

    /// Read from `start` to EOF in capped pieces.
    ///
    /// The loop ends at the resource total when it is known, at a short answer
    /// (EOF) otherwise, and at `HTTP_STREAM_TO_EOF_BYTE_LIMIT` as a hard stop so
    /// an origin that keeps answering full pieces cannot grow the cache without
    /// bound. It deliberately has no attempt cap: a sidecar larger than a few
    /// pieces must not be truncated (the old shape read the entire tail in one
    /// request, which is unbounded in the other direction).
    fn stream_to_eof(&mut self, start: u64) -> Result<()> {
        self.prefetch = None;
        self.cache_start = start;
        self.cache_bytes.clear();
        while self.cache_bytes.len() as u64 <= HTTP_STREAM_TO_EOF_BYTE_LIMIT {
            let chunk_start = self.cache_end();
            if let Some(total) = self.content_length
                && chunk_start >= total
            {
                break;
            }
            let fetched = self.fetch_range(ByteRange {
                start: chunk_start,
                length: Some(HTTP_REQUEST_MAX_BYTES),
            })?;
            if fetched.is_empty() {
                break;
            }
            let short = (fetched.len() as u64) < HTTP_REQUEST_MAX_BYTES;
            self.cache_bytes.extend_from_slice(&fetched);
            if short {
                break;
            }
        }
        Ok(())
    }
}

impl PendingHttpFetch {
    fn spawn(uri: String, http_headers: Vec<(String, String)>, range: ByteRange) -> Self {
        http_trace_log(format!(
            "{{\"event\":\"http_prefetch_start\",\"start\":{},\"length\":{}}}",
            range.start,
            range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
        ));
        let handle = thread::spawn(move || {
            let agent = http_agent();
            fetch_http_range(&agent, &uri, &http_headers, range, "http_prefetch_range")
        });
        Self { range, handle }
    }

    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Join the in-flight request. A panic in the worker is reported as a fetch
    /// error instead of unwinding into the demuxer thread.
    fn join(self) -> Result<HttpRangeResponse> {
        let Self { range, handle } = self;
        let started = Instant::now();
        let result = handle
            .join()
            .map_err(|_| SourceError::Http("http prefetch thread panicked".to_string()))
            .and_then(|response| response);
        http_trace_log(format!(
            "{{\"event\":\"http_prefetch_join\",\"start\":{},\"length\":{},\"elapsed_ms\":{:.3}}}",
            range.start,
            range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        result
    }
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .build()
        .into()
}

const HTTP_FETCH_MAX_ATTEMPTS: u32 = 3;
/// Total attempts allowed for one logical fetch once attempts start making
/// progress (see `HttpRetryGate`).
const HTTP_FETCH_MAX_RESUME_ATTEMPTS: u32 = 8;
const HTTP_FETCH_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_millis(200), Duration::from_secs(1)];
/// Wall-clock ceiling on one logical fetch, retries and backoff included.
///
/// `read_range` runs on the demuxer thread, so every retry freezes playback;
/// this bounds the stall. It is deliberately generous: with the 4 MiB request
/// cap a fetch can legitimately need several 15 s attempts on a slow origin,
/// and giving up ends playback (EIO is terminal) rather than merely stalling
/// it. Attempts without any progress still stop after
/// `HTTP_FETCH_MAX_ATTEMPTS`, so a broken origin fails fast.
///
/// The ceiling is enforced *between* attempts, so the true worst case adds one
/// attempt's own timeouts (connect + headers + the body deadline) on top of it.
const HTTP_FETCH_TOTAL_BUDGET: Duration = Duration::from_secs(120);

/// Hard ceiling on the body of one HTTP request.
///
/// Deliberately *not* the read-ahead window. ureq's `timeout_recv_response`
/// (15 s) also bounds the body phase, so one request for N MiB demands a
/// sustained N/15 MiB/s from the origin: fetching a 32 MiB window as a single
/// request needs ~18 Mbps and fails on anything slower with
/// `timeout: receive response` -> EIO -> terminal playback error. Capping a
/// request at 4 MiB drops that floor to ~2.2 Mbps, and the window is still
/// filled to its configured depth by successive capped requests.
const HTTP_REQUEST_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// How much already-played data stays in the cache. A rewind inside this tail is
/// served locally instead of re-downloaded: mpv keeps 50 MiB of back buffer by
/// default (`--demuxer-max-back-bytes`), VLC reuses a 3x4 MiB ring set. 16 MiB
/// sits between them and covers a 10 s step at up to ~13 Mbps.
const HTTP_CACHE_RETAIN_BYTES: u64 = 16 * 1024 * 1024;

/// The retained tail is only cut once it is this much over budget, so the O(n)
/// buffer move happens per few MiB of playback instead of once per read.
const HTTP_CACHE_TRIM_SLACK: u64 = 8 * 1024 * 1024;

/// Hard stop for open-ended reads (`read_uri_to_end`: danmaku/subtitle sidecars).
/// Matches the kernel's other sidecar ceilings; it only exists so an origin that
/// answers full pieces forever cannot grow the cache without bound.
const HTTP_STREAM_TO_EOF_BYTE_LIMIT: u64 = 256 * 1024 * 1024;

/// Most pieces one read may pull. An origin that answers short bodies needs
/// several pieces per read (that is normal); this only stops a pathological one,
/// and hitting it is reported as an error rather than a silent short read.
const HTTP_FETCH_MAX_PIECES_PER_READ: u32 = 64;

/// Consecutive background-prefetch failures before the chain is parked until a
/// synchronous fetch succeeds.
const HTTP_PREFETCH_MAX_FAILURES: u32 = 3;

/// Retry policy for one logical fetch (every attempt at the same range).
///
/// Two-tier: without progress the old behaviour stands (three attempts, so a
/// broken origin fails fast instead of freezing the demuxer thread); with
/// progress the fetch may keep resuming, because on a slow link a capped 4 MiB
/// request legitimately spans several 15 s attempts. Resuming is bounded by the
/// attempt count and `HTTP_FETCH_TOTAL_BUDGET` so a foreground stall stays
/// finite.
struct HttpRetryGate {
    started: Instant,
    attempts: u32,
    attempts_without_progress: u32,
    last_bytes: u64,
}

impl HttpRetryGate {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            attempts: 0,
            attempts_without_progress: 0,
            last_bytes: 0,
        }
    }

    /// Note that an attempt is starting; returns its 1-based number.
    fn begin_attempt(&mut self) -> u32 {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts
    }

    /// Whether the wall-clock ceiling is already spent. Checked before an
    /// attempt starts, because the per-request timeouts (connect + headers +
    /// the body deadline) can add tens of seconds to whatever the budget left
    /// over when the attempt began.
    fn expired(&self) -> bool {
        self.started.elapsed() >= HTTP_FETCH_TOTAL_BUDGET
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Record a failed attempt (`received` = bytes of this logical fetch so
    /// far): `Some(backoff)` to wait and try again, `None` to give up.
    fn fail(&mut self, received: u64) -> Option<Duration> {
        if received > self.last_bytes {
            self.last_bytes = received;
            self.attempts_without_progress = 0;
        } else {
            self.attempts_without_progress = self.attempts_without_progress.saturating_add(1);
        }
        if self.attempts_without_progress >= HTTP_FETCH_MAX_ATTEMPTS
            || self.attempts >= HTTP_FETCH_MAX_RESUME_ATTEMPTS
        {
            return None;
        }
        let backoff = http_retry_backoff(self.attempts);
        if self.started.elapsed().saturating_add(backoff) >= HTTP_FETCH_TOTAL_BUDGET {
            return None;
        }
        Some(backoff)
    }
}

fn http_retry_backoff(attempt: u32) -> Duration {
    let index = usize::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    HTTP_FETCH_RETRY_BACKOFF
        .get(index)
        .copied()
        .unwrap_or(Duration::from_secs(1))
}

/// Whether a failed HTTP exchange is worth retrying: transport errors and 5xx
/// responses are transient; 4xx responses are deterministic client errors.
fn http_error_is_retryable(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::StatusCode(status) => *status >= 500,
        _ => true,
    }
}

/// Parses the `total` out of a `Content-Range: bytes start-end/total` header.
/// Returns `None` for missing headers, unsatisfied-range (`*/total` still
/// yields the total), and unknown totals (`bytes 0-1/*`).
fn parse_content_range_total(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (_, total) = rest.rsplit_once('/')?;
    total.trim().parse::<u64>().ok()
}

/// Parses the first byte offset out of a `Content-Range: bytes start-end/total`
/// header. `None` for an unsatisfied-range form (`bytes */total`), which names
/// no offset.
fn parse_content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, _) = rest.rsplit_once('/')?;
    let (start, _) = range.trim().split_once('-')?;
    start.trim().parse::<u64>().ok()
}

/// The strongest entity validator the response offers, preferred in the order
/// RFC 9110 recommends for `If-Range`.
fn response_entity_validator<T>(response: &ureq::http::Response<T>) -> Option<String> {
    ["etag", "last-modified"].into_iter().find_map(|name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// Learns the total length from a one-byte GET, for origins that reject HEAD.
///
/// The body is deliberately never read. An origin that rejects HEAD *and*
/// ignores Range answers 200 with the whole object, so buffering the response
/// would turn a `len()` call into a full download of the media -- gigabytes
/// into memory before playback, just to learn a number the headers already
/// carry.
fn probe_http_total_length(
    agent: &ureq::Agent,
    uri: &str,
    http_headers: &[(String, String)],
) -> Result<Option<u64>> {
    let probe = ByteRange {
        start: 0,
        length: Some(1),
    };
    let mut request = agent.get(uri).header("Range", &http_range_header(probe));
    for (name, value) in http_headers {
        request = request.header(name, value);
    }
    let response = request
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| {
            http_trace_log(format!(
                "{{\"event\":\"http_length_probe_error\",\"phase\":\"request\",\"error\":\"{}\"}}",
                json_escape(&error.to_string()),
            ));
            SourceError::Http(error.to_string())
        })?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let total_length = match status {
        206 | 416 => header("content-range")
            .as_deref()
            .and_then(parse_content_range_total),
        // Range was ignored; Content-Length is the whole object, which is
        // exactly the total being probed for.
        200 => header("content-length").and_then(|value| value.trim().parse::<u64>().ok()),
        status if status >= 400 => {
            let error = SourceError::Http(format!("http status: {status}"));
            http_trace_log(format!(
                "{{\"event\":\"http_length_probe_error\",\"phase\":\"status\",\"status\":{status}}}"
            ));
            return Err(error);
        }
        _ => None,
    };
    http_trace_log(format!(
        "{{\"event\":\"http_length_probe\",\"status\":{},\"total\":{}}}",
        status,
        total_length.map_or_else(|| "null".to_string(), |total| total.to_string()),
    ));
    Ok(total_length)
}

fn http_range_header(range: ByteRange) -> String {
    match range.length {
        Some(length) if length > 0 => {
            let end = range.start.saturating_add(length).saturating_sub(1);
            format!("bytes={}-{}", range.start, end)
        }
        _ => format!("bytes={}-", range.start),
    }
}

fn fetch_http_range(
    agent: &ureq::Agent,
    uri: &str,
    http_headers: &[(String, String)],
    range: ByteRange,
    event: &str,
) -> Result<HttpRangeResponse> {
    let mut bytes = Vec::new();
    let mut total_length = None;
    // Entity validator from the first response. A resumed request replays it as
    // `If-Range` so an origin that re-encoded or load-balanced to a different
    // variant answers 200 (which the status check below rejects for a non-zero
    // start) instead of handing back bytes from a different object to be spliced
    // onto the prefix we already hold.
    let mut validator: Option<String> = None;
    let mut gate = HttpRetryGate::new();
    loop {
        let attempt = gate.begin_attempt();
        if gate.expired() {
            // The ceiling is enforced between attempts; without this check the
            // last attempt could start just under the budget and then run to its
            // own timeouts, overshooting the stated bound by tens of seconds.
            http_trace_log(format!(
                "{{\"event\":\"{}_error\",\"phase\":\"budget\",\"attempt\":{},\"start\":{},\"elapsed_ms\":{:.3}}}",
                event,
                attempt,
                range.start,
                gate.elapsed().as_secs_f64() * 1000.0,
            ));
            return Err(SourceError::Http(format!(
                "fetch budget of {:?} exhausted for bytes {}..",
                HTTP_FETCH_TOTAL_BUDGET, range.start,
            )));
        }
        // Resume from what already arrived: earlier attempts keep their bytes
        // and the Range start advances past them.
        let received = bytes.len() as u64;
        if range.length.is_some_and(|length| received >= length) {
            // A body error surfaced after every requested byte arrived; the
            // payload is complete, so do not re-request an open-ended tail.
            return Ok(HttpRangeResponse {
                bytes,
                total_length,
            });
        }
        let resume_range = ByteRange {
            start: range.start.saturating_add(received),
            length: range.length.map(|length| length.saturating_sub(received)),
        };
        let header = http_range_header(resume_range);
        let started = Instant::now();
        let mut request = agent.get(uri).header("Range", &header);
        for (name, value) in http_headers {
            request = request.header(name, value);
        }
        if received > 0
            && let Some(validator) = validator.as_deref()
        {
            request = request.header("If-Range", validator);
        }
        let mut response = match request.call() {
            Ok(response) => response,
            Err(error) => {
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"request\",\"attempt\":{},\"start\":{},\"length\":{},\"elapsed_ms\":{:.3},\"error\":\"{}\"}}",
                    event,
                    attempt,
                    resume_range.start,
                    resume_range
                        .length
                        .map_or_else(|| "null".to_string(), |length| length.to_string()),
                    started.elapsed().as_secs_f64() * 1000.0,
                    json_escape(&error.to_string()),
                ));
                if http_error_is_retryable(&error)
                    && let Some(backoff) = gate.fail(received)
                {
                    http_trace_log(format!(
                        "{{\"event\":\"{}_retry\",\"phase\":\"request\",\"attempt\":{},\"start\":{},\"received\":{},\"backoff_ms\":{}}}",
                        event,
                        attempt,
                        resume_range.start,
                        received,
                        backoff.as_millis(),
                    ));
                    thread::sleep(backoff);
                    continue;
                }
                return Err(SourceError::Http(error.to_string()));
            }
        };
        let status = response.status().as_u16();
        match status {
            206 => {
                let content_range = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                // A resumed request must continue exactly where the prefix
                // ends. A server that answers 206 from a different offset --
                // or a changed entity that ignored If-Range -- would otherwise
                // be spliced onto the bytes already held and returned as
                // silently corrupt media.
                if let Some(start) = content_range.as_deref().and_then(parse_content_range_start)
                    && start != resume_range.start
                {
                    http_trace_log(format!(
                        "{{\"event\":\"{}_error\",\"phase\":\"content_range\",\"attempt\":{},\"start\":{},\"served_start\":{}}}",
                        event, attempt, resume_range.start, start,
                    ));
                    return Err(SourceError::Http(format!(
                        "server served range from {start}, expected {}",
                        resume_range.start
                    )));
                }
                if total_length.is_none() {
                    total_length = content_range.as_deref().and_then(parse_content_range_total);
                }
                if validator.is_none() {
                    validator = response_entity_validator(&response);
                }
            }
            200 => {
                if resume_range.start > 0 {
                    // The server sent the file from byte zero: treating that
                    // payload as `resume_range.start` data would silently
                    // corrupt the cache, so fail instead of retrying.
                    http_trace_log(format!(
                        "{{\"event\":\"{}_error\",\"phase\":\"status\",\"attempt\":{},\"start\":{},\"status\":200}}",
                        event, attempt, resume_range.start,
                    ));
                    return Err(SourceError::Http(
                        "server ignored Range request (status 200)".to_string(),
                    ));
                }
                if total_length.is_none() {
                    total_length = response
                        .headers()
                        .get("content-length")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok());
                }
                if validator.is_none() {
                    validator = response_entity_validator(&response);
                }
            }
            // ureq maps 4xx/5xx to Error::StatusCode before this point; any
            // other status (204, 304, ...) carries no usable range payload.
            _ => {
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"status\",\"attempt\":{},\"start\":{},\"status\":{}}}",
                    event, attempt, resume_range.start, status,
                ));
                return Err(SourceError::Http(format!(
                    "unexpected HTTP status {status} for Range request"
                )));
            }
        }
        if let Err(error) = response.body_mut().as_reader().read_to_end(&mut bytes) {
            http_trace_log(format!(
                "{{\"event\":\"{}_error\",\"phase\":\"body\",\"attempt\":{},\"start\":{},\"length\":{},\"status\":{},\"bytes\":{},\"elapsed_ms\":{:.3},\"error\":\"{}\"}}",
                event,
                attempt,
                resume_range.start,
                resume_range
                    .length
                    .map_or_else(|| "null".to_string(), |length| length.to_string()),
                status,
                bytes.len(),
                started.elapsed().as_secs_f64() * 1000.0,
                json_escape(&error.to_string()),
            ));
            if let Some(backoff) = gate.fail(bytes.len() as u64) {
                http_trace_log(format!(
                    "{{\"event\":\"{}_retry\",\"phase\":\"body\",\"attempt\":{},\"start\":{},\"received\":{},\"backoff_ms\":{}}}",
                    event,
                    attempt,
                    range.start.saturating_add(bytes.len() as u64),
                    bytes.len(),
                    backoff.as_millis(),
                ));
                thread::sleep(backoff);
                continue;
            }
            return Err(SourceError::Http(error.to_string()));
        }
        http_trace_log(format!(
            "{{\"event\":\"{}\",\"attempt\":{},\"start\":{},\"length\":{},\"status\":{},\"bytes\":{},\"elapsed_ms\":{:.3}}}",
            event,
            attempt,
            range.start,
            range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
            status,
            bytes.len(),
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        return Ok(HttpRangeResponse {
            bytes,
            total_length,
        });
    }
}

impl std::fmt::Debug for HttpRangeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRangeSource")
            .field("uri", &redacted_uri(&self.uri))
            .field("content_length", &self.content_length)
            .field("cache_start", &self.cache_start)
            .field("cache_bytes", &self.cache_bytes.len())
            .field("read_ahead_bytes", &self.read_ahead_bytes)
            .field("request_bytes", &self.request_bytes)
            .finish()
    }
}

impl MediaSource for HttpRangeSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        if self.content_length.is_some() {
            return Ok(self.content_length);
        }
        let started = Instant::now();
        http_trace_log(format!(
            "[erika-http-trace] stage=head_request uri={} cache_start={} cache_end={} read_ahead={}",
            redacted_uri(&self.uri),
            self.cache_start,
            self.cache_end(),
            self.read_ahead_bytes,
        ));
        // Keep metadata probing off the range-request pool. Some HTTP/1.0
        // servers close a HEAD connection without an explicit Connection
        // header; reusing that stale socket for the first GET otherwise
        // surfaces as `Peer disconnected`.
        // TODO(perf): cache a dedicated metadata agent on `HttpRangeSource` so
        // repeated `len()` probes without Content-Length do not rebuild the TLS
        // client, while still keeping HEAD sockets out of the range-request pool.
        let head_agent = http_agent();
        let mut gate = HttpRetryGate::new();
        let head_error = loop {
            let attempt = gate.begin_attempt();
            let mut request = head_agent.head(&self.uri);
            for (name, value) in &self.http_headers {
                request = request.header(name, value);
            }
            match request.call() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let length = response
                        .headers()
                        .get("content-length")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok());
                    // Some streaming servers synthesize an empty HEAD body and
                    // incorrectly report that body length as the media length.
                    // A zero-byte media resource is not useful to the demuxer,
                    // so verify it with a one-byte range request before caching
                    // the value. Content-Range carries the actual object size.
                    if length == Some(0) {
                        http_trace_log(format!(
                            "[erika-http-trace] stage=head_zero_length_fallback status={} elapsed_ms={:.3}",
                            status,
                            started.elapsed().as_secs_f64() * 1000.0,
                        ));
                        return match probe_http_total_length(
                            &self.agent,
                            &self.uri,
                            &self.http_headers,
                        ) {
                            Ok(total_length) => {
                                self.content_length = total_length;
                                Ok(self.content_length)
                            }
                            Err(error) => Err(SourceError::Http(format!(
                                "HEAD reported Content-Length: 0 and range probe failed: {error}"
                            ))),
                        };
                    }
                    self.content_length = length;
                    http_trace_log(format!(
                        "[erika-http-trace] stage=head_response status={} length={} elapsed_ms={:.3}",
                        status,
                        length.map_or_else(|| "null".to_string(), |length| length.to_string()),
                        started.elapsed().as_secs_f64() * 1000.0,
                    ));
                    return Ok(length);
                }
                Err(error) => {
                    http_trace_log(format!(
                        "[erika-http-trace] stage=head_error attempt={} elapsed_ms={:.3} error={}",
                        attempt,
                        started.elapsed().as_secs_f64() * 1000.0,
                        json_escape(&error.to_string()),
                    ));
                    // A HEAD probe carries no body, so there is no progress to
                    // weigh: the gate falls back to the stall allowance.
                    if http_error_is_retryable(&error)
                        && let Some(backoff) = gate.fail(0)
                    {
                        http_trace_log(format!(
                            "[erika-http-trace] stage=head_retry attempt={} backoff_ms={}",
                            attempt,
                            backoff.as_millis(),
                        ));
                        thread::sleep(backoff);
                        continue;
                    }
                    break error;
                }
            }
        };
        // Some servers reject HEAD (e.g. 405) yet still serve ranges. Probe
        // with a one-byte GET and take the total from Content-Range.
        http_trace_log(format!(
            "[erika-http-trace] stage=head_fallback_range error={}",
            json_escape(&head_error.to_string()),
        ));
        match probe_http_total_length(&self.agent, &self.uri, &self.http_headers) {
            Ok(total_length) => {
                self.content_length = total_length;
                Ok(self.content_length)
            }
            Err(_) => Err(SourceError::Http(head_error.to_string())),
        }
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        // Fold in a finished piece first: the hit checks below only see what is
        // already in `cache_bytes`.
        self.settle_finished_prefetch();
        self.trim_cache(range);

        if let Some(bytes) = self.cached_slice(range) {
            self.maybe_start_prefetch(range);
            http_trace_log(format!(
                "{{\"event\":\"http_cache_hit\",\"start\":{},\"length\":{},\"bytes\":{}}}",
                range.start,
                range.length.unwrap_or_default(),
                bytes.len(),
            ));
            return Ok(bytes);
        }

        if let Some(total) = self.content_length
            && range.start >= total
        {
            // Past EOF: an empty read, which the AVIO layer turns into EOF.
            return Ok(Vec::new());
        }

        // The read is not covered yet: let an in-flight prefetch land if it is
        // the shortest path to those bytes, then fetch what is still missing.
        self.settle_prefetch_for(range);
        if let Some(bytes) = self.cached_slice(range) {
            self.maybe_start_prefetch(range);
            return Ok(bytes);
        }

        self.fetch_missing(range)?;
        self.maybe_start_prefetch(range);

        // Serve whatever the cache now holds from the read position. A short
        // read is legitimate (EOF, or an origin that answered short); an empty
        // result becomes EOF in the AVIO layer, so it is only allowed when the
        // resource really ends before the read.
        let offset = usize::try_from(range.start.saturating_sub(self.cache_start)).unwrap_or(0);
        let Some(tail) = self.cache_bytes.get(offset..) else {
            if let Some(total) = self.content_length
                && range.start < total
            {
                return Err(SourceError::Http(format!(
                    "no data for bytes {}.. (origin total {total})",
                    range.start,
                )));
            }
            return Ok(Vec::new());
        };
        let copy_len = range.length.map_or(tail.len(), |length| {
            usize::try_from(length)
                .unwrap_or(usize::MAX)
                .min(tail.len())
        });
        Ok(tail[..copy_len].to_vec())
    }
}

pub fn source_from_uri(uri: &str) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint(uri, MediaSourceHint::Auto)
}

/// Reads an entire URI through the same MediaSource abstraction used by FFmpeg.
///
/// This is intentionally synchronous for small sidecar assets such as danmaku or
/// subtitle files. On Android it also establishes and completes the ownership
/// transfer for `fd://` descriptors within the native call.
pub fn read_uri_to_end(uri: &str) -> Result<Vec<u8>> {
    let mut source = source_from_uri(uri)?;
    source.read_range(ByteRange::suffix_from(0))
}

pub fn source_from_uri_with_hint(
    uri: &str,
    source_hint: MediaSourceHint,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers(uri, source_hint, Vec::new())
}

pub fn source_from_uri_with_hint_and_headers(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_options(uri, source_hint, http_headers, None)
}

/// `http_read_ahead_bytes` only applies to HTTP(S) sources and overrides the
/// per-request read-ahead window; `None` keeps the default resolution
/// (env override, then the 2 MiB engine default).
pub fn source_from_uri_with_options(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    http_read_ahead_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    match source_hint {
        MediaSourceHint::Auto => source_from_auto_uri(uri, http_headers, http_read_ahead_bytes),
        MediaSourceHint::LocalFile => source_from_local_uri(uri),
        MediaSourceHint::Http => {
            if uri.starts_with("http://") || uri.starts_with("https://") {
                Ok(Box::new(HttpRangeSource::with_http_headers_and_read_ahead(
                    uri,
                    http_headers,
                    http_read_ahead_bytes,
                )))
            } else {
                Err(SourceError::Unsupported(uri.to_string()))
            }
        }
    }
}

fn source_from_auto_uri(
    uri: &str,
    http_headers: Vec<(String, String)>,
    http_read_ahead_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    if uri.starts_with("fd://") {
        return source_from_local_uri(uri);
    }
    if let Some(path) = uri.strip_prefix("file://") {
        return Ok(Box::new(LocalFileSource::open(path)?));
    }
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(Box::new(HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            http_headers,
            http_read_ahead_bytes,
        )));
    }
    let path = Path::new(uri);
    if path.exists() {
        return Ok(Box::new(LocalFileSource::open(path)?));
    }
    Err(SourceError::Unsupported(uri.to_string()))
}

fn source_from_local_uri(uri: &str) -> Result<Box<dyn MediaSource>> {
    if uri.starts_with("fd://") {
        #[cfg(target_os = "android")]
        {
            // SAFETY: accepting this URI is the ownership-transfer boundary.
            return Ok(Box::new(unsafe {
                OwnedFileDescriptorSource::open_uri(uri)?
            }));
        }
        #[cfg(not(target_os = "android"))]
        {
            return Err(SourceError::Unsupported(uri.to_string()));
        }
    }
    Ok(Box::new(LocalFileSource::open(local_path_from_uri(uri))?))
}

fn local_path_from_uri(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

fn range_contains(container: ByteRange, range: ByteRange) -> bool {
    let (Some(container_length), Some(range_length)) = (container.length, range.length) else {
        return false;
    };
    let Some(container_end) = container.start.checked_add(container_length) else {
        return false;
    };
    let Some(range_end) = range.start.checked_add(range_length) else {
        return false;
    };
    range.start >= container.start && range_end <= container_end
}

fn http_read_ahead_bytes() -> u64 {
    env::var("ERIKA_HTTP_READAHEAD_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES)
}

fn http_trace_log(line: impl AsRef<str>) {
    if !trace::env_flag("ERIKA_HTTP_TRACE") {
        return;
    }
    let path = env::var_os("ERIKA_HTTP_TRACE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/erika_http_trace.jsonl"));
    trace::append_line(line.as_ref(), path);
}

fn redacted_uri(uri: &str) -> String {
    let mut value = uri.to_string();
    for key in ["api_key=", "AccessToken="] {
        let mut search_from = 0;
        while let Some(relative) = value[search_from..].find(key) {
            let start = search_from + relative + key.len();
            let end = value[start..]
                .find('&')
                .map(|relative_end| start + relative_end)
                .unwrap_or(value.len());
            value.replace_range(start..end, "REDACTED");
            search_from = start + "REDACTED".len();
        }
    }
    value
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn http_retry_gate_fails_fast_when_no_bytes_arrive() {
        let mut gate = HttpRetryGate::new();
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        // A fetch that never moved must fail fast: read_range blocks the
        // demuxer thread.
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_none(), "three stalls must stop the fetch");
    }

    #[test]
    fn http_retry_gate_keeps_resuming_while_bytes_arrive() {
        let mut gate = HttpRetryGate::new();
        let mut allowed = 0;
        for received in 1..=u64::from(HTTP_FETCH_MAX_RESUME_ATTEMPTS) {
            let _ = gate.begin_attempt();
            match gate.fail(received) {
                Some(_) => allowed += 1,
                None => break,
            }
        }
        // The whole resume allowance is available once every attempt moves the
        // resume point: a capped 4 MiB request on a slow origin legitimately
        // spans several 15 s attempts, and stopping after three would end
        // playback instead of merely slowing it down.
        assert_eq!(allowed, HTTP_FETCH_MAX_RESUME_ATTEMPTS - 1);
    }

    #[test]
    fn http_retry_gate_forgets_stalls_once_bytes_arrive() {
        let mut gate = HttpRetryGate::new();
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(0).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(64).is_some(), "progress resets the stall count");
        let _ = gate.begin_attempt();
        assert!(gate.fail(64).is_some());
        let _ = gate.begin_attempt();
        assert!(gate.fail(64).is_some());
        let _ = gate.begin_attempt();
        assert!(
            gate.fail(64).is_none(),
            "progress does not license endless stalls"
        );
    }

    #[test]
    fn http_retry_gate_respects_the_wall_clock_ceiling() {
        let mut gate = HttpRetryGate::new();
        gate.started = Instant::now() - HTTP_FETCH_TOTAL_BUDGET;
        let _ = gate.begin_attempt();
        assert!(
            gate.fail(1).is_none(),
            "the ceiling stops even a progressing fetch"
        );
    }

    struct MockResponse {
        delay: Duration,
        raw: Vec<u8>,
    }

    impl MockResponse {
        fn immediate(raw: Vec<u8>) -> Self {
            Self {
                delay: Duration::ZERO,
                raw,
            }
        }

        fn delayed(delay: Duration, raw: Vec<u8>) -> Self {
            Self { delay, raw }
        }
    }

    /// Serves each response over one connection (in order) and reports every
    /// received request head through the returned channel.
    fn spawn_mock_http_server(responses: Vec<MockResponse>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = sender.send(head);
                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }
                let _ = stream.write_all(&response.raw);
                let _ = stream.flush();
            }
        });
        (uri, receiver)
    }

    fn http_206_response(start: u64, total: u64, body: &[u8]) -> Vec<u8> {
        let end = start + body.len() as u64 - 1;
        let mut raw = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    /// A 206 head that promises `declared_length` bytes but sends fewer before
    /// the connection closes, producing a body-phase transport error.
    fn http_206_truncated_response(
        start: u64,
        total: u64,
        declared_length: usize,
        body: &[u8],
    ) -> Vec<u8> {
        let end = start + declared_length as u64 - 1;
        let mut raw = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n",
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn http_simple_response(status_line: &str, body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn recv_request_head(requests: &mpsc::Receiver<String>) -> String {
        requests
            .recv_timeout(Duration::from_secs(5))
            .expect("mock server should have received a request")
            .to_lowercase()
    }

    /// Serves up to `connections` Range requests, trickling each body at
    /// `bytes_per_sec` so the client spends real time inside the body phase --
    /// the way a slow origin does, and the only way to exercise the client's own
    /// body deadline. `piece_limit` caps each answer the way a CDN that ignores
    /// the requested length does; `bytes_per_sec == 0` means no throttling.
    fn spawn_drip_http_server(
        total: u64,
        bytes_per_sec: u64,
        piece_limit: Option<u64>,
        connections: usize,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = sender.send(head.clone());
                let (start, mut end) = parse_range_head(&head, total);
                if let Some(limit) = piece_limit {
                    end = end.min(start.saturating_add(limit).saturating_sub(1));
                }
                let length = end.saturating_sub(start) + 1;
                let response_head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(response_head.as_bytes()).is_err() {
                    continue;
                }
                let slice = 32 * 1024u64;
                let mut sent = 0u64;
                while sent < length {
                    let count = slice.min(length - sent) as usize;
                    if stream.write_all(&vec![b'x'; count]).is_err() {
                        break;
                    }
                    sent += count as u64;
                    if bytes_per_sec > 0 {
                        thread::sleep(Duration::from_secs_f64(count as f64 / bytes_per_sec as f64));
                    }
                }
                let _ = stream.flush();
            }
        });
        (uri, receiver)
    }

    /// `Range: bytes=start-end` from a raw request head, clamped to the file.
    fn parse_range_head(head: &str, total: u64) -> (u64, u64) {
        let value = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("range:"))
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_default();
        let spec = value.strip_prefix("bytes=").unwrap_or_default();
        let mut parts = spec.split('-');
        let start = parts
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        let end = parts
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or_else(|_| total.saturating_sub(1));
        (start, end.min(total.saturating_sub(1)))
    }

    #[test]
    fn local_file_source_reads_ranges() {
        let path = std::env::temp_dir().join(format!("erika-source-{}.bin", std::process::id()));
        {
            let mut file = File::create(&path).unwrap();
            file.write_all(b"abcdef").unwrap();
        }

        let mut source = LocalFileSource::open(&path).unwrap();
        assert_eq!(source.len().unwrap(), Some(6));
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 2,
                    length: Some(3)
                })
                .unwrap(),
            b"cde"
        );
        assert_eq!(
            read_uri_to_end(&format!("file://{}", path.display())).unwrap(),
            b"abcdef"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn source_from_uri_rejects_unknown_scheme() {
        match source_from_uri("smb://example/video.mkv") {
            Ok(_) => panic!("unexpectedly accepted unsupported source"),
            Err(error) => assert!(matches!(error, SourceError::Unsupported(_))),
        }
    }

    #[test]
    fn source_hint_controls_selection() {
        let source =
            source_from_uri_with_hint("https://example.invalid/video.mp4", MediaSourceHint::Http)
                .unwrap();
        assert_eq!(source.uri(), "https://example.invalid/video.mp4");

        assert!(matches!(
            source_from_uri_with_hint("file:///tmp/video.mp4", MediaSourceHint::Http),
            Err(SourceError::Unsupported(_))
        ));
    }

    #[test]
    fn owned_fd_uri_parses_asset_slice() {
        assert_eq!(
            parse_fd_uri("fd://42?offset=4096&length=8192").unwrap(),
            OwnedFdUri {
                fd: 42,
                offset: 4096,
                length: Some(8192),
            }
        );
        assert_eq!(
            parse_fd_uri("fd://7?length=-1").unwrap(),
            OwnedFdUri {
                fd: 7,
                offset: 0,
                length: None,
            }
        );
    }

    #[test]
    fn owned_fd_uri_rejects_invalid_or_ambiguous_values() {
        for uri in [
            "fd://-1",
            "fd://not-a-number",
            "fd://3?offset=x",
            "fd://3?offset=1&offset=2",
            "fd://3?unknown=1",
        ] {
            assert!(matches!(
                parse_fd_uri(uri),
                Err(SourceError::InvalidFileDescriptorUri(_))
            ));
        }
    }

    #[cfg(target_os = "android")]
    #[test]
    fn unregistered_owned_fd_uri_cannot_adopt_a_numeric_descriptor() {
        let error = unsafe { OwnedFileDescriptorSource::open_uri("fd://2147483647") }
            .expect_err("an unregistered descriptor must be rejected");
        assert!(matches!(
            error,
            SourceError::InvalidFileDescriptorUri(message)
                if message.contains("not explicitly transferred")
        ));
    }

    #[test]
    fn http_default_read_ahead_is_streaming_sized() {
        assert_eq!(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES, 2 * 1024 * 1024);
    }

    #[test]
    fn http_source_constructor_preserves_custom_headers() {
        let source = HttpRangeSource::with_http_headers(
            "https://example.invalid/video.mp4",
            vec![
                ("Authorization".to_string(), "Bearer test".to_string()),
                ("X-Playback-Session".to_string(), "session-123".to_string()),
            ],
        );
        assert_eq!(
            source.http_headers,
            vec![
                ("Authorization".to_string(), "Bearer test".to_string()),
                ("X-Playback-Session".to_string(), "session-123".to_string()),
            ]
        );
    }

    #[test]
    fn http_source_constructor_preserves_explicit_read_ahead() {
        let source = HttpRangeSource::with_http_headers_and_read_ahead(
            "https://example.invalid/video.mp4",
            Vec::new(),
            Some(16 * 1024 * 1024),
        );

        assert_eq!(source.read_ahead_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn http_source_new_starts_without_custom_headers() {
        let source = HttpRangeSource::new("https://example.invalid/video.mp4");

        assert!(source.http_headers.is_empty());
    }

    #[test]
    fn http_source_preserves_headers_without_normalizing_values() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer a+b/c==".to_string()),
            (
                "X-Client-Tag".to_string(),
                "  preserve whitespace  ".to_string(),
            ),
        ];
        let source = HttpRangeSource::with_http_headers(
            "https://example.invalid/video.mp4",
            headers.clone(),
        );

        assert_eq!(source.http_headers, headers);
    }

    #[test]
    fn range_contains_accepts_inner_byte_ranges() {
        assert!(range_contains(
            ByteRange {
                start: 100,
                length: Some(200),
            },
            ByteRange {
                start: 128,
                length: Some(64),
            },
        ));
        assert!(!range_contains(
            ByteRange {
                start: 100,
                length: Some(200),
            },
            ByteRange {
                start: 280,
                length: Some(64),
            },
        ));
    }

    #[test]
    fn content_range_total_parses_totals_and_rejects_unknown() {
        assert_eq!(parse_content_range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 100-199/200"), Some(200));
        assert_eq!(parse_content_range_total("bytes */555"), Some(555));
        assert_eq!(parse_content_range_total("bytes 0-99/*"), None);
        assert_eq!(parse_content_range_total("items 0-99/1234"), None);
        assert_eq!(parse_content_range_total(""), None);
    }

    #[test]
    fn length_probe_keeps_non_range_http_statuses_as_errors() {
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )]);
        let error = probe_http_total_length(&http_agent(), &uri, &[]).unwrap_err();
        assert!(error.to_string().contains("404"));
    }

    #[test]
    fn http_range_rejects_status_200_for_nonzero_offset() {
        let body = vec![b'a'; 100];
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_simple_response("200 OK", &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(100);
        let error = source
            .read_range(ByteRange {
                start: 10,
                length: Some(10),
            })
            .expect_err("a 200 answer to a mid-file Range request must fail");
        assert!(matches!(
            error,
            SourceError::Http(message) if message.contains("ignored Range")
        ));
        assert!(recv_request_head(&requests).contains("range: bytes=10-99"));
    }

    #[test]
    fn http_range_accepts_status_200_for_whole_file_and_backfills_total() {
        let body = b"whole-file-payload".to_vec();
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_simple_response("200 OK", &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(
            source.read_range(ByteRange::suffix_from(0)).unwrap(),
            body.clone()
        );
        // Content-Length of the 200 response backfills the total without HEAD.
        assert_eq!(source.len().unwrap(), Some(body.len() as u64));
    }

    #[test]
    fn http_range_backfills_total_from_206_content_range() {
        let body = vec![b'x'; 16];
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, 4096, &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.read_range(ByteRange::suffix_from(0)).unwrap(), body);
        // The 206 Content-Range total satisfies len() without a HEAD request.
        assert_eq!(source.len().unwrap(), Some(4096));
    }

    #[test]
    fn http_range_retries_after_server_error() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_simple_response("500 Internal Server Error", b"boom")),
            MockResponse::immediate(http_206_response(0, 64, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
    }

    #[test]
    fn http_range_resumes_truncated_body_from_received_offset() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            // Promises 64 bytes but closes after 32: a body-phase error.
            MockResponse::immediate(http_206_truncated_response(0, 64, 64, &body[..32])),
            MockResponse::immediate(http_206_response(32, 64, &body[32..])),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
        // The resumed request must start where the truncated body stopped.
        assert!(recv_request_head(&requests).contains("range: bytes=32-63"));
    }

    #[test]
    fn take_prefetch_joins_inflight_thread_instead_of_refetching() {
        let body: Vec<u8> = (0..100u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::delayed(
            Duration::from_millis(250),
            http_206_response(0, 100, &body),
        )]);
        let mut source = HttpRangeSource::new(uri.clone());
        source.content_length = Some(100);
        let range = ByteRange {
            start: 0,
            length: Some(100),
        };
        source.prefetch = Some(PendingHttpFetch::spawn(uri, Vec::new(), range));
        assert_eq!(source.read_range(range).unwrap(), body);
        let _ = recv_request_head(&requests);
        // Joining the pending prefetch must not issue a duplicate download.
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn short_prefetch_read_falls_back_to_synchronous_fetch() {
        let body: Vec<u8> = (0..100u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            // Prefetch answer is complete HTTP but shorter than the request.
            MockResponse::immediate(http_206_response(0, 100, &body[..50])),
            MockResponse::immediate(http_206_response(50, 100, &body[50..])),
        ]);
        let mut source = HttpRangeSource::new(uri.clone());
        source.content_length = Some(100);
        let range = ByteRange {
            start: 0,
            length: Some(100),
        };
        source.prefetch = Some(PendingHttpFetch::spawn(uri, Vec::new(), range));
        // A short prefetch must trigger the synchronous follow-up, not an
        // empty (fake-EOF) read. The piece already held is kept, so the
        // follow-up only asks for the gap.
        assert_eq!(source.read_range(range).unwrap(), body);
        let _ = recv_request_head(&requests);
        assert!(recv_request_head(&requests).contains("range: bytes=50-99"));
    }

    #[test]
    fn failing_prefetch_degrades_to_a_synchronous_fetch() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            // 4xx is not retryable, so the prefetch fails on its first answer.
            MockResponse::immediate(http_simple_response("404 Not Found", b"gone")),
            MockResponse::immediate(http_206_response(0, 64, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri.clone());
        source.content_length = Some(64);
        let range = ByteRange {
            start: 0,
            length: Some(64),
        };
        source.prefetch = Some(PendingHttpFetch::spawn(uri, Vec::new(), range));

        // A dead background prefetch must not end the read: the synchronous path
        // fetches the same bytes.
        assert_eq!(source.read_range(range).unwrap(), body);
        let first = recv_request_head(&requests);
        assert!(first.contains("range: bytes=0-63"), "first: {first}");
        let second = recv_request_head(&requests);
        assert!(second.contains("range: bytes=0-63"), "second: {second}");
        assert_eq!(
            source.prefetch_failures, 0,
            "a successful synchronous fetch clears the failure latch"
        );
    }

    #[test]
    fn http_read_straddling_the_cache_end_joins_the_inflight_piece() {
        let total = 1024 * 1024 + 4096;
        let piece = vec![b'p'; 4096];
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::delayed(
                Duration::from_millis(200),
                http_206_response(1024 * 1024, total, &piece),
            ),
            // Only a duplicate download would ever need this one.
            MockResponse::immediate(http_206_response(1024 * 1024, total, &piece)),
        ]);
        let mut source = HttpRangeSource::new(uri.clone());
        source.content_length = Some(total);
        // 1 MiB already buffered, and the next piece in flight.
        source.cache_start = 0;
        source.cache_bytes = vec![b'c'; 1024 * 1024];
        source.prefetch = Some(PendingHttpFetch::spawn(
            uri,
            Vec::new(),
            ByteRange {
                start: 1024 * 1024,
                length: Some(4096),
            },
        ));

        // The read starts inside the cache and ends past its end, so it needs
        // the piece that is already on the wire: joining it is the only way not
        // to download the same range twice.
        let bytes = source
            .read_range(ByteRange {
                start: 1024 * 1024 - 1024,
                length: Some(2048),
            })
            .unwrap();
        assert_eq!(bytes.len(), 2048);
        assert!(bytes[..1024].iter().all(|byte| *byte == b'c'));
        assert!(bytes[1024..].iter().all(|byte| *byte == b'p'));
        let _ = recv_request_head(&requests);
        assert!(
            requests.recv_timeout(Duration::from_millis(200)).is_err(),
            "a read that straddles the cache end must not duplicate the in-flight request"
        );
    }

    #[test]
    fn http_stream_to_eof_reads_past_eight_pieces() {
        // An open-ended read (danmaku/subtitle sidecar) larger than one read's
        // fetch allowance must still reach EOF. The first rewrite of this path
        // capped the loop at 8 x 4 MiB = 32 MiB and truncated silently.
        let chunks = 9u64;
        let total = chunks * HTTP_REQUEST_MAX_BYTES;
        let responses = (0..chunks)
            .map(|index| {
                MockResponse::immediate(http_206_response(
                    index * HTTP_REQUEST_MAX_BYTES,
                    total,
                    &vec![b'x'; HTTP_REQUEST_MAX_BYTES as usize],
                ))
            })
            .collect();
        let (uri, requests) = spawn_mock_http_server(responses);
        let mut source = HttpRangeSource::new(uri);

        let bytes = source.read_range(ByteRange::suffix_from(0)).unwrap();
        assert_eq!(
            bytes.len() as u64,
            total,
            "an open-ended read must reach EOF, not stop at the attempt cap"
        );
        for index in 0..chunks {
            let head = recv_request_head(&requests);
            assert!(
                head.contains(&format!("range: bytes={}-", index * HTTP_REQUEST_MAX_BYTES)),
                "request {index}: {head}"
            );
        }
    }

    #[test]
    fn http_short_pieces_still_cover_the_read() {
        // An origin that caps every body at 256 KiB needs many pieces to cover a
        // gap. Walking away would hand the caller an empty read, which the AVIO
        // layer turns into EOF: bytes that exist must never look like the end of
        // the resource.
        let total = 8 * 1024 * 1024u64;
        let piece = 256 * 1024u64;
        // A range-aware origin that caps every answer at 256 KiB.
        let (uri, _requests) = spawn_drip_http_server(total, 0, Some(piece), 40);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);

        // Warm the cache with the first short piece, then read just inside the
        // 4 MiB forward-gap guard: covering it takes ~15 more pieces.
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        let target = HTTP_REQUEST_MAX_BYTES;
        let bytes = source
            .read_range(ByteRange {
                start: target,
                length: Some(1024),
            })
            .expect("short pieces must still cover the read");
        assert_eq!(bytes.len(), 1024, "a covered read must not look like EOF");
    }

    #[test]
    fn http_caller_requests_are_capped_too() {
        let total = 64 * 1024 * 1024u64;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, total, &vec![b'a'; 4096]),
        )]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);

        // No cache yet, so the window is anchored at the read -- and a 32 MiB
        // caller request must not become a 32 MiB body (that is the issue #1
        // shape). The caller sees a short read and asks again.
        let bytes = source
            .read_range(ByteRange {
                start: 0,
                length: Some(32 * 1024 * 1024),
            })
            .unwrap();
        assert_eq!(bytes.len(), 4096);
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=0-4194303"),
            "request head: {head}"
        );
    }

    #[test]
    fn http_trim_never_drops_the_current_read_position() {
        let mut source = HttpRangeSource::new("https://example.invalid/video.mp4");
        source.cache_start = 0;
        source.cache_bytes = vec![b'c'; 25 * 1024 * 1024];

        // A read starting at 0 may not have its own start trimmed away, however
        // large it is.
        source.trim_cache(ByteRange {
            start: 0,
            length: Some(20 * 1024 * 1024),
        });
        assert_eq!(source.cache_start, 0);

        // A read near the cache end trims the head down to the retention budget.
        source.trim_cache(ByteRange {
            start: 25 * 1024 * 1024,
            length: Some(1024),
        });
        assert_eq!(
            source.cache_start,
            25 * 1024 * 1024 - HTTP_CACHE_RETAIN_BYTES
        );
    }

    #[test]
    fn http_prefetch_refills_towards_the_configured_depth() {
        // 20 MiB ahead with a 32 MiB window is still below the configured depth,
        // so the chain must start another piece; the old half-window threshold
        // would have settled there and never reached the setting.
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(20 * 1024 * 1024, 64 * 1024 * 1024, &vec![b'z'; 1024]),
        )]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(64 * 1024 * 1024);
        source.cache_start = 0;
        source.cache_bytes = vec![b'c'; 20 * 1024 * 1024];

        source.maybe_start_prefetch(ByteRange {
            start: 0,
            length: Some(1024),
        });
        assert!(
            source.prefetch.is_some(),
            "20 MiB ahead is below a 32 MiB window"
        );
    }

    #[test]
    fn http_deep_window_against_a_slow_origin_still_serves_the_read() {
        // The issue #1 shape: a 32 MiB window against an origin that can only
        // deliver ~900 KB/s. Before the request cap this asked for all 32 MiB in
        // one body, which the client's 15 s body deadline killed
        // (`timeout: receive response` -> EIO -> playback ended). A capped 4 MiB
        // request finishes in about five seconds.
        let total = 64 * 1024 * 1024u64;
        let (uri, requests) = spawn_drip_http_server(total, 900 * 1024, None, 2);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);

        let started = Instant::now();
        let bytes = source
            .read_range(ByteRange {
                start: 0,
                length: Some(1024),
            })
            .expect("a deep window must not turn a slow origin into a failed read");
        assert_eq!(bytes.len(), 1024);
        // The discriminating failure is the `expect` above (the old shape errors
        // out at the 15 s body deadline); this bound only catches a fetch that
        // silently stopped making progress, so it is sized for a loaded machine.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "first read took {:?}",
            started.elapsed()
        );
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=0-4194303"),
            "request head: {head}"
        );
    }

    #[test]
    fn http_resumes_more_than_three_times_while_bytes_arrive() {
        // Four pieces, each on its own connection: three truncated bodies, then
        // the rest. Every attempt moves the resume point, so the fetch has to
        // keep going -- the flat three-attempt rule used to end playback here
        // (a capped request on a slow origin legitimately spans several
        // attempts, each cut short by the 15 s body deadline).
        let total = 4 * 1024 * 1024u64;
        let piece = 1024 * 1024usize;
        let held = vec![b'a'; piece];
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_truncated_response(
                0,
                total,
                4 * 1024 * 1024,
                &held,
            )),
            MockResponse::immediate(http_206_truncated_response(
                1024 * 1024,
                total,
                3 * 1024 * 1024,
                &held,
            )),
            MockResponse::immediate(http_206_truncated_response(
                2 * 1024 * 1024,
                total,
                2 * 1024 * 1024,
                &held,
            )),
            MockResponse::immediate(http_206_response(3 * 1024 * 1024, total, &held)),
        ]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(8 * 1024 * 1024),
        );
        source.content_length = Some(total);

        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        let first = recv_request_head(&requests);
        assert!(first.contains("range: bytes=0-4194303"), "{first}");
        let second = recv_request_head(&requests);
        assert!(second.contains("range: bytes=1048576-4194303"), "{second}");
        let third = recv_request_head(&requests);
        assert!(third.contains("range: bytes=2097152-4194303"), "{third}");
        // A fourth attempt: the old gate stopped at three, no matter how much
        // each attempt had already delivered.
        let fourth = recv_request_head(&requests);
        assert!(fourth.contains("range: bytes=3145728-4194303"), "{fourth}");
    }

    #[test]
    fn http_request_body_is_capped_regardless_of_the_read_ahead_window() {
        let total = 64 * 1024 * 1024u64;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, total, &vec![b'a'; 4096]),
        )]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(32 * 1024 * 1024),
        );
        source.content_length = Some(total);
        assert_eq!(source.read_ahead_bytes, 32 * 1024 * 1024);
        assert_eq!(source.request_bytes, HTTP_REQUEST_MAX_BYTES);

        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        // A 32 MiB window used to go out as one 32 MiB request (~18 Mbps at the
        // 15 s body deadline, i.e. an issue #1 failure on anything slower). It
        // must now be a capped request.
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=0-4194303"),
            "request head: {head}"
        );
    }

    #[test]
    fn http_prefetch_chain_refills_the_window_in_capped_pieces() {
        let total = 64 * 1024 * 1024u64;
        let piece = vec![b'p'; HTTP_REQUEST_MAX_BYTES as usize];
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(0, total, &piece)),
            MockResponse::immediate(http_206_response(HTTP_REQUEST_MAX_BYTES, total, &piece)),
        ]);
        let mut source = HttpRangeSource::with_http_headers_and_read_ahead(
            uri,
            Vec::new(),
            Some(8 * 1024 * 1024),
        );
        source.content_length = Some(total);

        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        let first = recv_request_head(&requests);
        assert!(first.contains("range: bytes=0-4194303"), "first: {first}");
        // The refill is the *next capped piece*, anchored at the cache end --
        // not another request for the whole window.
        let refill = recv_request_head(&requests);
        assert!(
            refill.contains("range: bytes=4194304-8388607"),
            "refill: {refill}"
        );

        // Once the piece lands it is folded into the cache (so the following
        // piece has somewhere to start) instead of waiting for the reader.
        // The wait is deliberately generous: a loaded machine can stretch the
        // loopback round trip well past its idle latency.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline
            && !source
                .prefetch
                .as_ref()
                .is_some_and(PendingHttpFetch::is_finished)
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 1024,
                    length: Some(64)
                })
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            source.cache_bytes.len() as u64,
            2 * HTTP_REQUEST_MAX_BYTES,
            "the finished piece must have been absorbed into the cache"
        );
    }

    #[test]
    fn http_rewind_inside_the_retained_tail_is_served_from_cache() {
        let total = 40 * 1024 * 1024u64;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, total, b"unused"),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        // Pretend a 40 MiB window has been buffered from byte zero.
        source.cache_start = 0;
        source.cache_bytes = (0..total).map(|offset| (offset % 251) as u8).collect();

        // Reading near the tail trims the head down to the retention budget...
        let tail_start = total - 1024;
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: tail_start,
                    length: Some(1024)
                })
                .unwrap()
                .len(),
            1024
        );
        assert_eq!(
            source.cache_start,
            (tail_start) - HTTP_CACHE_RETAIN_BYTES,
            "the head must be trimmed to the retention budget, measured from the read"
        );

        // ...and a small rewind lands inside what is left: served locally.
        let rewind_start = source.cache_end() - 4 * 1024 * 1024;
        let bytes = source
            .read_range(ByteRange {
                start: rewind_start,
                length: Some(64),
            })
            .unwrap();
        assert_eq!(bytes.len(), 64);
        assert_eq!(bytes[0], (rewind_start % 251) as u8);
        assert!(
            requests.recv_timeout(Duration::from_millis(200)).is_err(),
            "a rewind inside the retained tail must not hit the network"
        );
    }

    #[test]
    fn http_rewind_past_the_retained_tail_reanchors_the_window() {
        let target = 2 * 1024 * 1024u64;
        let total = target + 128;
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(target, total, &[b'r'; 128]),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(total);
        // The cache only holds the tail; everything before it was trimmed.
        source.cache_start = 24 * 1024 * 1024;
        source.cache_bytes = vec![b'c'; 1024];

        // Far behind the retained tail: re-anchor at the read position, the
        // same thing every player does once a seek lands past its back buffer.
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: target,
                    length: Some(64)
                })
                .unwrap()
                .len(),
            64
        );
        let head = recv_request_head(&requests);
        assert!(
            head.contains(&format!("range: bytes={target}-")),
            "request head: {head}"
        );
        assert_eq!(source.cache_start, target);
    }

    #[test]
    fn len_retries_head_before_succeeding() {
        let head_response = b"HTTP/1.1 200 OK\r\nContent-Length: 4321\r\nConnection: close\r\n\r\n";
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_simple_response("500 Internal Server Error", b"")),
            MockResponse::immediate(head_response.to_vec()),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(4321));
        assert!(recv_request_head(&requests).starts_with("head"));
        assert!(recv_request_head(&requests).starts_with("head"));
    }

    #[test]
    fn len_probe_does_not_download_a_body_that_ignores_range() {
        // HEAD is rejected and the origin ignores Range, answering 200 with the
        // whole object. The probe must take the total from Content-Length and
        // leave the payload on the wire instead of buffering the media.
        let payload = vec![b'x'; 512 * 1024];
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        raw.extend_from_slice(&payload);
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            MockResponse::immediate(raw),
        ]);

        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(payload.len() as u64));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"), "probe head: {probe}");
    }

    #[test]
    fn resumed_range_is_bound_to_the_first_response_entity() {
        let body: Vec<u8> = (0..64u8).collect();
        // Same truncated-then-resume shape as the resume test above, but the
        // first response carries a validator the retry has to replay.
        let mut truncated = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-63/64\r\nContent-Length: 64\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        truncated.extend_from_slice(&body[..32]);
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(truncated),
            MockResponse::immediate(http_206_response(32, 64, &body[32..])),
        ]);

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );

        let first = recv_request_head(&requests);
        assert!(!first.contains("if-range"), "first request: {first}");
        let resumed = recv_request_head(&requests);
        assert!(
            resumed.contains("if-range: \"v1\""),
            "resumed request must replay the validator: {resumed}"
        );
    }

    #[test]
    fn resumed_range_rejects_a_response_served_from_a_different_offset() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, _requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_truncated_response(0, 64, 64, &body[..32])),
            // The resume asked for byte 32; this answers from 0 instead, which
            // would splice mismatched bytes onto the prefix already held.
            MockResponse::immediate(http_206_response(0, 64, &body[..32])),
        ]);

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        let error = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64),
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("served range from 0"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn len_falls_back_to_range_probe_when_head_is_rejected() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            MockResponse::immediate(http_206_response(0, 1234, b"z")),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(1234));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn len_falls_back_to_range_probe_when_head_reports_zero() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            MockResponse::immediate(http_206_response(0, 911_198_509, b"z")),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(911_198_509));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn len_preserves_zero_when_range_probe_confirms_empty_resource() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            MockResponse::immediate(
                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(0));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn redacted_uri_hides_access_tokens() {
        assert_eq!(
            redacted_uri("https://example.invalid/video.mkv?api_key=secret&x=1"),
            "https://example.invalid/video.mkv?api_key=REDACTED&x=1"
        );
        assert_eq!(
            redacted_uri("https://example.invalid/video.mkv?AccessToken=secret"),
            "https://example.invalid/video.mkv?AccessToken=REDACTED"
        );
    }
}
