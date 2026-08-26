pub mod cli;
pub mod config;
pub mod git;
pub mod model;
pub mod provider;
pub mod report;
pub mod run;
pub mod scheduler;
pub mod storage;

use anyhow::Result;
use cli::{Cli, Command};

pub async fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Providers(args) => scheduler::providers_command(args).await,
        Command::Provider(args) => scheduler::provider_command(args).await,
        Command::Doctor(args) => scheduler::doctor_command(args).await,
        Command::Review(args) => run::review_command(args).await,
        Command::Status(args) => run::status_command(args).await,
        Command::Follow(args) => run::follow_command(args).await,
        Command::Report(args) => run::report_command(args).await,
        Command::Cancel(args) => run::cancel_command(args).await,
        Command::Resume(args) => run::resume_command(args).await,
        Command::Runs(args) => run::runs_command(args).await,
        Command::Fix(args) => run::fix_command(args).await,
        Command::InstallSkill(args) => run::install_skill_command(args).await,
        Command::Internal(args) => run::internal_command(args).await,
    }
}
