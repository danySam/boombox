use anyhow::Result;
use boombox_core::config;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print the config file path
    Path,
    /// Create a commented starter config if one doesn't exist
    Init,
    /// Show the effective configuration
    Show,
}

impl ConfigCommand {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Path => {
                println!("{}", config::config_path()?.display());
            }
            Self::Init => {
                let (path, created) = config::Config::ensure_exists()?;
                if created {
                    println!("created {}", path.display());
                    println!();
                    println!("Next: `boombox setup` walks through creating your Spotify app and");
                    println!("signing in, and fills in the client ID for you.");
                } else {
                    println!("{} already exists", path.display());
                }
            }
            Self::Show => {
                let cfg = config::Config::load()?;
                print!("{}", toml::to_string_pretty(&cfg)?);
            }
        }
        Ok(())
    }
}
