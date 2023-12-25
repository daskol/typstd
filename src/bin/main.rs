use clap::Parser;
use std::fs::File;
use structured_logger::{json::new_writer, Builder};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    InitializeParams, InitializeResult, InitializedParams,
};
use tower_lsp::{LanguageServer, LspService, Server};

struct TypstLanguageService {}

#[tower_lsp::async_trait]
impl LanguageServer for TypstLanguageService {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        log::info!("initialize(): {}", log::as_serde!(params));
        Ok(InitializeResult::default())
    }

    async fn initialized(&self, _: InitializedParams) {}

    async fn shutdown(&self) -> Result<()> {
        Ok(())
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

#[tokio::main]
pub async fn main() {
    let args = Args::parse();

    let mut log_builder = Builder::with_level("info");
    if args.log_output.is_some() {
        let log_file = File::options()
            .create(true)
            .append(true)
            .open(args.log_output.clone().unwrap())
            .unwrap();
        log_builder = log_builder.with_default_writer(new_writer(log_file));
    }
    log_builder.init();

    if args.listen.is_some() {
        println!("not implemented") // TODO
    } else {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::new(|_| TypstLanguageService {});
        Server::new(stdin, stdout, socket).serve(service).await;
    };
}
