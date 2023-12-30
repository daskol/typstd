use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

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
    _entrypoint: String,
}

#[derive(Debug, Deserialize)]
struct TypstProject {
    #[serde(rename = "document")]
    documents: Vec<TypstDocument>,
    _package: Option<TypstPackage>,
}

/// Target represents a compilation target for a particular main file located
/// at specific root directory.
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
    /// Actual execution contexts for language analysis. It would be better to
    /// use URI as keys instead of paths if we want non-local environment such
    /// as browsers.
    worlds: RwLock<HashMap<PathBuf, Arc<Mutex<LanguageServiceWorld>>>>,
}

impl TypstLanguageService {
    /// Find the closest parent URI for the specified one.
    fn find_world(
        &self,
        uri: &Url,
    ) -> Option<Arc<Mutex<LanguageServiceWorld>>> {
        let mut path = Path::new(uri.path());
        let worlds = self.worlds.read().unwrap();
        // Is it better to use trie or something like that?
        while let Some(parent) = path.parent() {
            log::info!("look at {:?} path", path);
            match worlds.get(parent) {
                Some(world) => return Some(world.clone()),
                None => {
                    path = parent;
                }
            };
        }
        None
    }
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

        let mut root_uris = Vec::<Url>::new();
        if let Some(folders) = params.workspace_folders {
            root_uris.extend(folders.iter().map(|folder| folder.uri.clone()));
        } else if let Some(root_uri) = params.root_uri {
            root_uris.push(root_uri);
        } else {
            // TODO: Use current directory?
            log::warn!("there is not root workspace")
        }

        tracing::info!("try to load workspace configuration from typst.toml");
        let mut targets = Vec::<Target>::new();
        for root_uri in root_uris.iter() {
            match import_targets(Path::new(root_uri.path())) {
                Ok(new_targets) => targets.extend(new_targets),
                Err(err) => log::warn!(
                    "failed to import targets from {}: {}",
                    root_uri,
                    err
                ),
            };
        }

        for ent in targets.iter() {
            match LanguageServiceWorld::new(&ent.root_dir, &ent.main_file) {
                Some(world) => {
                    log::info!(
                        "initialize world for {:?} at {:?}",
                        ent.main_file,
                        ent.root_dir,
                    );
                    let world = Mutex::new(world);
                    self.worlds
                        .write()
                        .unwrap()
                        .insert(ent.root_dir.clone(), world.into());
                }
                None => log::error!(
                    "failed to initialize world for {:?} at {:?}",
                    ent.main_file,
                    ent.root_dir,
                ),
            };
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

    #[instrument(skip_all)]
    async fn initialized(&self, _params: InitializedParams) {
        log::info!("language server client is initialized");
    }

    #[instrument(skip_all)]
    async fn shutdown(&self) -> Result<()> {
        log::info!("shutdown language server");
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
            let Some(world) = self.find_world(&uri) else {
                return;
            };
            world.lock().unwrap().update_file(
                Path::new(uri.path()),
                change.text.as_str(),
                (begin.line as usize, begin.character as usize),
                (end.line as usize, end.character as usize),
            );
        }

        log::info!("try to compile document");
        let Some(world) = self.find_world(&uri) else {
            return;
        };
        let started_at = Instant::now();
        world.lock().unwrap().compile();
        let elapsed = started_at.elapsed();
        log::info!("compilation finished in {:?}", elapsed);
    }

    #[instrument(skip_all, fields(uri = %params.text_document.uri))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        // TODO: Find context (world) by file and evalute the context.
        log::info!(
            "open {} as {}",
            params.text_document.uri,
            params.text_document.language_id
        );

        // It seems that there is a data race in sense that we are trying to
        // create a new world non-atomically. This means that a concurrent
        // call can create a new world faster.
        let uri = params.text_document.uri;
        let path = Path::new(uri.path());
        let world = match self.find_world(&uri) {
            Some(world) => world.clone(),
            None => {
                let Some(root_dir) = path.parent() else {
                    log::error!("there is no root directory for {:?}", path);
                    return;
                };
                let world = LanguageServiceWorld::new(root_dir, path);
                match world {
                    Some(world) => {
                        let world = Arc::new(Mutex::new(world));
                        self.worlds
                            .write()
                            .unwrap()
                            .insert(root_dir.to_path_buf(), world.clone());
                        world
                    }
                    None => {
                        log::error!(
                            "failed to create new world from scratch for {:?}",
                            path
                        );
                        return;
                    }
                }
            }
        };

        log::info!("find world {:?} at ...", path);
        world
            .lock()
            .unwrap()
            .add_file(path, params.text_document.text);

        log::info!("try to compile document");
        let started_at = Instant::now();
        world.lock().unwrap().compile();
        let elapsed = started_at.elapsed();
        log::info!("compilation finished in {:?}", elapsed);
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

        let uri = params.text_document_position.text_document.uri;
        let path = Path::new(uri.path());
        let world = match self.find_world(&uri) {
            Some(world) => world,
            None => {
                log::error!("unable to find a world for completion");
                return Ok(None);
            }
        };

        let labels = world.lock().unwrap().complete(
            path,
            position.line as usize,
            position.character as usize,
        );
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
        .add_directive("typstd=info".parse().unwrap());

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
        .add_directive("typstd=info".parse().unwrap());

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
            worlds: Default::default(),
        });
        Server::new(stdin, stdout, socket).serve(service).await;
    };
}
