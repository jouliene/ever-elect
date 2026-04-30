use crate::util::default_config_path;
use anyhow::{Result, bail};
use std::path::PathBuf;

pub(crate) enum CliCommand {
    Run {
        config_path: PathBuf,
    },
    Init {
        config_path: PathBuf,
        explicit_path: bool,
    },
    Help,
}

impl CliCommand {
    pub(crate) fn parse() -> Result<Self> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        match args.as_slice() {
            [] => Ok(Self::Run {
                config_path: default_config_path(),
            }),
            [cmd] if cmd == "run" => Ok(Self::Run {
                config_path: default_config_path(),
            }),
            [cmd, path] if cmd == "run" => Ok(Self::Run {
                config_path: PathBuf::from(path),
            }),
            [cmd] if cmd == "init" => Ok(Self::Init {
                config_path: default_config_path(),
                explicit_path: false,
            }),
            [cmd, path] if cmd == "init" => Ok(Self::Init {
                config_path: PathBuf::from(path),
                explicit_path: true,
            }),
            [cmd] if cmd == "help" || cmd == "--help" || cmd == "-h" => Ok(Self::Help),
            [path] => Ok(Self::Run {
                config_path: PathBuf::from(path),
            }),
            _ => bail!("invalid arguments; use `ever-elect help`"),
        }
    }
}

pub(crate) fn print_help() {
    println!(
        "Usage:\n  ever-elect run [config]\n  ever-elect init [config]\n  ever-elect help\n\nDefault config: ~/.tycho/ever-elect.json"
    );
}
