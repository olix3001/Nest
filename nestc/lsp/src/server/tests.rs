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
use lsp_types::request::{Completion, GotoDefinition, HoverRequest, Initialize, Request as _, Shutdown};
use lsp_types::{
    DiagnosticSeverity, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    InitializeParams, Position, PublishDiagnosticsParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    VersionedTextDocumentIdentifier,
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

    /// Ask `method`, and wait for its answer past any notifications.
    fn ask(&mut self, method: &str, params: impl serde::Serialize) -> serde_json::Value {
        self.version += 1;
        let id = 1000 + self.version;
        self.request(id, method, params);
        loop {
            if let Message::Response(r) = self.recv() {
                assert_eq!(r.id, RequestId::from(id));
                return r.response_result.expect("an answer");
            }
        }
    }

    fn at(&mut self, method: &str, path: &Path, position: Position) -> serde_json::Value {
        let params = TextDocumentPositionParams::new(
            TextDocumentIdentifier::new(path_to_uri(path).unwrap()),
            position,
        );
        self.ask(method, params)
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

const PROGRAM: &str = "\
/// A point on a plane.
@public
Point :: struct {
  /// Across.
  x: i32,
  y: i32,
}

impl Point {
  /// Both coordinates, added.
  sum :: func (self: *Self) -> i32 { return self.x + self.y }
}

add :: func (a: i32, b: i32) -> i32 { return a + b }

main :: func () -> i32 {
  let p: Point := Point { x: 1, y: 2 }
  let n: i32 := add(p.x, 3)
  return p.sum() + n
}
";

/// Where the `nth` `needle` in `text` starts, plus `past` columns.
fn position(text: &str, needle: &str, nth: usize, past: u32) -> Position {
    let offset = text.match_indices(needle).nth(nth).expect("the needle is there").0;
    let pos = crate::analysis::position(text, offset);
    Position::new(pos.line, pos.character + past)
}

/// An open file, analyzed, and a client to ask about it.
fn program() -> (Scratch, PathBuf, Client) {
    let dir = Scratch::new();
    let file = dir.0.join("main.nest");
    let mut client = Client::start(Fake(Err("no workspace here".to_string())));
    // A correct file has no diagnostics to publish, and a question analyzes it.
    client.open(&file, PROGRAM);
    (dir, file, client)
}

fn hover_text(v: &serde_json::Value) -> String {
    v["contents"]["value"].as_str().unwrap_or_default().to_string()
}

/// A hover shows a function's declaration without its body, a method's
/// container and documentation, and a local's type.
#[test]
fn a_hover_shows_the_declaration_and_its_documentation() {
    let (_dir, file, mut client) = program();

    let add = hover_text(&client.at(HoverRequest::METHOD, &file, position(PROGRAM, "add(p.x", 0, 1)));
    assert!(add.contains("add :: func (a: i32, b: i32) -> i32\n```"), "{add}");

    let sum = hover_text(&client.at(HoverRequest::METHOD, &file, position(PROGRAM, "sum()", 0, 2)));
    assert!(sum.contains("sum :: func (self: *Self) -> i32"), "{sum}");
    assert!(sum.contains("Point"), "{sum}");
    assert!(sum.contains("Both coordinates, added."), "{sum}");

    let p = hover_text(&client.at(HoverRequest::METHOD, &file, position(PROGRAM, "p.sum", 0, 0)));
    assert!(p.contains("p: Point"), "{p}");

    // On a definition's own name, past its attribute.
    let point = hover_text(&client.at(HoverRequest::METHOD, &file, position(PROGRAM, "Point ::", 0, 1)));
    assert!(point.contains("A point on a plane."), "{point}");
    assert!(point.contains("x: i32"), "{point}");

    // A function's body is not its name.
    let body = client.at(HoverRequest::METHOD, &file, position(PROGRAM, "{ return a + b }", 0, 0));
    assert!(body.is_null(), "{body}");
}

/// Go-to-definition lands on the name: a function's, and a field's.
#[test]
fn a_definition_is_its_name() {
    let (_dir, file, mut client) = program();

    let add = client.at(GotoDefinition::METHOD, &file, position(PROGRAM, "add(p.x", 0, 0));
    assert_eq!(add["range"]["start"], serde_json::to_value(position(PROGRAM, "add ::", 0, 0)).unwrap());
    assert_eq!(add["uri"], serde_json::to_value(path_to_uri(&file)).unwrap());

    let x = client.at(GotoDefinition::METHOD, &file, position(PROGRAM, "p.x", 0, 2));
    assert_eq!(x["range"]["start"], serde_json::to_value(position(PROGRAM, "x: i32", 0, 0)).unwrap());
}

fn labels(v: &serde_json::Value) -> Vec<String> {
    v["items"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap().to_string()).collect()
}

/// After a `.` the members of the value's type are offered, and elsewhere the
/// names in scope.
#[test]
fn completion_offers_members_and_names_in_scope() {
    let (_dir, file, mut client) = program();
    let text = PROGRAM.replace("  return p.sum() + n", "  let m: i32 := p.\n  return p.sum() + n");
    client.change(&file, &text);

    let members = labels(&client.at(Completion::METHOD, &file, position(&text, "p.\n", 0, 2)));
    for want in ["x", "y", "sum"] {
        assert!(members.contains(&want.to_string()), "{want} in {members:?}");
    }
    assert!(!members.contains(&"add".to_string()), "{members:?}");

    let names = labels(&client.at(Completion::METHOD, &file, position(&text, "add(p.x", 0, 0)));
    for want in ["add", "p", "Point", "main", "let"] {
        assert!(names.contains(&want.to_string()), "{want} in {names:?}");
    }
    assert!(!names.contains(&"n".to_string()), "`n` is declared later: {names:?}");
}

/// A file a unit read, changed on disk while not open, is analyzed again when
/// the client says so.
#[test]
fn a_file_changed_on_disk_is_analyzed_again() {
    let dir = Scratch::new();
    let main = dir.0.join("main.nest");
    let other = dir.0.join("other.nest");
    std::fs::write(&other, "@public g :: func () -> i32 { return true }\n").unwrap();
    let mut client = Client::start(Fake(Err("no workspace here".to_string())));

    client.open(&main, "{ g } :: import \"other.nest\"\nf :: func () -> i32 { return g() }\n");
    assert!(!client.diagnostics(&other).is_empty(), "`other.nest` is wrong on disk");

    std::fs::write(&other, "@public g :: func () -> i32 { return 1 }\n").unwrap();
    let change = lsp_types::FileEvent::new(path_to_uri(&other).unwrap(), lsp_types::FileChangeType::CHANGED);
    client.notify(
        lsp_types::notification::DidChangeWatchedFiles::METHOD,
        lsp_types::DidChangeWatchedFilesParams { changes: vec![change] },
    );
    assert_eq!(client.diagnostics(&other), Vec::new());
}

/// After a `.` on a primitive, a slice or a string literal, their impls'
/// methods; in a `match` arm and where an enum is expected, its variants.
#[test]
fn completion_knows_primitives_slices_and_variants() {
    let (_dir, file, mut client) = program();
    let text = PROGRAM.replace(
        "  return p.sum() + n",
        "  let w: i32 := n.\n  let q: Color := .\n  let r: i32 := q.match { . => 1, _ => 2 }\n  return p.sum() + n",
    ) + "Color :: enum { red, green }\n";
    client.change(&file, &text);

    let ints = labels(&client.at(Completion::METHOD, &file, position(&text, "n.\n", 0, 2)));
    assert!(ints.contains(&"wrapping_add".to_string()), "{ints:?}");

    let made = labels(&client.at(Completion::METHOD, &file, position(&text, "Color := .", 0, 10)));
    assert!(made.contains(&"red".to_string()) && made.contains(&"green".to_string()), "{made:?}");

    let arms = labels(&client.at(Completion::METHOD, &file, position(&text, "{ . =>", 0, 3)));
    assert!(arms.contains(&"red".to_string()) && arms.contains(&"green".to_string()), "{arms:?}");
}

/// A name no import brings in is offered with the import that does, and
/// choosing it adds that import, after the file's others.
#[test]
fn completion_imports_what_it_offers() {
    let (_dir, file, mut client) = program();
    let text = "io :: import <std/io>\n".to_string()
        + &PROGRAM.replace("  return p.sum() + n", "  let h := HashM\n  return p.sum() + n");
    client.change(&file, &text);

    let answer = client.at(Completion::METHOD, &file, position(&text, "HashM\n", 0, 5));
    let items = answer["items"].as_array().unwrap();
    let map = items.iter().find(|i| i["label"] == "HashMap").expect("`HashMap` is offered");
    let edit = &map["additionalTextEdits"][0];
    assert_eq!(edit["newText"], "{ HashMap } :: import <std/collections>\n", "{map}");
    assert_eq!(edit["range"]["start"]["line"], 1, "{map}");
    // What is imported already is not offered again.
    assert!(!items.iter().any(|i| i["label"] == "io" && i.get("additionalTextEdits").is_some()), "{answer}");
}

/// An impl whose generics are bounded applies only where the bounds hold: a
/// blanket impl's methods are offered on a type that meets them, and not on one
/// that does not.
#[test]
fn completion_checks_an_impl_s_bounds() {
    let (_dir, file, mut client) = program();
    let text = PROGRAM.replace(
        "  return p.sum() + n",
        "  let sq: Square := Square { side: 2 }\n  let a: i32 := sq.\n  let b: i32 := p.\n  return p.sum() + n",
    ) + "\
Shape :: trait {
  area :: func (self: *Self) -> i32
}

Described :: trait {
  describe :: func (self: *Self) -> i32
}

impl <T: Shape> Described for T {
  describe :: func (self: *Self) -> i32 { return self.area() }
}

Square :: struct { side: i32 }

impl Shape for Square {
  area :: func (self: *Self) -> i32 { return self.side * self.side }
}
";
    client.change(&file, &text);

    let square = labels(&client.at(Completion::METHOD, &file, position(&text, "sq.\n", 0, 3)));
    assert!(square.contains(&"describe".to_string()), "{square:?}");
    assert!(square.contains(&"area".to_string()), "{square:?}");

    let point = labels(&client.at(Completion::METHOD, &file, position(&text, "p.\n", 0, 2)));
    assert!(point.contains(&"sum".to_string()), "{point:?}");
    assert!(!point.contains(&"describe".to_string()), "`Point` is not a `Shape`: {point:?}");
}
