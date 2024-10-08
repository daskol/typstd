use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::result;
use std::sync::Arc;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use clap::Parser;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::instrument;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, util::SubscriberInitExt, EnvFilter};
use typst_ide::CompletionKind;

use typstd::workspace::{search_targets, search_workspace, Target};
use typstd::LanguageServiceWorld;

#[derive(Debug)]
struct TypstLanguageService {
    /// Language Server Protocol (LSP) client for backward communication with
    /// service clients. Primarly, it is used for publishing diagnostics
    /// information.
    client: Client,
    /// Actual execution contexts for language analysis. It would be better to
    /// use URI as keys instead of paths if we want non-local environment such
    /// as browsers.
    worlds: RwLock<HashMap<PathBuf, Arc<Mutex<LanguageServiceWorld>>>>,
}

impl TypstLanguageService {
    /// Compile document and update user with compilation status.
    fn compile(&self, uri: &Url) -> result::Result<(), String> {
        log::info!("try to compile document");
        let Some((_, world)) = self.find_world(uri) else {
            return Err("missing compilation context".to_string());
        };
        let started_at = Instant::now();
        let result = world.lock().unwrap().compile();
        let elapsed = started_at.elapsed();
        match result {
            Ok(_) => {
                log::info!("compilation finished in {:?}", elapsed);
                Ok(())
            }
            Err(err) => {
                log::error!("compilation failed in {:?}: {}", elapsed, err);
                Err(err)
            }
        }
    }

    /// Find the closest parent URI for the specified one.
    fn find_world(
        &self,
        uri: &Url,
    ) -> Option<(PathBuf, Arc<Mutex<LanguageServiceWorld>>)> {
        let mut path = Path::new(uri.path());
        let worlds = self.worlds.read().unwrap();
        // Is it better to use trie or something like that?
        while let Some(parent) = path.parent() {
            match worlds.get(parent) {
                Some(world) => {
                    return Some((parent.to_path_buf(), world.clone()))
                }
                None => {
                    path = parent;
                }
            };
        }
        None
    }

    fn new_world_from_str(
        &self,
        uri: &Url,
        text: String,
    ) -> Option<(PathBuf, Arc<Mutex<LanguageServiceWorld>>)> {
        log::info!("initialize world from main file with text");
        let path = Path::new(uri.path());
        self.new_world_from_path(path, Some(text))
    }

    fn new_world_from_uri(
        &self,
        uri: &Url,
    ) -> Option<(PathBuf, Arc<Mutex<LanguageServiceWorld>>)> {
        let path = Path::new(uri.path());
        let Some(root_dir) = path.parent() else {
            log::error!("there is no root directory for {:?}", path);
            return None;
        };

        // Search for workspace root (i.e. search for `typst.toml`) from the
        // parent directory of the file to the filesystem hierarchy. If we
        // found nothing then fallback to base directory of the file.
        let root_dir = search_workspace(root_dir).unwrap_or(root_dir);

        // Create a new world and insert it to world index. If there are no valid targets then
        // create file-specific world; otherwise; search once again.
        let targets = search_targets(vec![root_dir]);
        log::info!("found {} target(s)", targets.len());
        match self.new_worlds(targets) {
            0 => self.new_world_from_path(path, None),
            _ => self
                .find_world(uri)
                .or_else(|| self.new_world_from_path(path, None)),
        }
    }

    fn new_world_from_path(
        &self,
        main_file: &Path,
        main_text: Option<String>,
    ) -> Option<(PathBuf, Arc<Mutex<LanguageServiceWorld>>)> {
        log::info!("initialize world from main file: path={:?}", main_file);
        let root_dir = main_file.parent()?;
        match LanguageServiceWorld::new(root_dir, main_file, main_text) {
            Some(world) => {
                log::info!(
                    "initialize world for {:?} at {:?}",
                    main_file,
                    root_dir,
                );
                let world = Arc::new(Mutex::new(world));
                self.worlds
                    .write()
                    .unwrap()
                    .insert(root_dir.to_path_buf(), world.clone());
                Some((root_dir.to_path_buf(), world))
            }
            None => {
                log::error!(
                    "failed to initialize world for {:?} at {:?}",
                    main_file,
                    root_dir,
                );
                None
            }
        }
    }

    fn new_worlds(&self, targets: Vec<Target>) -> u32 {
        let mut counter: u32 = 0;
        for (index, target) in targets.iter().enumerate() {
            let Some(relpath) =
                target.main_file.strip_prefix(&target.root_dir).ok()
            else {
                log::warn!(
                    "[{}] main file {:?} is not descendant of {:?}: skip it",
                    index,
                    target.root_dir,
                    target.main_file
                );
                continue;
            };
            match LanguageServiceWorld::new(
                &target.root_dir,
                &target.main_file,
                None,
            ) {
                Some(world) => {
                    log::info!(
                        "[{}] initialize world for {:?} at {:?}",
                        index,
                        relpath,
                        target.root_dir,
                    );
                    let world = Mutex::new(world);
                    self.worlds
                        .write()
                        .unwrap()
                        .insert(target.root_dir.clone(), world.into());
                    counter += 1;
                }
                None => log::error!(
                    "[{}] failed to initialize world for {:?} at {:?}",
                    index,
                    relpath,
                    target.root_dir,
                ),
            };
        }
        counter
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
        // It is safe to unwrap since all keys and values are JSON
        // serialiable.
        let params_json = serde_json::to_string_pretty(&params).unwrap();
        log::info!("initialize language server params={}", params_json);

        let mut root_uris = Vec::<Url>::new();
        if let Some(folders) = params.workspace_folders {
            log::info!("use workspace folders for targets discovery");
            root_uris.extend(folders.iter().map(|folder| folder.uri.clone()));
        } else if let Some(root_uri) = params.root_uri {
            log::info!("use obsolete root uri for targets discovery");
            root_uris.push(root_uri);
        }

        log::info!("try to load workspace configurations");
        let root_dirs = if !root_uris.is_empty() {
            root_uris
                .iter()
                .map(|uri| Path::new(uri.path()).to_path_buf())
                .collect()
        } else {
            log::warn!("no root uris: fallback to current work directory");
            env::current_dir().ok().map_or(vec![], |cwd| vec![cwd])
        };
        let root_dirs = root_dirs.iter().map(PathBuf::as_path).collect();
        let targets = search_targets(root_dirs);

        log::info!("found {} target(s)", targets.len());
        self.new_worlds(targets);

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

    #[instrument(
        skip_all,
        fields(uri = %params.text_document.uri.path_segments()
            .map(|it| it.last().unwrap_or("/"))
            .unwrap_or("/")
        )
    )]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        log::info!("close {}", params.text_document.uri);
    }

    #[instrument(
        skip_all,
        fields(uri = %params.text_document.uri.path_segments()
            .map(|it| it.last().unwrap_or("/"))
            .unwrap_or("/")
        )
    )]
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
            let Some((_, world)) = self.find_world(&uri) else {
                return;
            };
            world.lock().unwrap().update_file(
                Path::new(uri.path()),
                change.text.as_str(),
                (begin.line as usize, begin.character as usize),
                (end.line as usize, end.character as usize),
            );
        }
    }

    #[instrument(
        skip_all,
        fields(uri = %params.text_document.uri.path_segments()
            .map(|it| it.last().unwrap_or("/"))
            .unwrap_or("/")
        )
    )]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let lang_id = params.text_document.language_id;
        let uri = params.text_document.uri;
        log::info!("open {} text document {}", lang_id, uri);

        // It seems that there is a data race in sense that we are trying to
        // create a new world non-atomically. This means that a concurrent
        // call can create a new world faster.
        let path = Path::new(uri.path());
        let text = params.text_document.text;
        let Some((root_dir, world)) = self
            .find_world(&uri)
            .or_else(|| self.new_world_from_uri(&uri))
            .or_else(|| self.new_world_from_str(&uri, text.clone()))
        else {
            log::error!("failed to find or initialize new world");
            return;
        };

        log::info!("found world rooted at {:?}", root_dir);
        world.lock().unwrap().add_file(path, text);
        let _ = self.compile(&uri);
    }

    #[instrument(
        skip_all,
        fields(uri = %params.text_document.uri.path_segments()
            .map(|it| it.last().unwrap_or("/"))
            .unwrap_or("/")
        )
    )]
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        log::info!("save text document located at {}", uri);
        let Err(msg) = self.compile(&uri) else {
            self.client.publish_diagnostics(uri, vec![], None).await;
            return;
        };

        // Handle compilation errors in a primitive way.
        let pos = Position {
            line: 0,
            character: 0,
        };
        let diagnostic = Diagnostic {
            range: Range {
                start: pos,
                end: pos,
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("typst".to_string()),
            message: msg,
            ..Default::default()
        };
        self.client
            .publish_diagnostics(uri, vec![diagnostic], None)
            .await;
    }

    #[instrument(
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri
            .path_segments()
            .map(|it| it.last().unwrap_or("/"))
            .unwrap_or("/")
        )
    )]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        log::info!(
            "hover at {}:{} in {}",
            params.text_document_position_params.position.line,
            params.text_document_position_params.position.character,
            params.text_document_position_params.text_document.uri,
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
            Some((_, world)) => world,
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

#[cfg(not(feature = "telemetry"))]
fn init_logging(
    log_output: Option<String>,
) -> result::Result<(), Box<dyn Error>> {
    let filter = EnvFilter::from_env("TYPSTD_LOG")
        .add_directive("typstd=info".parse().unwrap());

    let registry = tracing_subscriber::registry().with(filter);

    match log_output {
        Some(path) => {
            let path = Path::new(&path);
            let log_dir = path.parent().unwrap_or(Path::new("."));
            let filename = path.file_name().ok_or("invalid log filename")?;
            let layer = fmt::Layer::default()
                .with_writer(tracing_appender::rolling::never(
                    log_dir, filename,
                ))
                .with_ansi(false);
            Ok(registry.with(layer).try_init()?)
        }
        None => Ok(registry.try_init()?),
    }
}

#[cfg(feature = "telemetry")]
fn init_logging() -> result::Result<(), Box<dyn Error>> {
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .install_simple()
        .expect("unable to initialize OtlpPipeline");

    // Create a tracing layer with the configured tracer
    let opentelemetry = tracing_opentelemetry::layer().with_tracer(tracer);

    // Parse an `EnvFilter` configuration from the `RUST_LOG`
    // environment variable.
    let filter = EnvFilter::from_env("TYPSTD_LOG")
        .add_directive("typstd=info".parse().unwrap());

    // Use the tracing subscriber `Registry`, or any other subscriber
    // that impls `LookupSpan`
    let registry = tracing_subscriber::registry()
        .with(opentelemetry)
        .with(filter);

    match log_output {
        Some(path) => {
            let path = Path::new(&path);
            let log_dir = path.parent().unwrap_or(Path::new("."));
            let filename = path.file_name().ok_or("invalid log filename")?;
            let layer = fmt::Layer::default()
                .with_writer(tracing_appender::rolling::never(
                    log_dir, filename,
                ))
                .with_ansi(false);
            Ok(registry.with(layer).try_init()?)
        }
        None => Ok(registry.try_init()?),
    }
}

#[tokio::main]
pub async fn main() {
    let args = Args::parse();
    if args.listen.is_some() {
        unimplemented!("serve over listen TCP/UDP sockets and WebSocket");
    }

    let _ = init_logging(args.log_output);

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| TypstLanguageService {
        client: client,
        worlds: Default::default(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
