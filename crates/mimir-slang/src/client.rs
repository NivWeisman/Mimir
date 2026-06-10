//! Async client for the slang sidecar.
//!
//! Two layers, intentionally separable so the framing layer can be tested
//! without spawning a process:
//!
//! * [`Connection<R, W>`] — does NDJSON framing over any
//!   `(AsyncBufRead, AsyncWrite)` pair. Tests use [`tokio::io::duplex`] to
//!   exercise this in-process.
//! * [`Client`] — owns the [`tokio::process::Child`] for the sidecar binary
//!   and a [`Connection`] over its stdio. This is what `mimir-server` will
//!   actually hold onto.
//!
//! ## Concurrency
//!
//! The wire protocol allows out-of-order responses (each reply echoes the
//! request `id`), but the current implementation is **single-flight**: the
//! [`Connection`] holds `&mut self` across each `request → wait for
//! response` cycle, so callers serialise on the wrapping `Mutex` inside
//! [`Client`]. If we later need pipelining, the wire format already supports
//! it — only the client-side dispatcher needs replacing.
//!
//! To avoid blocking interactive editor requests behind a slow background
//! elaborate, **interactive methods** (`definition`, `complete`, etc.) use
//! `Mutex::try_lock` and return [`ClientError::Busy`] immediately when the
//! connection is occupied. The server falls through to its tree-sitter
//! fallback in that case. [`Client::compile`] uses the full `lock().await`
//! because it runs on a background task where waiting is acceptable.
//!
//! ## Failure model
//!
//! * Process won't spawn → [`ClientError::Spawn`].
//! * Sidecar's stdio closes mid-conversation → [`ConnectionError::Closed`].
//! * Sidecar replies with an `error` field → [`ConnectionError::Sidecar`].
//! * Sidecar replies but the JSON doesn't decode → [`ConnectionError::Decode`].
//!
//! None of these are recoverable inside the client. The server is expected
//! to log the error, drop the [`Client`] (which kills the child on drop),
//! and either respawn or fall back to tree-sitter-only mode.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, instrument, trace, warn};

/// Hard deadline for one `compile` round-trip. Generous because a real
/// project elaborate can legitimately take tens of seconds — the point is
/// to bound a *hung* sidecar (which used to block the elaborate task
/// forever), not to police slow-but-progressing compiles.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(300);

/// Deadline for one `expandMacro` round-trip. The expand sidecar only runs
/// the preprocessor (normally sub-second), and the callers are interactive
/// (hover footer, explicit expand command), so a short ceiling is right.
const EXPAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for the polite `shutdown` request and the subsequent child
/// wait. Past it we stop being polite and kill the process instead of
/// hanging the server's own shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

use crate::protocol::{
    methods, CompileResult, ElaborateParams, ExpandMacroParams, ExpandMacroResult, Request,
    Response, ResponseError,
};

// --------------------------------------------------------------------------
// Errors
// --------------------------------------------------------------------------

/// Anything that can go wrong on a single request/response exchange over a
/// [`Connection`]. These are framing- or protocol-level — process lifecycle
/// failures live on [`ClientError`].
#[derive(Debug, Error)]
pub enum ConnectionError {
    /// Underlying reader/writer returned an I/O error. Most often "broken
    /// pipe" because the sidecar exited.
    #[error("I/O error talking to sidecar: {0}")]
    Io(#[from] std::io::Error),

    /// The sidecar closed its stdout cleanly without sending the response
    /// we were waiting for.
    #[error("sidecar closed its stdout before responding")]
    Closed,

    /// Got bytes back, but they didn't decode as a [`Response`] (or the
    /// `result` payload didn't decode as the expected type).
    #[error("could not decode sidecar response: {0}")]
    Decode(#[from] serde_json::Error),

    /// Sidecar processed our request and returned an `error` payload.
    #[error("sidecar returned error {}: {}", .0.code, .0.message)]
    Sidecar(ResponseError),

    /// Sidecar's response had an `id` strictly greater than the request we
    /// just sent. This is a genuine protocol violation — the sidecar skipped
    /// ahead in the id space, which should never happen with a single-flight
    /// sequenced sender.
    ///
    /// Note: responses with `id < expected` are *stale* (from a previously
    /// cancelled task) and are silently drained by [`Connection::request`],
    /// so they never surface as this error.
    #[error("response id mismatch: expected {expected}, got {got}")]
    IdMismatch {
        /// The id we put on the outgoing request.
        expected: u64,
        /// The id the sidecar echoed.
        got: u64,
    },

    /// The sidecar did not respond within the request's deadline
    /// (`COMPILE_TIMEOUT` / `EXPAND_TIMEOUT` / `SHUTDOWN_TIMEOUT`).
    /// Produced by [`Connection::request_with_deadline`]. The request bytes
    /// are already out, so a late response (or a partial line from the
    /// cancelled read) may still arrive — the next caller's
    /// resync-and-drain step in [`Connection::request`] disposes of it, so
    /// the connection stays usable.
    #[error("sidecar request timed out after {secs}s")]
    Timeout {
        /// Wall-clock seconds we waited before giving up.
        secs: u64,
    },

    /// Response had neither `result` nor `error`. Wire-protocol violation.
    #[error("response had neither `result` nor `error` (id={id})")]
    EmptyResponse {
        /// The id the bad response carried.
        id: u64,
    },
}

/// Anything that can go wrong constructing or running a [`Client`].
///
/// `Spawn` and `Connection` are kept separate because the recovery is
/// different — a spawn failure is usually misconfiguration (wrong path),
/// a connection failure is usually a sidecar crash.
#[derive(Debug, Error)]
pub enum ClientError {
    /// `tokio::process::Command::spawn` failed. Almost always "no such
    /// file or directory" — the configured sidecar path doesn't exist.
    #[error("could not spawn sidecar `{program}`: {source}")]
    Spawn {
        /// The path we tried to launch.
        program: String,
        /// The OS-level error.
        #[source]
        source: std::io::Error,
    },

    /// The child spawned but didn't expose the stdio handles we asked for.
    /// This shouldn't happen given we set [`Stdio::piped`] on all three —
    /// modelled as an error so we never silently `unwrap` and crash the
    /// LSP server.
    #[error("sidecar child is missing the {which} handle")]
    MissingStdio {
        /// Which of `stdin` / `stdout` / `stderr` was missing.
        which: &'static str,
    },

    /// A request through the underlying [`Connection`] failed.
    #[error(transparent)]
    Connection(#[from] ConnectionError),

    /// The sidecar connection is occupied by another in-flight request
    /// (typically a background elaborate). The caller should skip the slang
    /// path and fall through to its tree-sitter fallback immediately rather
    /// than queuing behind a potentially slow elaborate round-trip.
    #[error("sidecar connection is busy with another in-flight request")]
    Busy,

    /// No sidecar client exists to send the request to. Produced by
    /// embedders that hold an *optional* client (the server keeps
    /// `Option<Arc<Client>>` behind a lock) when a request races a
    /// configuration change that removed the sidecar — a recoverable
    /// "feature unavailable right now", never a panic.
    #[error("no slang sidecar is configured")]
    NotConfigured,
}

// --------------------------------------------------------------------------
// Connection — framing over any AsyncBufRead + AsyncWrite
// --------------------------------------------------------------------------

/// NDJSON framing over a generic reader/writer pair.
///
/// The reader must already be buffered (we use `read_line`), which is why
/// [`Client::spawn`] wraps the child's stdout in a [`BufReader`] before
/// constructing the [`Connection`].
pub struct Connection<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    reader: R,
    writer: W,
    /// Monotonically incremented per request. `u64` is plenty — at one
    /// request per microsecond it'd take 580,000 years to wrap.
    next_id: u64,
    /// Reusable line buffer. Kept as `Vec<u8>` + [`AsyncBufReadExt::read_until`]
    /// rather than `String` + `read_line` deliberately: `read_until` appends
    /// through this `&mut` borrow, so a read cancelled by a deadline leaves
    /// the partial line *here*, where the resync step at the top of
    /// [`Self::request`] can complete and discard it. `read_line` moves the
    /// buffer into its future and loses the partial bytes on cancellation,
    /// which would corrupt the framing undetectably.
    line_buf: Vec<u8>,
}

impl<R, W> Connection<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Construct a connection over the given streams. Doesn't perform any
    /// I/O — the first request will.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
            line_buf: Vec::new(),
        }
    }

    /// Test-only constructor that starts `next_id` at a given value.
    ///
    /// Used by tests that simulate a prior task having been cancelled after
    /// writing its request bytes (advancing `next_id`) but before reading
    /// the response, leaving stale bytes in the reader.
    #[cfg(test)]
    fn with_next_id(reader: R, writer: W, next_id: u64) -> Self {
        Self {
            reader,
            writer,
            next_id,
            line_buf: Vec::new(),
        }
    }

    /// Send a request and wait for its matching response.
    ///
    /// `params` is serialised to a `serde_json::Value` so we can build the
    /// outer envelope around it. The response's `result` is decoded into
    /// `Res` on success; an `error` field is surfaced as
    /// [`ConnectionError::Sidecar`].
    #[instrument(level = "debug", skip(self, params), fields(method))]
    pub async fn request<P, Res>(&mut self, method: &str, params: &P) -> Result<Res, ConnectionError>
    where
        P: Serialize,
        Res: DeserializeOwned,
    {
        // A previous request may have been cancelled (deadline elapsed, task
        // aborted) in the middle of `read_until`, leaving a *partial* line
        // in `line_buf` whose tail is still in the reader. Re-synchronise to
        // a line boundary before writing anything new: finish the partial
        // line and discard it. A line that is already complete is the
        // previous (processed or stale) response — discard without touching
        // the reader.
        if !self.line_buf.is_empty() {
            if self.line_buf.last() != Some(&b'\n') {
                self.reader.read_until(b'\n', &mut self.line_buf).await?;
                debug!(
                    bytes = self.line_buf.len(),
                    "resynced framing after a cancelled read",
                );
            }
            self.line_buf.clear();
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let req = {
            mimir_core::time_scope!("slang.ipc.serialize");
            Request {
                id,
                method: method.to_string(),
                params: serde_json::to_value(params)?,
            }
        };

        // Encode + write + newline + flush. Doing the encode and append in
        // one allocation avoids a second syscall just for the `\n`.
        let line = {
            mimir_core::time_scope!("slang.ipc.encode_envelope");
            let mut l = serde_json::to_string(&req)?;
            l.push('\n');
            l
        };
        debug!(bytes = line.len(), "sending request");
        {
            mimir_core::time_scope!("slang.ipc.write_to_sidecar");
            self.writer.write_all(line.as_bytes()).await?;
            self.writer.flush().await?;
        }

        // Read lines until we find the response for `id`.
        //
        // Responses with `resp.id < id` are *stale*: they were produced by
        // the sidecar for an earlier request whose Tokio task was aborted
        // after the request bytes were written but before the response was
        // read.  When a task is aborted at an `await` point the
        // `MutexGuard` on the `Connection` is dropped, so the next task
        // acquires the lock with `next_id` already advanced but the stale
        // bytes still sitting in the sidecar's stdout buffer.  We drain
        // them here rather than surfacing an `IdMismatch` error.
        // Outer timer captures the full read-and-match phase including any
        // stale responses we drain. Per-line decode + result decode are
        // sub-timers so we can attribute deserialise cost vs. raw wait.
        mimir_core::time_scope!("slang.ipc.read_and_match");
        loop {
            self.line_buf.clear();
            let n = {
                mimir_core::time_scope!("slang.ipc.read_line");
                self.reader.read_until(b'\n', &mut self.line_buf).await?
            };
            if n == 0 {
                return Err(ConnectionError::Closed);
            }
            debug!(bytes = n, "received response");

            let resp: Response = {
                mimir_core::time_scope!("slang.ipc.decode_envelope");
                serde_json::from_slice(&self.line_buf)?
            };
            if resp.id == id {
                return match (resp.result, resp.error) {
                    (Some(value), None) => {
                        mimir_core::time_scope!("slang.ipc.decode_result");
                        Ok(serde_json::from_value(value)?)
                    }
                    (None, Some(err)) => Err(ConnectionError::Sidecar(err)),
                    (None, None) | (Some(_), Some(_)) => {
                        Err(ConnectionError::EmptyResponse { id })
                    }
                };
            }
            if resp.id < id {
                // Stale response from a prior task that was cancelled mid-flight.
                trace!(stale_id = resp.id, expected = id, "discarding stale sidecar response");
                continue;
            }
            // resp.id > id — the sidecar jumped ahead. Genuine protocol error.
            return Err(ConnectionError::IdMismatch {
                expected: id,
                got: resp.id,
            });
        }
    }

    /// Like [`Self::request`], but gives up after `deadline` with
    /// [`ConnectionError::Timeout`].
    ///
    /// The timed-out request's bytes are already written, so the sidecar may
    /// still answer it later — the resync-and-drain logic at the top of
    /// [`Self::request`] disposes of that late response (and of any partial
    /// line the cancelled read left behind), so a single hung round-trip
    /// doesn't poison the connection.
    pub async fn request_with_deadline<P, Res>(
        &mut self,
        method: &str,
        params: &P,
        deadline: Duration,
    ) -> Result<Res, ConnectionError>
    where
        P: Serialize,
        Res: DeserializeOwned,
    {
        match tokio::time::timeout(deadline, self.request(method, params)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                warn!(method, secs = deadline.as_secs(), "sidecar request hit its deadline");
                Err(ConnectionError::Timeout {
                    secs: deadline.as_secs(),
                })
            }
        }
    }

    /// Convenience wrapper for the [`methods::COMPILE`] method.
    pub async fn compile(
        &mut self,
        params: &ElaborateParams,
    ) -> Result<CompileResult, ConnectionError> {
        mimir_core::time_scope!("slang.compile.connection_total");
        self.request(methods::COMPILE, params).await
    }

    /// Convenience wrapper for the [`methods::EXPAND_MACRO`] method.
    pub async fn expand_macro(
        &mut self,
        params: &ExpandMacroParams,
    ) -> Result<ExpandMacroResult, ConnectionError> {
        mimir_core::time_scope!("slang.expand_macro.connection_total");
        self.request(methods::EXPAND_MACRO, params).await
    }
}

// --------------------------------------------------------------------------
// Client — owns the sidecar processes
// --------------------------------------------------------------------------

/// The concrete [`Connection`] type over one sidecar child's piped stdio.
type SidecarConnection = Connection<BufReader<ChildStdout>, ChildStdin>;

/// What [`Client::spawn_one`] hands back: a framed connection, the child
/// process, and the task draining its stderr.
type SpawnedSidecar = (SidecarConnection, Child, JoinHandle<()>);

/// Owns **two** running slang sidecar processes — one for heavy
/// `compile`/elaborate round-trips, one dedicated to on-demand `expandMacro` —
/// each with its own [`Connection`] over its stdio.
///
/// Why two: the wire protocol is single-flight, so a [`Connection`] serialises
/// all its callers behind one in-flight request. A real-project elaborate can
/// hold the compile connection for many seconds (or hang outright), and macro
/// expansion is interactive — a hover footer or the explicit "Expand Macro"
/// command must not queue behind it. Giving expansion its own process and
/// connection decouples the two completely. The expand sidecar only ever runs
/// the preprocessor (never builds the elaborated design), so its steady-state
/// memory stays small.
///
/// Drop semantics: dropping the `Client` drops both [`Child`]ren, which sends
/// SIGKILL on Unix. For a graceful shutdown call [`Client::shutdown`] first,
/// which sends the protocol-level `shutdown` request to each and waits on both.
pub struct Client {
    /// Connection for `compile`/elaborate. Wrapped in a `Mutex` because
    /// [`Connection::request`] needs `&mut` access.
    connection: Mutex<SidecarConnection>,
    /// Dedicated connection (and process) for `expandMacro`, so an interactive
    /// expansion never queues behind — or stalls on — a long/stuck elaborate
    /// holding `connection`.
    expand_connection: Mutex<SidecarConnection>,
    /// The compile sidecar process. Held under a `Mutex` so `shutdown` can
    /// `wait` without competing with anyone else; `drop` doesn't need the lock.
    child: Mutex<Child>,
    /// The expand sidecar process.
    expand_child: Mutex<Child>,
    /// Background tasks draining each sidecar's stderr line-by-line and
    /// re-emitting it through `tracing`. Held to keep the tasks alive for the
    /// lifetime of the client; aborted on drop.
    stderr_pump: Option<JoinHandle<()>>,
    expand_stderr_pump: Option<JoinHandle<()>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(h) = self.stderr_pump.take() {
            h.abort();
        }
        if let Some(h) = self.expand_stderr_pump.take() {
            h.abort();
        }
    }
}

impl Client {
    /// Spawn the sidecar at `program` with the given arguments.
    ///
    /// All three of stdin/stdout/stderr are piped: stdin/stdout for the
    /// NDJSON channel, stderr for the sidecar's log output. Stderr is
    /// drained line-by-line on a background task and each line is
    /// re-emitted through `tracing` under the `mimir_slang_sidecar`
    /// target — both so the lines reach the same destination as the
    /// rest of the server's logs and so the OS pipe buffer never fills
    /// (an undrained pipe buffer eventually blocks `write` on the
    /// sidecar's end and stalls the elaborator).
    #[instrument(level = "debug", skip(args), fields(program = ?program.as_ref()))]
    pub async fn spawn<P, A, S>(program: P, args: A) -> Result<Self, ClientError>
    where
        P: AsRef<Path>,
        A: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let program_ref = program.as_ref();
        // Materialise the args so both sidecars launch with identical command
        // lines (an `IntoIterator` can only be consumed once).
        let args: Vec<std::ffi::OsString> =
            args.into_iter().map(|s| s.as_ref().to_os_string()).collect();

        let (connection, child, stderr_pump) = Self::spawn_one(program_ref, &args)?;
        let (expand_connection, expand_child, expand_stderr_pump) =
            Self::spawn_one(program_ref, &args)?;
        debug!("slang sidecars spawned (compile + expand)");
        Ok(Self {
            connection: Mutex::new(connection),
            expand_connection: Mutex::new(expand_connection),
            child: Mutex::new(child),
            expand_child: Mutex::new(expand_child),
            stderr_pump: Some(stderr_pump),
            expand_stderr_pump: Some(expand_stderr_pump),
        })
    }

    /// Spawn one sidecar process and wire its stdio into a [`Connection`] plus
    /// a background stderr-draining task. Shared by the compile and expand
    /// sidecars so they are launched identically.
    fn spawn_one(program: &Path, args: &[std::ffi::OsString]) -> Result<SpawnedSidecar, ClientError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // We don't want a stray SIGINT from the editor's terminal to
            // tear down the sidecar before the server can shut down
            // cleanly. `kill_on_drop` keeps the cleanup story simple.
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| ClientError::Spawn {
            program: program.display().to_string(),
            source,
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or(ClientError::MissingStdio { which: "stdin" })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ClientError::MissingStdio { which: "stdout" })?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ClientError::MissingStdio { which: "stderr" })?;

        let stderr_pump = tokio::spawn(drain_sidecar_stderr(stderr));
        let connection = Connection::new(BufReader::new(stdout), stdin);
        Ok((connection, child, stderr_pump))
    }

    /// Run a `compile` request against the sidecar.
    ///
    /// Elaborates all project files and serialises the full symbol table as a
    /// [`CompileResult`] containing a `MimirAst` and a flat diagnostics list.
    ///
    /// Bounded by `COMPILE_TIMEOUT` so a hung sidecar surfaces as
    /// [`ConnectionError::Timeout`] instead of blocking the elaborate task
    /// forever.
    pub async fn compile(
        &self,
        params: &ElaborateParams,
    ) -> Result<CompileResult, ClientError> {
        mimir_core::time_scope!("slang.compile.client_total");
        let mut conn = {
            mimir_core::time_scope!("slang.compile.client_lock_wait");
            self.connection.lock().await
        };
        mimir_core::time_scope!("slang.compile.connection_total");
        Ok(conn
            .request_with_deadline(methods::COMPILE, params, COMPILE_TIMEOUT)
            .await?)
    }

    /// Run an `expandMacro` request against the **dedicated expand sidecar**,
    /// blocking on its connection mutex.
    ///
    /// Because expansion has its own process and connection
    /// (`Self::expand_connection`), this no longer serialises behind a
    /// `compile` on the main connection — a long or stuck elaborate can't stall
    /// it. It only waits behind another in-flight expansion (sub-second), so
    /// the explicit "Expand Macro" command can safely block on it. Used by the
    /// command; the hover footer uses [`Client::try_expand_macro`].
    pub async fn expand_macro(
        &self,
        params: &ExpandMacroParams,
    ) -> Result<ExpandMacroResult, ClientError> {
        mimir_core::time_scope!("slang.expand_macro.client_total");
        let mut conn = {
            mimir_core::time_scope!("slang.expand_macro.client_lock_wait");
            self.expand_connection.lock().await
        };
        Ok(conn
            .request_with_deadline(methods::EXPAND_MACRO, params, EXPAND_TIMEOUT)
            .await?)
    }

    /// Non-blocking variant of [`Client::expand_macro`].
    ///
    /// If the dedicated expand connection is momentarily held by another
    /// expansion, returns [`ClientError::Busy`] immediately instead of queuing.
    /// This is what the hover macro-expansion *footer* uses: a hover is
    /// high-frequency and latency-sensitive, so it must never block. When busy,
    /// the caller omits the footer (the cursor's macro still has its `define`
    /// shown by the base hover) and the next idle hover fills it in. With the
    /// dedicated connection this rarely happens — it's no longer contended by
    /// elaborate — but staying non-blocking keeps hovers cheap.
    pub async fn try_expand_macro(
        &self,
        params: &ExpandMacroParams,
    ) -> Result<ExpandMacroResult, ClientError> {
        mimir_core::time_scope!("slang.expand_macro.client_total");
        let mut conn = match self.expand_connection.try_lock() {
            Ok(conn) => conn,
            Err(_) => return Err(ClientError::Busy),
        };
        Ok(conn
            .request_with_deadline(methods::EXPAND_MACRO, params, EXPAND_TIMEOUT)
            .await?)
    }

    /// Send the `shutdown` request to **both** sidecars, then wait for each
    /// child to exit.
    ///
    /// We swallow a "sidecar already gone" error from the request itself
    /// (the sidecar might exit before flushing a response) but propagate
    /// other failures. Caller takes ownership so the type system enforces
    /// "you can't use this client after shutdown."
    ///
    /// Every phase is bounded by `SHUTDOWN_TIMEOUT`: a sidecar that
    /// ignores the polite request is killed rather than allowed to hang the
    /// server's own shutdown.
    pub async fn shutdown(self) -> Result<(), ClientError> {
        // We only care whether the request *got out*; each sidecar is allowed
        // to drop its connection without responding to `shutdown`.
        Self::send_shutdown(&self.connection).await;
        Self::send_shutdown(&self.expand_connection).await;

        Self::wait_or_kill(&self.child, "compile").await?;
        Self::wait_or_kill(&self.expand_child, "expand").await?;
        Ok(())
    }

    /// Send a `shutdown` request over one connection, logging (but not
    /// propagating) the expected "connection closed" outcome.
    async fn send_shutdown(connection: &Mutex<SidecarConnection>) {
        let mut conn = connection.lock().await;
        let res: Result<serde_json::Value, _> = conn
            .request_with_deadline(methods::SHUTDOWN, &serde_json::Value::Null, SHUTDOWN_TIMEOUT)
            .await;
        if let Err(e) = res {
            // `Closed` is the expected outcome. Anything else is worth a warn
            // so we notice misbehaving sidecars.
            match e {
                ConnectionError::Closed | ConnectionError::Io(_) => {
                    debug!(error = %e, "sidecar closed during shutdown (expected)");
                }
                other => warn!(error = %other, "unexpected error during shutdown"),
            }
        }
    }

    /// Wait for one sidecar child to exit, killing it if it outlives
    /// [`SHUTDOWN_TIMEOUT`]. The kill path returns `Ok` — a sidecar that had
    /// to be killed is a logged anomaly, not a failure of *our* shutdown.
    async fn wait_or_kill(child: &Mutex<Child>, label: &str) -> Result<(), ClientError> {
        let mut guard = child.lock().await;
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, guard.wait()).await {
            Ok(status) => {
                debug!(status = ?status?, label, "sidecar exited");
            }
            Err(_elapsed) => {
                warn!(label, "sidecar ignored shutdown request; killing it");
                let _ = guard.start_kill();
                let _ = guard.wait().await;
            }
        }
        Ok(())
    }
}

impl ConnectionError {
    /// True if this error means the sidecar's stdio is gone — useful for
    /// the server to decide whether to respawn.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, ConnectionError::Closed | ConnectionError::Io(_))
    }
}

// std::io::Error doesn't implement From for ClientError directly because
// we want callers to use `?` on `Connection` errors. The blanket From
// above (via `#[from]`) covers ConnectionError, which already wraps Io.
impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Connection(ConnectionError::Io(e))
    }
}

// --------------------------------------------------------------------------
// Stderr drain
// --------------------------------------------------------------------------

/// Background task: read the sidecar's stderr line-by-line and re-emit
/// each line through `tracing`.
///
/// Level is picked from the line contents so users can filter normally:
///   * `panic`, `error:`, `parse error`           → `error!`
///   * `warn`                                     → `warn!`
///   * everything else (incl. `timing`, `scope=`) → `info!`
///
/// All lines use the `mimir_slang_sidecar` target so callers can scope
/// log filters at it specifically:
///
/// ```text
/// RUST_LOG=mimir=info,mimir_slang_sidecar=info
/// ```
///
/// Two invariants this preserves:
///
/// 1. **No pipe stalls.** Without this drain, the OS pipe buffer for
///    the sidecar's stderr (typically 64 KiB on Linux) fills under
///    `MIMIR_SLANG_TIMING=1` or `MIMIR_DEBUG_TIMING=1`, eventually
///    blocking the sidecar's `write` and stalling elaborate.
///
/// 2. **Timing output is visible.** Before this drain, the sidecar's
///    `[mimir-slang-sidecar] timing build=... visit=...` summary was
///    written but never observed because nothing read the pipe and the
///    pipe closed when the child exited.
async fn drain_sidecar_stderr(stderr: tokio::process::ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                debug!("sidecar stderr closed");
                return;
            }
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if trimmed.is_empty() {
                    continue;
                }
                let lower = trimmed.to_ascii_lowercase();
                if lower.contains("panic")
                    || lower.contains("error:")
                    || lower.contains("parse error")
                {
                    error!(target: "mimir_slang_sidecar", "{}", trimmed);
                } else if lower.contains("warn") {
                    warn!(target: "mimir_slang_sidecar", "{}", trimmed);
                } else {
                    info!(target: "mimir_slang_sidecar", "{}", trimmed);
                }
            }
            Err(e) => {
                warn!(error = %e, "sidecar stderr read failed; ending drain");
                return;
            }
        }
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SourceFile;
    use pretty_assertions::assert_eq;
    use tokio::io::{duplex, AsyncWriteExt, BufReader};

    /// A `Connection` plumbed against an in-memory `duplex` pair acts as
    /// the test harness: one end stands in for "the sidecar", the other
    /// is the real client.
    ///
    /// Each test spawns a tokio task that role-plays the sidecar:
    /// reads exactly one request line, sends back the canned response.
    fn pair() -> ((BufReader<tokio::io::DuplexStream>, tokio::io::DuplexStream),
                  (BufReader<tokio::io::DuplexStream>, tokio::io::DuplexStream)) {
        // Two duplexes: one for client→sidecar, one for sidecar→client.
        // Each duplex gives us a (read, write) pair; we pair them so the
        // client writes go into what the sidecar reads, and vice versa.
        let (client_to_sidecar_w, client_to_sidecar_r) = duplex(64 * 1024);
        let (sidecar_to_client_w, sidecar_to_client_r) = duplex(64 * 1024);
        (
            // Client side: reads from sidecar, writes to sidecar.
            (BufReader::new(sidecar_to_client_r), client_to_sidecar_w),
            // Sidecar side: reads from client, writes to client.
            (BufReader::new(client_to_sidecar_r), sidecar_to_client_w),
        )
    }

    /// Round-trip: client sends `compile`, fake sidecar echoes back a
    /// canned `CompileResult` with one diagnostic, client decodes it.
    #[tokio::test]
    async fn compile_request_response_roundtrip() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, methods::COMPILE);

            let resp = Response {
                id: req.id,
                result: Some(serde_json::json!({
                    "ast": {"files": []},
                    "diagnostics": [{
                        "path": "a.sv",
                        "range": {"start": {"line": 0, "character": 0},
                                  "end":   {"line": 0, "character": 4}},
                        "severity": "error",
                        "code": "ExpectedSemicolon",
                        "message": "expected ;"
                    }]
                })),
                error: None,
            };
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            s_w.write_all(out.as_bytes()).await.unwrap();
        });

        let mut conn = Connection::new(c_r, c_w);
        let params = ElaborateParams {
            files: vec![SourceFile {
                path: "a.sv".into(),
                text: "module m endmodule".into(),
                is_compilation_unit: true,
            }],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            extra_args: vec![],
            single_unit: false,
            timescale: None,
        };
        let result = conn.compile(&params).await.expect("compile ok");
        sidecar.await.unwrap();

        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, "ExpectedSemicolon");
    }

    /// Sidecar returns a wire-level error → client surfaces it as
    /// `ConnectionError::Sidecar`.
    #[tokio::test]
    async fn sidecar_error_response_surfaces_as_error() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            let resp = Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: -32602,
                    message: "no files provided".into(),
                }),
            };
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            s_w.write_all(out.as_bytes()).await.unwrap();
        });

        let mut conn = Connection::new(c_r, c_w);
        let params = ElaborateParams {
            files: vec![],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            extra_args: vec![],
            single_unit: false,
            timescale: None,
        };
        let err = conn.compile(&params).await.unwrap_err();
        sidecar.await.unwrap();

        match err {
            ConnectionError::Sidecar(e) => {
                assert_eq!(e.code, -32602);
                assert!(e.message.contains("no files"));
            }
            other => panic!("expected Sidecar error, got {other:?}"),
        }
    }

    /// Sidecar drops the connection (closes its write end) without
    /// responding → client surfaces `Closed`, and `is_terminal` agrees.
    #[tokio::test]
    async fn closed_pipe_surfaces_as_closed() {
        let ((c_r, c_w), (mut s_r, s_w)) = pair();

        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            // Close the write half by dropping it before responding.
            drop(s_w);
        });

        let mut conn = Connection::new(c_r, c_w);
        let params = ElaborateParams {
            files: vec![],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            extra_args: vec![],
            single_unit: false,
            timescale: None,
        };
        let err = conn.compile(&params).await.unwrap_err();
        sidecar.await.unwrap();

        assert!(matches!(err, ConnectionError::Closed));
        assert!(err.is_terminal());
    }

    /// Sidecar returns a response with an id *ahead* of what we sent
    /// (id > expected) — genuine protocol violation → `IdMismatch`.
    #[tokio::test]
    async fn id_ahead_surfaces_as_id_mismatch() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            let resp = Response {
                id: req.id.wrapping_add(999), // jump ahead → protocol error
                result: Some(serde_json::json!({"ast": {"files": []}, "diagnostics": []})),
                error: None,
            };
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            s_w.write_all(out.as_bytes()).await.unwrap();
        });

        let mut conn = Connection::new(c_r, c_w);
        let err = conn
            .compile(&ElaborateParams {
                files: vec![],
                include_dirs: vec![],
                defines: vec![],
                top: None,
                extra_args: vec![],
                single_unit: false,
                timescale: None,
            })
            .await
            .unwrap_err();
        sidecar.await.unwrap();

        assert!(matches!(err, ConnectionError::IdMismatch { .. }));
    }

    /// When a previous task was aborted after sending its request, the
    /// sidecar still writes back a stale response. The next `request` call
    /// should drain the stale response and return the fresh one.
    ///
    /// This reproduces the "response id mismatch: expected 2, got 1" error
    /// that appeared when `did_change` cancelled a pending elaborate that
    /// had already been sent to the sidecar.
    #[tokio::test]
    async fn stale_response_drained_transparently() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        // The "sidecar" reads one request (id=2) and first sends back a
        // stale id=1 response (simulating the buffered response from a prior
        // cancelled task), then the fresh id=2 response.
        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();

            // Stale response for the prior cancelled request.
            let stale = Response {
                id: 1,
                result: Some(serde_json::json!({"ast": {"files": []}, "diagnostics": []})),
                error: None,
            };
            let mut out = serde_json::to_string(&stale).unwrap();
            out.push('\n');
            s_w.write_all(out.as_bytes()).await.unwrap();
            s_w.flush().await.unwrap();

            // Fresh response for the current request.
            let fresh = Response {
                id: 2,
                result: Some(serde_json::json!({"ast": {"files": []}, "diagnostics": []})),
                error: None,
            };
            let mut out = serde_json::to_string(&fresh).unwrap();
            out.push('\n');
            s_w.write_all(out.as_bytes()).await.unwrap();
        });

        // Start at next_id=2, simulating a prior request (id=1) that was
        // written but never read (Tokio task aborted before read_line).
        let mut conn = Connection::with_next_id(c_r, c_w, 2);
        let result = conn
            .compile(&ElaborateParams {
                files: vec![],
                include_dirs: vec![],
                defines: vec![],
                top: None,
                extra_args: vec![],
                single_unit: false,
                timescale: None,
            })
            .await;
        sidecar.await.unwrap();

        // Stale id=1 is drained; id=2 matches → success.
        assert!(result.is_ok(), "expected ok, got {result:?}");
    }

    /// Two sequential requests get sequential ids and both round-trip.
    #[tokio::test]
    async fn id_increments_across_requests() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        let sidecar = tokio::spawn(async move {
            for _ in 0..2 {
                let mut line = String::new();
                s_r.read_line(&mut line).await.unwrap();
                let req: Request = serde_json::from_str(&line).unwrap();
                let resp = Response {
                    id: req.id,
                    result: Some(serde_json::json!({"ast": {"files": []}, "diagnostics": []})),
                    error: None,
                };
                let mut out = serde_json::to_string(&resp).unwrap();
                out.push('\n');
                s_w.write_all(out.as_bytes()).await.unwrap();
            }
        });

        let mut conn = Connection::new(c_r, c_w);
        let p = ElaborateParams {
            files: vec![],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            extra_args: vec![],
            single_unit: false,
            timescale: None,
        };
        let _ = conn.compile(&p).await.unwrap();
        let _ = conn.compile(&p).await.unwrap();
        sidecar.await.unwrap();
    }

    /// A sidecar that never answers makes `request_with_deadline` return
    /// `Timeout` instead of hanging forever — the bug this deadline exists
    /// to prevent.
    #[tokio::test]
    async fn silent_sidecar_surfaces_as_timeout() {
        let ((c_r, c_w), (mut s_r, _s_w)) = pair();

        // The "sidecar" reads the request and then goes silent (but keeps
        // its write end open, so the client sees neither data nor EOF).
        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            // Hold the write end open until the client has timed out.
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let mut conn = Connection::new(c_r, c_w);
        let err = conn
            .request_with_deadline::<_, CompileResult>(
                methods::COMPILE,
                &serde_json::json!({"files": []}),
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        sidecar.await.unwrap();

        assert!(matches!(err, ConnectionError::Timeout { .. }), "got {err:?}");
    }

    /// After a timed-out request — including one cancelled in the middle of
    /// reading a *partial* response line — the connection re-synchronises:
    /// the next request finishes and discards the leftover line, drains the
    /// stale response by id, and succeeds.
    #[tokio::test]
    async fn connection_resyncs_after_timed_out_request() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        let sidecar = tokio::spawn(async move {
            // Request 1: send only *half* the response line, then stall past
            // the client's deadline so the cancelled read leaves a partial
            // line in the client's buffer — and only then send the rest, the
            // way a briefly-stalled (but alive) sidecar would.
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            let full = {
                let resp = Response {
                    id: req.id,
                    result: Some(serde_json::json!({"ast": {"files": []}, "diagnostics": []})),
                    error: None,
                };
                let mut s = serde_json::to_string(&resp).unwrap();
                s.push('\n');
                s
            };
            let (head, tail) = full.split_at(full.len() / 2);
            s_w.write_all(head.as_bytes()).await.unwrap();
            s_w.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            s_w.write_all(tail.as_bytes()).await.unwrap();
            s_w.flush().await.unwrap();

            // Request 2: answer it properly.
            let mut line2 = String::new();
            s_r.read_line(&mut line2).await.unwrap();
            let req2: Request = serde_json::from_str(&line2).unwrap();
            let resp2 = Response {
                id: req2.id,
                result: Some(serde_json::json!({"ast": {"files": []}, "diagnostics": []})),
                error: None,
            };
            let mut out = serde_json::to_string(&resp2).unwrap();
            out.push('\n');
            s_w.write_all(out.as_bytes()).await.unwrap();
        });

        let mut conn = Connection::new(c_r, c_w);
        let params = serde_json::json!({"files": []});

        let first = conn
            .request_with_deadline::<_, CompileResult>(
                methods::COMPILE,
                &params,
                Duration::from_millis(50),
            )
            .await;
        assert!(
            matches!(first, Err(ConnectionError::Timeout { .. })),
            "first request should time out, got {first:?}",
        );

        // Second request must succeed despite the partial + stale bytes.
        let second = conn
            .request_with_deadline::<_, CompileResult>(
                methods::COMPILE,
                &params,
                Duration::from_secs(5),
            )
            .await;
        sidecar.await.unwrap();
        assert!(second.is_ok(), "expected resync + success, got {second:?}");
    }

    /// Spawning a non-existent program surfaces as `ClientError::Spawn`,
    /// not a panic. This exercises the process-layer code without needing
    /// a real sidecar binary on disk.
    #[tokio::test]
    async fn spawn_missing_binary_returns_spawn_error() {
        let result = Client::spawn(
            "/this/path/definitely/does/not/exist/mimir-slang-fake",
            std::iter::empty::<&str>(),
        )
        .await;
        match result {
            Err(ClientError::Spawn { program, .. }) => {
                assert!(program.contains("does/not/exist"));
            }
            Ok(_) => panic!("expected spawn to fail"),
            Err(other) => panic!("expected Spawn error, got {other:?}"),
        }
    }
}
