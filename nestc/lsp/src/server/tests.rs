//! The server driven over an in-memory connection, the way an editor drives it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::notification::{
    DidChangeTextDocument, DidOpenTextDocument, Exit, Initialized, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{Initialize, Request as _, Shutdown};
use lsp_types::{
    DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    InitializeParams, PublishDiagnosticsParams, TextDocumentContentChangeEvent,
    TextDocumentItem, VersionedTextDocumentIdentifier,
};

use super::run;
use crate::analysis::path_to_uri;
use crate::workspace::{Metadata, Package, Target, Toolchain};

/// A directory of its own under the system's temporary one, removed when done.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nest-lsp-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The canonical spelling, which is the one an editor sends.
        Scratch(dir.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A toolchain that answers without running anything.
struct Fake(Result<Metadata, String>);

impl Toolchain for Fake {
    fn prepare(&self, _root: &Path) -> Result<Metadata, String> {
        self.0.clone()
    }
}

struct Client {
    conn: Connection,
    server: Option<JoinHandle<Result<(), String>>>,
    version: i32,
}

impl Client {
    fn start(toolchain: Fake) -> Client {
        let (server, conn) = Connection::memory();
        let toolchain: Arc<dyn Toolchain> = Arc::new(toolchain);
        let handle = std::thread::spawn(move || run(&server, |_| toolchain));
        let client = Client { conn, server: Some(handle), version: 0 };
        client.request(1, Initialize::METHOD, InitializeParams::default());
        client.expect_response(1);
        client.notify(Initialized::METHOD, serde_json::json!({}));
        client
    }

    fn request(&self, id: i32, method: &str, params: impl serde::Serialize) {
        let req = Request::new(RequestId::from(id), method.to_string(), params);
        self.conn.sender.send(req.into()).unwrap();
    }

    fn notify(&self, method: &str, params: impl serde::Serialize) {
        let n = Notification::new(method.to_string(), params);
        self.conn.sender.send(n.into()).unwrap();
    }

    fn recv(&self) -> Message {
        self.conn
            .receiver
            .recv_timeout(Duration::from_secs(120))
            .expect("the server answers")
    }

    fn expect_response(&self, id: i32) {
        match self.recv() {
            Message::Response(r) if r.id == RequestId::from(id) => {}
            other => panic!("expected the response to {id}, got {other:?}"),
        }
    }

    fn open(&mut self, path: &Path, text: &str) {
        self.version += 1;
        let item = TextDocumentItem::new(
            path_to_uri(path).unwrap(),
            "nest".to_string(),
            self.version,
            text.to_string(),
        );
        self.notify(DidOpenTextDocument::METHOD, DidOpenTextDocumentParams { text_document: item });
    }

    fn change(&mut self, path: &Path, text: &str) {
        self.version += 1;
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier::new(path_to_uri(path).unwrap(), self.version),
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_string(),
            }],
        };
        self.notify(DidChangeTextDocument::METHOD, params);
    }

    /// The next diagnostics published for `path`.
    fn diagnostics(&self, path: &Path) -> Vec<lsp_types::Diagnostic> {
        let uri = path_to_uri(path).unwrap();
        loop {
            if let Message::Notification(n) = self.recv()
                && n.method == PublishDiagnostics::METHOD
            {
                let p: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
                if p.uri == uri {
                    return p.diagnostics;
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let Some(server) = self.server.take() else { return };
        if std::thread::panicking() {
            return;
        }
        self.request(99, Shutdown::METHOD, ());
        self.expect_response(99);
        self.notify(Exit::METHOD, ());
        server.join().unwrap().unwrap();
    }
}

const WRONG: &str = "f :: func (n: i32) -> i32 { return n < n }\n";
const RIGHT: &str = "f :: func (n: i32) -> i32 { return n }\n";

/// A file with no manifest is analyzed on its own, from its buffer: an error
/// appears where it is, and goes when the buffer is fixed.
#[test]
fn a_buffer_s_errors_come_and_go() {
    let dir = Scratch::new();
    let file = dir.0.join("main.nest");
    // What is on disk is not what is analyzed.
    std::fs::write(&file, RIGHT).unwrap();
    let mut client = Client::start(Fake(Err("no workspace here".to_string())));

    client.open(&file, WRONG);
    let diags = client.diagnostics(&file);
    assert!(!diags.is_empty(), "the buffer has an error");
    assert!(diags.iter().any(|d| d.severity == Some(DiagnosticSeverity::ERROR)), "{diags:?}");
    assert_eq!(diags[0].range.start.line, 0, "{diags:?}");

    client.change(&file, RIGHT);
    assert_eq!(client.diagnostics(&file), Vec::new());
}

/// A file in a workspace is analyzed with the command line twig gave its
/// target, once the workspace is prepared.
#[test]
fn a_workspace_file_is_analyzed_as_its_target() {
    let dir = Scratch::new();
    std::fs::write(dir.0.join("nest.toml"), "").unwrap();
    let file = dir.0.join("src").join("main.nest");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, RIGHT).unwrap();
    let meta = Metadata {
        root: dir.0.clone(),
        packages: vec![Package {
            name: "app".to_string(),
            dir: dir.0.clone(),
            targets: vec![Target {
                entry: file.clone(),
                lib: false,
                args: vec![file.display().to_string(), "-C".to_string(), "profile=release".to_string()],
            }],
        }],
    };
    let mut client = Client::start(Fake(Ok(meta)));

    client.open(&file, WRONG);
    let diags = client.diagnostics(&file);
    assert!(!diags.is_empty(), "the buffer has an error");
}

/// A workspace twig could not prepare says why, on its open documents.
#[test]
fn a_workspace_that_cannot_be_prepared_says_why() {
    let dir = Scratch::new();
    std::fs::write(dir.0.join("nest.toml"), "").unwrap();
    let file = dir.0.join("main.nest");
    let mut client = Client::start(Fake(Err("`twig metadata` failed".to_string())));

    client.open(&file, RIGHT);
    let diags = client.diagnostics(&file);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(diags[0].message.contains("twig metadata"), "{diags:?}");
}
