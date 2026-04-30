mod cli;
mod config;
mod init;
mod runtime;
mod util;

use anyhow::Result;
use cli::{CliCommand, print_help};
use init::init;
use runtime::run;

#[tokio::main]
async fn main() -> Result<()> {
    match CliCommand::parse()? {
        CliCommand::Run { config_path } => run(config_path).await,
        CliCommand::Init {
            config_path,
            explicit_path,
        } => init(config_path, explicit_path),
        CliCommand::Help => {
            print_help();
            Ok(())
        }
    }
}
