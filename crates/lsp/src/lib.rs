use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use ast_grep_config::Severity;
use ast_grep_config::{CombinedScan, RuleCollection, RuleConfig};
use ast_grep_core::{language::Language, AstGrep, Doc, Node, NodeMatch, StrDoc};

use std::collections::HashMap;
use std::path::PathBuf;

pub use tower_lsp::{LspService, Server};

pub trait LSPLang: Language + Eq + Send + Sync + 'static {}
impl<T> LSPLang for T where T: Language + Eq + Send + Sync + 'static {}

struct VersionedAst<D: Doc> {
  version: i32,
  root: AstGrep<D>,
}

pub struct Backend<L: LSPLang> {
  client: Client,
  map: DashMap<String, VersionedAst<StrDoc<L>>>,
  rules: std::result::Result<RuleCollection<L>, String>,
}

#[derive(Serialize, Deserialize)]
pub struct MatchRequest {
  pattern: String,
}

#[derive(Serialize, Deserialize)]
pub struct MatchResult {
  uri: String,
  position: Range,
  content: String,
}

impl MatchResult {
  fn new(uri: String, position: Range, content: String) -> Self {
    Self {
      uri,
      position,
      content,
    }
  }
}

impl<L: LSPLang> Backend<L> {
  pub async fn search(&self, params: MatchRequest) -> Result<Vec<MatchResult>> {
    let matcher = params.pattern;
    let mut match_result = vec![];
    for slot in self.map.iter() {
      let uri = slot.key();
      let versioned = slot.value();
      for matched_node in versioned.root.root().find_all(matcher.as_str()) {
        let content = matched_node.text().to_string();
        let range = convert_node_to_range(&matched_node);
        match_result.push(MatchResult::new(uri.clone(), range, content));
      }
    }
    Ok(match_result)
  }
}

const FALLBACK_CODE_ACTION_PROVIDER: Option<CodeActionProviderCapability> =
  Some(CodeActionProviderCapability::Simple(true));

const SOURCE_FIX_ALL_AST_GREP: CodeActionKind = CodeActionKind::new("source.fixAll.ast-grep");

fn code_action_provider(
  client_capability: &ClientCapabilities,
) -> Option<CodeActionProviderCapability> {
  let is_literal_supported = client_capability
    .text_document
    .as_ref()?
    .code_action
    .as_ref()?
    .code_action_literal_support
    .is_some();
  if !is_literal_supported {
    return None;
  }
  Some(CodeActionProviderCapability::Options(CodeActionOptions {
    code_action_kinds: Some(vec![
      CodeActionKind::QUICKFIX,
      CodeActionKind::SOURCE_FIX_ALL,
      SOURCE_FIX_ALL_AST_GREP,
    ]),
    work_done_progress_options: Default::default(),
    resolve_provider: Some(true),
  }))
}

#[tower_lsp::async_trait]
impl<L: LSPLang> LanguageServer for Backend<L> {
  async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
    Ok(InitializeResult {
      server_info: Some(ServerInfo {
        name: "ast-grep language server".to_string(),
        version: None,
      }),
      capabilities: ServerCapabilities {
        // TODO: change this to incremental
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        code_action_provider: code_action_provider(&params.capabilities)
          .or(FALLBACK_CODE_ACTION_PROVIDER),
        ..ServerCapabilities::default()
      },
    })
  }

  async fn initialized(&self, _: InitializedParams) {
    self
      .client
      .log_message(MessageType::INFO, "server initialized!")
      .await;

    // Report errors loading config once, upon initialization
    if let Err(error) = &self.rules {
      // popup message
      self
        .client
        .show_message(
          MessageType::ERROR,
          format!("Failed to load rules: {}", error),
        )
        .await;
      // log message
      self
        .client
        .log_message(
          MessageType::ERROR,
          format!("Failed to load rules: {}", error),
        )
        .await;
    }
  }

  async fn shutdown(&self) -> Result<()> {
    Ok(())
  }

  async fn did_change_workspace_folders(&self, _: DidChangeWorkspaceFoldersParams) {
    self
      .client
      .log_message(MessageType::INFO, "workspace folders changed!")
      .await;
  }

  async fn did_change_configuration(&self, _: DidChangeConfigurationParams) {
    self
      .client
      .log_message(MessageType::INFO, "configuration changed!")
      .await;
  }

  async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {
    self
      .client
      .log_message(MessageType::INFO, "watched files have changed!")
      .await;
  }
  async fn did_open(&self, params: DidOpenTextDocumentParams) {
    self
      .client
      .log_message(MessageType::INFO, "file opened!")
      .await;
    self.on_open(params).await;
  }

  async fn did_change(&self, params: DidChangeTextDocumentParams) {
    self.on_change(params).await;
  }

  async fn did_save(&self, _: DidSaveTextDocumentParams) {
    self
      .client
      .log_message(MessageType::INFO, "file saved!")
      .await;
  }

  async fn did_close(&self, params: DidCloseTextDocumentParams) {
    self.on_close(params).await;
    self
      .client
      .log_message(MessageType::INFO, "file closed!")
      .await;
  }

  async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
    self
      .client
      .log_message(MessageType::INFO, "run code action!")
      .await;
    Ok(self.on_code_action(params).await)
  }
}

fn convert_node_to_range<D: Doc>(node_match: &Node<D>) -> Range {
  let (start_row, start_col) = node_match.start_pos();
  let (end_row, end_col) = node_match.end_pos();
  Range {
    start: Position {
      line: start_row as u32,
      character: start_col as u32,
    },
    end: Position {
      line: end_row as u32,
      character: end_col as u32,
    },
  }
}

fn get_non_empty_message<L: Language>(rule: &RuleConfig<L>) -> String {
  // Note: The LSP client in vscode won't show any diagnostics at all if it receives one with an empty message
  if rule.message.is_empty() {
    rule.id.to_string()
  } else {
    rule.message.to_string()
  }
}
fn convert_match_to_diagnostic<L: Language>(
  node_match: NodeMatch<StrDoc<L>>,
  rule: &RuleConfig<L>,
  uri: &Url,
) -> Diagnostic {
  Diagnostic {
    range: convert_node_to_range(&node_match),
    code: Some(NumberOrString::String(rule.id.clone())),
    code_description: url_to_code_description(&rule.url),
    severity: Some(match rule.severity {
      Severity::Error => DiagnosticSeverity::ERROR,
      Severity::Warning => DiagnosticSeverity::WARNING,
      Severity::Info => DiagnosticSeverity::INFORMATION,
      Severity::Hint => DiagnosticSeverity::HINT,
      Severity::Off => unreachable!("turned-off rule should not have match"),
    }),
    message: get_non_empty_message(rule),
    source: Some(String::from("ast-grep")),
    tags: None,
    related_information: collect_labels(&node_match, uri),
    data: None,
  }
}

fn collect_labels<L: Language>(
  node_match: &NodeMatch<StrDoc<L>>,
  uri: &Url,
) -> Option<Vec<DiagnosticRelatedInformation>> {
  let secondary_nodes = node_match.get_env().get_labels("secondary")?;
  Some(
    secondary_nodes
      .iter()
      .map(|n| {
        let location = Location {
          uri: uri.clone(),
          range: convert_node_to_range(n),
        };
        DiagnosticRelatedInformation {
          location,
          message: String::new(),
        }
      })
      .collect(),
  )
}

fn url_to_code_description(url: &Option<String>) -> Option<CodeDescription> {
  let href = Url::parse(url.as_ref()?).ok()?;
  Some(CodeDescription { href })
}

impl<L: LSPLang> Backend<L> {
  pub fn new(client: Client, rules: std::result::Result<RuleCollection<L>, String>) -> Self {
    Self {
      client,
      rules,
      map: DashMap::new(),
    }
  }
  async fn publish_diagnostics(&self, uri: Url, versioned: &VersionedAst<StrDoc<L>>) -> Option<()> {
    let mut diagnostics = vec![];
    let path = uri.to_file_path().ok()?;

    let rules = match &self.rules {
      Ok(rules) => rules.for_path(&path),
      Err(_) => {
        return Some(());
      }
    };
    let scan = CombinedScan::new(rules);
    let hit_set = scan.all_kinds();
    let matches = scan.scan(&versioned.root, hit_set, false).matches;
    for (id, ms) in matches {
      let rule = scan.get_rule(id);
      let to_diagnostic = |m| convert_match_to_diagnostic(m, rule, &uri);
      diagnostics.extend(ms.into_iter().map(to_diagnostic));
    }
    self
      .client
      .publish_diagnostics(uri, diagnostics, Some(versioned.version))
      .await;
    Some(())
  }
  async fn on_open(&self, params: DidOpenTextDocumentParams) -> Option<()> {
    let text_doc = params.text_document;
    let uri = text_doc.uri.as_str().to_owned();
    let text = text_doc.text;
    self
      .client
      .log_message(MessageType::LOG, "Parsing doc.")
      .await;
    let lang = Self::infer_lang_from_uri(&text_doc.uri)?;
    let root = AstGrep::new(text, lang);
    let versioned = VersionedAst {
      version: text_doc.version,
      root,
    };
    self
      .client
      .log_message(MessageType::LOG, "Publishing init diagnostics.")
      .await;
    self.publish_diagnostics(text_doc.uri, &versioned).await;
    self.map.insert(uri.to_owned(), versioned); // don't lock dashmap
    Some(())
  }
  async fn on_change(&self, params: DidChangeTextDocumentParams) -> Option<()> {
    let text_doc = params.text_document;
    let uri = text_doc.uri.as_str();
    let text = &params.content_changes[0].text;
    self
      .client
      .log_message(MessageType::LOG, "Parsing changed doc.")
      .await;
    let lang = Self::infer_lang_from_uri(&text_doc.uri)?;
    let root = AstGrep::new(text, lang);
    let mut versioned = self.map.get_mut(uri)?;
    // skip old version update
    if versioned.version > text_doc.version {
      return None;
    }
    *versioned = VersionedAst {
      version: text_doc.version,
      root,
    };
    self
      .client
      .log_message(MessageType::LOG, "Publishing diagnostics.")
      .await;
    self.publish_diagnostics(text_doc.uri, &versioned).await;
    Some(())
  }
  async fn on_close(&self, params: DidCloseTextDocumentParams) {
    self.map.remove(params.text_document.uri.as_str());
  }

  fn compute_all_fixes(
    &self,
    text_document: TextDocumentIdentifier,
    error_id_to_ranges: HashMap<String, Vec<Range>>,
    rules: &RuleCollection<L>,
    path: PathBuf,
  ) -> Option<HashMap<Url, Vec<TextEdit>>>
  where
    L: ast_grep_core::Language + std::cmp::Eq,
  {
    let uri = text_document.uri.as_str();
    let versioned = self.map.get(uri)?;
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    let edits = changes.entry(text_document.uri.clone()).or_default();

    for config in rules.for_path(&path) {
      let ranges = match error_id_to_ranges.get(&config.id) {
        Some(ranges) => ranges,
        None => continue,
      };
      let matcher = &config.matcher;

      for matched_node in versioned.root.root().find_all(&matcher) {
        let range = convert_node_to_range(&matched_node);
        if !ranges.contains(&range) {
          continue;
        }
        let fixer = match &config.matcher.fixer {
          Some(fixer) => fixer,
          None => continue,
        };
        let edit = matched_node.replace_by(fixer);
        let edit = TextEdit {
          range,
          new_text: String::from_utf8(edit.inserted_text).unwrap(),
        };

        edits.push(edit);
      }
    }
    Some(changes)
  }

  async fn on_code_action(&self, params: CodeActionParams) -> Option<CodeActionResponse> {
    let text_doc = params.text_document;
    let path = text_doc.uri.to_file_path().ok()?;
    let diagnostics = params.context.diagnostics;
    let error_id_to_ranges = Self::build_error_id_to_ranges(diagnostics);
    let mut response = CodeActionResponse::new();

    let code_action = params.context.only.as_ref()?.first()?.clone();

    // we only handle these code_actions
    // 1. QuickFix
    // 2. "source.fixAll" and "source.fixAll.ast-grep"
    if code_action != CodeActionKind::QUICKFIX
      && code_action != CodeActionKind::SOURCE_FIX_ALL
      && code_action != SOURCE_FIX_ALL_AST_GREP
    {
      return Some(response);
    }

    let Ok(rules) = &self.rules else {
      return Some(response);
    };

    let changes = self.compute_all_fixes(text_doc, error_id_to_ranges, rules, path);

    let edit = Some(WorkspaceEdit {
      changes,
      document_changes: None,
      change_annotations: None,
    });
    let action = CodeAction {
      title: "Source Code fix action".to_string(),
      command: None,
      diagnostics: None,
      edit,
      disabled: None,
      kind: Some(code_action),
      is_preferred: Some(true),
      data: None,
    };

    response.push(CodeActionOrCommand::from(action));
    Some(response)
  }

  fn build_error_id_to_ranges(diagnostics: Vec<Diagnostic>) -> HashMap<String, Vec<Range>> {
    let mut error_id_to_ranges = HashMap::new();
    for diagnostic in diagnostics {
      let rule_id = match diagnostic.code {
        Some(NumberOrString::String(rule)) => rule,
        _ => continue,
      };
      let ranges = error_id_to_ranges.entry(rule_id).or_insert_with(Vec::new);
      ranges.push(diagnostic.range);
    }
    error_id_to_ranges
  }

  // TODO: support other urls besides file_scheme
  fn infer_lang_from_uri(uri: &Url) -> Option<L> {
    let path = uri.to_file_path().ok()?;
    L::from_path(path)
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use ast_grep_config::{from_yaml_string, GlobalRules};
  use ast_grep_language::SupportLang;
  use serde_json::Value;
  use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

  fn start_lsp() -> (DuplexStream, DuplexStream) {
    let globals = GlobalRules::default();
    let config: RuleConfig<SupportLang> = from_yaml_string(
      r"
id: no-console-rule
message: No console.log
severity: warning
language: TypeScript
rule:
  pattern: console.log($$$A)
note: no console.log
fix: |
  alert($$$A)
",
      &globals,
    )
    .unwrap()
    .pop()
    .unwrap();
    let rc: RuleCollection<SupportLang> = RuleCollection::try_new(vec![config]).unwrap();
    let rc_result: std::result::Result<_, String> = Ok(rc);
    let (service, socket) = LspService::build(|client| Backend::new(client, rc_result)).finish();
    let (req_client, req_server) = duplex(1024);
    let (resp_server, resp_client) = duplex(1024);

    // start server as concurrent task
    tokio::spawn(Server::new(req_server, resp_server, socket).serve(service));

    (req_client, resp_client)
  }

  fn req(msg: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg)
  }

  // A function that takes a byte slice as input and returns the content length as an option
  fn resp(input: &[u8]) -> Option<&str> {
    let input_str = std::str::from_utf8(input).ok()?;
    let mut splits = input_str.split("\r\n\r\n");
    let header = splits.next()?;
    let body = splits.next()?;
    let length_str = header.trim_start_matches("Content-Length: ");
    let length = length_str.parse::<usize>().ok()?;
    Some(&body[..length])
  }

  async fn test_lsp() {
    let initialize = r#"{
      "jsonrpc":"2.0",
      "id": 1,
      "method": "initialize",
      "params": {
        "capabilities": {
          "textDocumentSync": 1
        }
      }
    }"#;
    let (mut req_client, mut resp_client) = start_lsp();
    let mut buf = vec![0; 1024];

    req_client
      .write_all(req(initialize).as_bytes())
      .await
      .unwrap();
    let _ = resp_client.read(&mut buf).await.unwrap();

    assert!(resp(&buf).unwrap().starts_with('{'));

    let save_file = r#"{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "textDocument/codeAction",
  "params": {
    "range": {
      "end": {
        "character": 10,
        "line": 1
      },
      "start": {
        "character": 10,
        "line": 1
      }
    },
    "textDocument": {
      "uri": "file:///Users/codes/ast-grep-vscode/test.tsx"
    },
    "context": {
      "diagnostics": [
        {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 16
            }
          },
          "code": "no-console-rule",
          "source": "ast-grep",
          "message": "No console.log"
        }
      ],
      "only": ["source.fixAll"]
    }
  }
  }"#;

    let mut buf = vec![0; 1024];
    req_client
      .write_all(req(save_file).as_bytes())
      .await
      .unwrap();
    let _ = resp_client.read(&mut buf).await.unwrap();

    let json_val: Value = serde_json::from_str(resp(&buf).unwrap()).unwrap();

    // {"jsonrpc":"2.0","method":"window/logMessage","params":{"message":"run code action!","type":3}}
    assert_eq!(json_val["method"], "window/logMessage");
  }

  #[test]
  fn actual_test() {
    tokio::runtime::Runtime::new().unwrap().block_on(test_lsp());
  }
}
