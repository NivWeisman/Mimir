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
//! [`Client`]. This is fine for today's "elaborate on idle" pattern. If we
//! later need pipelining, the wire format already supports it — only the
//! client-side dispatcher needs replacing.
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

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, instrument, warn};

use crate::protocol::{
    methods, DefinitionParams, DefinitionResult, ElaborateParams, ElaborateResult, Request,
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

    /// Sidecar's response had an `id` that didn't match the request we
    /// just sent. Means the sidecar is buggy or we lost framing somewhere.
    #[error("response id mismatch: expected {expected}, got {got}")]
    IdMismatch {
        /// The id we put on the outgoing request.
        expected: u64,
        /// The id the sidecar echoed.
        got: u64,
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
    /// Reusable buffer for `read_line` to avoid a per-request allocation.
    line_buf: String,
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
            line_buf: String::new(),
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
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let req = Request {
            id,
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };

        // Encode + write + newline + flush. Doing the encode and append in
        // one allocation avoids a second syscall just for the `\n`.
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        debug!(bytes = line.len(), "sending request");
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;

        // Read exactly one line back. read_line keeps the trailing '\n',
        // which `serde_json::from_str` is happy to ignore.
        self.line_buf.clear();
        let n = self.reader.read_line(&mut self.line_buf).await?;
        if n == 0 {
            return Err(ConnectionError::Closed);
        }
        debug!(bytes = n, "received response");

        let resp: Response = serde_json::from_str(&self.line_buf)?;
        if resp.id != id {
            return Err(ConnectionError::IdMismatch {
                expected: id,
                got: resp.id,
            });
        }
        match (resp.result, resp.error) {
            (Some(value), None) => Ok(serde_json::from_value(value)?),
            (None, Some(err)) => Err(ConnectionError::Sidecar(err)),
            (None, None) | (Some(_), Some(_)) => Err(ConnectionError::EmptyResponse { id }),
        }
    }

    /// Convenience wrapper for the [`methods::ELABORATE`] method.
    pub async fn elaborate(
        &mut self,
        params: &ElaborateParams,
    ) -> Result<ElaborateResult, ConnectionError> {
        self.request(methods::ELABORATE, params).await
    }

    /// Convenience wrapper for the [`methods::DEFINITION`] method.
    pub async fn definition(
        &mut self,
        params: &DefinitionParams,
    ) -> Result<DefinitionResult, ConnectionError> {
        self.request(methods::DEFINITION, params).await
    }
}

// --------------------------------------------------------------------------
// Client — owns the sidecar process
// --------------------------------------------------------------------------

/// Owns a running slang sidecar process and a [`Connection`] over its stdio.
///
/// Drop semantics: dropping the `Client` drops the [`Child`], which sends
/// SIGKILL on Unix. For a graceful shutdown call [`Client::shutdown`]
/// first, which sends the protocol-level `shutdown` request and then waits
/// on the child.
pub struct Client {
    /// Wrapped in a `Mutex` because [`Connection::request`] needs `&mut`
    /// access. The lock is uncontended in the typical "one elaborate per
    /// idle period" pattern; if that changes we'd switch to a request
    /// dispatcher with a per-request channel and free `&self` callers.
    connection: Mutex<Connection<BufReader<ChildStdout>, ChildStdin>>,
    /// The child process. Held under a `Mutex` so `shutdown` can `wait`
    /// without competing with anyone else; `drop` doesn't need the lock
    /// (Mutex's `drop` is fine even if we never lock it).
    child: Mutex<Child>,
}

impl Client {
    /// Spawn the sidecar at `program` with the given arguments.
    ///
    /// All three of stdin/stdout/stderr are piped — stdin/stdout for the
    /// NDJSON channel, stderr so the sidecar's logs don't leak to ours.
    /// Stderr isn't currently drained; a future change will wire it into
    /// the `tracing` subscriber. (Until then, a chatty sidecar can fill
    /// the pipe buffer and block — fine for now because slang's logging
    /// is opt-in.)
    #[instrument(level = "debug", skip(args), fields(program = ?program.as_ref()))]
    pub async fn spawn<P, A, S>(program: P, args: A) -> Result<Self, ClientError>
    where
        P: AsRef<Path>,
        A: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let program_ref = program.as_ref();
        let mut command = Command::new(program_ref);
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
            program: program_ref.display().to_string(),
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

        let connection = Connection::new(BufReader::new(stdout), stdin);
        debug!("sidecar spawned");
        Ok(Self {
            connection: Mutex::new(connection),
            child: Mutex::new(child),
        })
    }

    /// Run an `elaborate` request against the sidecar.
    pub async fn elaborate(
        &self,
        params: &ElaborateParams,
    ) -> Result<ElaborateResult, ClientError> {
        let mut conn = self.connection.lock().await;
        Ok(conn.elaborate(params).await?)
    }

    /// Run a `definition` request against the sidecar.
    ///
    /// Same locking discipline as `elaborate`: holds the `Connection`
    /// mutex for the request/response cycle, so a `definition` arriving
    /// during a pending `elaborate` will queue. Acceptable for v1 — the
    /// editor's F12 is interactive but rare; pipelining is a separate
    /// slice if measurements show contention.
    pub async fn definition(
        &self,
        params: &DefinitionParams,
    ) -> Result<DefinitionResult, ClientError> {
        let mut conn = self.connection.lock().await;
        Ok(conn.definition(params).await?)
    }

    /// Send the `shutdown` request, then wait for the child to exit.
    ///
    /// We swallow a "sidecar already gone" error from the request itself
    /// (the sidecar might exit before flushing a response) but propagate
    /// other failures. Caller takes ownership so the type system enforces
    /// "you can't use this client after shutdown."
    pub async fn shutdown(self) -> Result<(), ClientError> {
        // We only care whether the request *got out*; the sidecar is
        // allowed to drop the connection without responding to `shutdown`.
        {
            let mut conn = self.connection.lock().await;
            let res: Result<serde_json::Value, _> =
                conn.request(methods::SHUTDOWN, &serde_json::Value::Null).await;
            if let Err(e) = res {
                // `Closed` is the expected outcome. Anything else is worth
                // a warn so we notice misbehaving sidecars.
                match e {
                    ConnectionError::Closed | ConnectionError::Io(_) => {
                        debug!(error = %e, "sidecar closed during shutdown (expected)");
                    }
                    other => warn!(error = %other, "unexpected error during shutdown"),
                }
            }
        }

        let mut child = self.child.lock().await;
        let status = child.wait().await?;
        debug!(?status, "sidecar exited");
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
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Diagnostic, Severity, SourceFile};
    use mimir_core::{Position, Range};
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

    /// Round-trip: client sends `elaborate`, fake sidecar echoes back a
    /// canned `ElaborateResult` with one diagnostic, client decodes it.
    #[tokio::test]
    async fn elaborate_request_response_roundtrip() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        // Sidecar role-play. Reads one line, parses the Request, sends
        // back a hard-coded ElaborateResult with one error diagnostic.
        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, methods::ELABORATE);

            let result = ElaborateResult {
                diagnostics: vec![Diagnostic {
                    path: "a.sv".into(),
                    range: Range::new(Position::new(0, 0), Position::new(0, 4)),
                    severity: Severity::Error,
                    code: "ExpectedSemicolon".into(),
                    message: "expected ;".into(),
                }],
            };
            let resp = Response {
                id: req.id,
                result: Some(serde_json::to_value(result).unwrap()),
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
        };
        let result = conn.elaborate(&params).await.expect("elaborate ok");
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
        };
        let err = conn.elaborate(&params).await.unwrap_err();
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
        };
        let err = conn.elaborate(&params).await.unwrap_err();
        sidecar.await.unwrap();

        assert!(matches!(err, ConnectionError::Closed));
        assert!(err.is_terminal());
    }

    /// Sidecar returns a response with the wrong id → client refuses it.
    #[tokio::test]
    async fn id_mismatch_surfaces_as_id_mismatch() {
        let ((c_r, c_w), (mut s_r, mut s_w)) = pair();

        let sidecar = tokio::spawn(async move {
            let mut line = String::new();
            s_r.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            let resp = Response {
                id: req.id.wrapping_add(999),
                result: Some(serde_json::json!({"diagnostics": []})),
                error: None,
            };
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            s_w.write_all(out.as_bytes()).await.unwrap();
        });

        let mut conn = Connection::new(c_r, c_w);
        let err = conn
            .elaborate(&ElaborateParams {
                files: vec![],
                include_dirs: vec![],
                defines: vec![],
                top: None,
            })
            .await
            .unwrap_err();
        sidecar.await.unwrap();

        assert!(matches!(err, ConnectionError::IdMismatch { .. }));
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
                    result: Some(serde_json::json!({"diagnostics": []})),
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
        };
        let _ = conn.elaborate(&p).await.unwrap();
        let _ = conn.elaborate(&p).await.unwrap();
        sidecar.await.unwrap();
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
