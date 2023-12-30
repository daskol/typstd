use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde::Deserialize;

use clap::Parser;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{LanguageServer, LspService, Server};
use tracing::instrument;
use tracing_subscriber::{
    fmt, layer::SubscriberExt, util::SubscriberInitExt, util::TryInitError,
    EnvFilter,
};
use typst_ide::CompletionKind;

use typstd::LanguageServiceWorld;

#[derive(Debug, Deserialize)]
struct TypstDocument {
    entrypoint: String,
}

#[derive(Debug, Deserialize)]
struct TypstPackage {
    entrypoint: String,
}

#[derive(Debug, Deserialize)]
struct TypstProject {
    #[serde(rename = "document")]
    documents: Vec<TypstDocument>,
    package: Option<TypstPackage>,
}

struct Target {
    root_dir: PathBuf,
    main_file: PathBuf,
}

fn import_targets(root_dir: &Path) -> std::result::Result<Vec<Target>, String> {
    let path = root_dir.join("typst.toml");
    let bytes = fs::read(&path)
        .map_err(|err| format!("failed to read {path:?}: {err}"))?;
    let runes = std::str::from_utf8(&bytes)
        .map_err(|err| format!("failed to decode utf-8 at {path:?}: {err}"))?;
    let config = toml::from_str::<TypstProject>(runes)
        .map_err(|err| format!("failed to parse toml at {path:?}: {err}"))?;

    let targets = config
        .documents
        .iter()
        .map(|doc| Target {
            root_dir: root_dir.to_path_buf(),
            main_file: root_dir.join(&doc.entrypoint),
        })
        .collect();

    Ok(targets)
}

#[derive(Debug)]
struct TypstLanguageService {
    root_uris: RwLock<Vec<Url>>,
    /// Actual execution context for language analysis.
    world: Mutex<LanguageServiceWorld>,
}

#[tower_lsp::async_trait]
impl LanguageServer for TypstLanguageService {
    #[instrument(
        skip_all,
        fields(process_id = params.process_id),
    )]
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        tracing::info!(
            "initialize language server params={}",
            serde_json::to_string(&params).unwrap()
        );

        if let Some(folders) = params.workspace_folders {
            self.root_uris
                .write()
                .unwrap()
                .extend(folders.iter().map(|folder| folder.uri.clone()));
        } else if let Some(root_uri) = params.root_uri {
            self.root_uris.write().unwrap().push(root_uri);
        } else {
            // TODO: Use current directory?
            log::warn!("there is not root workspace")
        }

        if let Some(root_uri) = self.root_uris.read().unwrap().first() {
            log::info!(
                "init language server at workspace {} (total {} folders)",
                root_uri,
                self.root_uris.read().unwrap().len()
            );
        }

        tracing::info!("try to load workspace configuration from typst.toml");
        for root_uri in self.root_uris.read().unwrap().iter() {
            let targets = import_targets(&PathBuf::from(root_uri.path()));
            if let Ok(targets) = targets {
                for target in targets.iter() {
                    tracing::info!("import target {:?}", target.main_file);
                }
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(
                            true,
                        )),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "#".to_string(),
                        ".".to_string(),
                        "@".to_string(),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(
                        WorkspaceFoldersServerCapabilities {
                            supported: Some(true),
                            change_notifications: Some(OneOf::Left(true)),
                        },
                    ),
                    file_operations: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    #[instrument]
    async fn initialized(&self, _params: InitializedParams) {}

    #[instrument]
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    #[instrument(skip_all, fields(uri = %params.text_document.uri))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        log::info!("close {}", params.text_document.uri);
    }

    #[instrument(skip_all, fields(uri = %params.text_document.uri))]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        log::info!("apply {} changes", params.content_changes.len());
        // TODO: (1) find a context by URI; (2) trigger an update of that
        // source within Context(?).
        let uri = params.text_document.uri;
        for change in params.content_changes.iter() {
            let Some(range) = change.range else {
                continue;
            };
            let begin = range.start;
            let end = range.end;
            self.world.lock().unwrap().update_file(
                Path::new(uri.path()),
                change.text.as_str(),
                (begin.line as usize, begin.character as usize),
                (end.line as usize, end.character as usize),
            );
        }
    }

    #[instrument(skip_all, fields(uri = %params.text_document.uri))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        // TODO: Find context (world) by file and evalute the context.
        log::info!(
            "open {} as {}",
            params.text_document.uri,
            params.text_document.language_id
        );

        let path = params.text_document.uri.path();
        let text = params.text_document.text;
        log::info!("add {} as a main file", path);
        self.world
            .lock()
            .unwrap()
            .add_main_file(Path::new(path), text);

        log::info!("try to compile {}", path);
        self.world.lock().unwrap().compile()
    }

    #[instrument(skip_all, fields(uri = %params.text_document.uri))]
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        log::info!("save {}", params.text_document.uri);
    }

    #[instrument(
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri),
    )]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        log::info!(
            "hover at {}:{}",
            params.text_document_position_params.position.line,
            params.text_document_position_params.position.character,
        );
        Ok(None)
    }

    #[instrument(
        skip_all,
        fields(uri = %params.text_document_position.text_document.uri),
    )]
    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position.position;
        log::info!("complete at {}:{}", position.line, position.character);
        // TODO: Get source by URI.
        let labels = self
            .world
            .lock()
            .unwrap()
            .complete(position.line as usize, position.character as usize);
        if labels.is_empty() {
            return Ok(None);
        }
        let items = labels
            .iter()
            .map(|el| CompletionItem {
                label: el.label.clone(),
                kind: Some(match el.kind {
                    CompletionKind::Func => CompletionItemKind::FUNCTION,
                    CompletionKind::Syntax => CompletionItemKind::SNIPPET,
                    CompletionKind::Type => CompletionItemKind::CLASS,
                    CompletionKind::Param => CompletionItemKind::VALUE,
                    CompletionKind::Constant => CompletionItemKind::CONSTANT,
                    // There is no suitable category for symbols (like
                    // dot.circle) in language server protocol. So we decided
                    // to map `Symbol` to `EnumMember` since set of all
                    // symbols are is bounded and we can say that all symbols
                    // constitutes some big enumeration. ¯\_(ツ)_/¯
                    CompletionKind::Symbol(_) => {
                        CompletionItemKind::ENUM_MEMBER
                    }
                }),
                ..Default::default()
            })
            .collect();
        Ok(Some(CompletionResponse::Array(items)))
    }
}

#[derive(Parser, Debug)]
#[clap(name = "typstd", version, author, about = "Typst language server.")]
struct Args {
    /// Path to log file.
    #[arg(long)]
    log_output: Option<String>,

    /// Listen TCP address
    #[arg(short, long)]
    listen: Option<String>,
}

#[cfg(not(feature = "otel"))]
fn set_up_logging() -> std::result::Result<(), TryInitError> {
    let log_file = tracing_appender::rolling::never(".", "typstd.log");

    // Parse an `EnvFilter` configuration from the `RUST_LOG`
    // environment variable.
    let filter = EnvFilter::from_env("TYPSTD_LOG")
        .add_directive(tracing::Level::INFO.into());

    // Use the tracing subscriber `Registry`, or any other subscriber
    // that impls `LookupSpan`
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::Layer::default().with_writer(log_file).with_ansi(false))
        .try_init()
}

#[cfg(feature = "otel")]
fn set_up_logging() -> std::result::Result<(), TryInitError> {
    // TODO: Take value either from envvar or CLI argument.
    let log_file = tracing_appender::rolling::never(".", "typstd.log");

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .install_simple()
        .expect("Unable to initialize OtlpPipeline");

    // Create a tracing layer with the configured tracer
    let opentelemetry = tracing_opentelemetry::layer().with_tracer(tracer);

    // Parse an `EnvFilter` configuration from the `RUST_LOG`
    // environment variable.
    let filter = EnvFilter::from_env("TYPSTD_LOG")
        .add_directive(tracing::Level::INFO.into());

    // Use the tracing subscriber `Registry`, or any other subscriber
    // that impls `LookupSpan`
    tracing_subscriber::registry()
        .with(opentelemetry)
        .with(filter)
        .with(fmt::Layer::default().with_writer(log_file).with_ansi(false))
        .try_init()
}

#[tokio::main]
pub async fn main() {
    // TODO: Take value either from envvar or CLI argument.
    let _ = set_up_logging();

    let args = Args::parse();
    if args.listen.is_some() {
        tracing::error!("not implemented"); // TODO
    } else {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::new(|_| TypstLanguageService {
            root_uris: RwLock::new(vec![]),
            world: LanguageServiceWorld::new().unwrap().into(), // TODO
        });
        Server::new(stdin, stdout, socket).serve(service).await;
    };
}
