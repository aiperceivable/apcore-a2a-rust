//! CLI entrypoint for apcore-a2a.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "apcore-a2a", version = crate::VERSION, about = "A2A protocol adapter for apcore")]
pub struct Cli {
    #[arg(short, long, default_value = "./extensions")]
    pub extensions_dir: String,

    #[arg(short, long, default_value = "apcore-a2a")]
    pub name: String,

    #[arg(long, default_value = "http://localhost:8000")]
    pub url: String,

    #[arg(short, long, default_value_t = 8000)]
    pub port: u16,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = crate::APCoreA2AConfig {
        name: cli.name.clone(),
        url: cli.url.clone(),
        ..Default::default()
    };
    let source = crate::BackendSource::from(cli.extensions_dir.as_str());

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move { crate::async_serve(source, config).await })?;
    Ok(())
}
