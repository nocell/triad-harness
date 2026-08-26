use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "triad", version, about = "Multi-provider AI review harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Providers(ProvidersArgs),
    Provider(ProviderArgs),
    Doctor(DoctorArgs),
    Review(ReviewArgs),
    Status(RunIdArgs),
    Follow(FollowArgs),
    Report(RunIdArgs),
    Cancel(RunIdArgs),
    Resume(ResumeArgs),
    Runs(RunsArgs),
    Fix(FixArgs),
    InstallSkill(InstallSkillArgs),
    #[command(hide = true)]
    Internal(InternalArgs),
}

#[derive(Debug, Args)]
pub struct ProvidersArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long)]
    pub refresh: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    Enable {
        provider: String,
    },
    Disable {
        provider: String,
    },
    Login {
        provider: String,
    },
    Install {
        provider: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewArgs {
    pub target: Option<String>,
    #[arg(long, conflicts_with_all = ["commit", "uncommitted"])]
    pub base: Option<String>,
    #[arg(long, conflicts_with_all = ["base", "uncommitted"])]
    pub commit: Option<String>,
    #[arg(long, conflicts_with_all = ["base", "commit"])]
    pub uncommitted: bool,
    #[arg(long, default_value = "auto")]
    pub providers: String,
    #[arg(long, default_value = "auto")]
    pub leader: String,
    #[arg(long)]
    pub require_all: bool,
    /// Run review and reduction as a terminal CI check; no approval or fix stage is created.
    #[arg(long, conflicts_with = "detach")]
    #[serde(default)]
    pub dry_run: bool,
    #[arg(long)]
    pub detach: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long, hide = true)]
    pub run_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunIdArgs {
    pub run_id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct FollowArgs {
    pub run_id: String,
    #[arg(long, default_value_t = 2)]
    pub interval: u64,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub run_id: String,
    #[arg(long)]
    pub detach: bool,
}

#[derive(Debug, Args)]
pub struct RunsArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct FixArgs {
    pub run_id: String,
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub exclude: Vec<String>,
    #[arg(long, default_value = "auto")]
    pub leader: String,
    #[arg(long)]
    pub detach: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum SkillHost {
    Codex,
    Claude,
    Kimi,
    All,
}

#[derive(Debug, Args)]
pub struct InstallSkillArgs {
    #[arg(long, value_enum)]
    pub host: SkillHost,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct InternalArgs {
    #[command(subcommand)]
    pub command: InternalCommand,
}

#[derive(Debug, Subcommand)]
pub enum InternalCommand {
    Worker {
        request: PathBuf,
    },
    ClaudeHook {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        failure: bool,
    },
}
