//! Command-line interface shared by every binary in the crate.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::agent;
use crate::auth::Credentials;
use crate::bootstrap::{ConfigOverrides, Services};
use crate::config::{Config, Effort, SandboxMode};
use crate::images;
use crate::models::format_cost;
use crate::paths::AaykraPaths;
use crate::providers::all_known_models;
use crate::runtime::git_output;
use crate::session::{Session, SessionStore, user_message};
use crate::types::{ImageAttachment, OutputFormat, PermissionMode};
use crate::ui::headless::{self, HeadlessGateway, HeadlessOptions};

#[derive(Debug, Parser)]
#[command(
    name = "aaykra-code",
    version,
    about = "Aaykra Code — a fast, native, fully agentic coding agent",
    long_about = None,
    propagate_version = true,
    max_term_width = 110
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub global: GlobalArgs,
    /// Initial prompt. With no TTY (or with --print) this runs headlessly.
    #[arg(
        value_name = "PROMPT",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub prompt: Vec<String>,
}

#[derive(Debug, Args, Clone)]
pub struct GlobalArgs {
    /// Working directory for the agent (defaults to the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Explicit configuration file, merged on top of global and project files.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Model id, or `provider/model`.
    #[arg(long, short = 'm', global = true, value_name = "MODEL")]
    pub model: Option<String>,
    /// Provider name (anthropic, openai, openrouter, ollama, …).
    #[arg(long, global = true, value_name = "PROVIDER")]
    pub provider: Option<String>,
    /// Agent to run.
    #[arg(long, short = 'a', global = true, value_name = "AGENT")]
    pub agent: Option<String>,
    /// Permission mode.
    #[arg(long, global = true, value_enum, value_name = "MODE")]
    pub permission_mode: Option<PermissionMode>,
    /// Skip every permission check. Dangerous.
    #[arg(long, global = true, hide = true)]
    pub dangerously_skip_permissions: bool,
    /// Reasoning effort.
    #[arg(long, global = true, value_enum, value_name = "LEVEL")]
    pub effort: Option<Effort>,
    /// Sandbox implementation.
    #[arg(long, global = true, value_enum, value_name = "MODE")]
    pub sandbox: Option<SandboxMode>,
    /// Allow or deny network access inside the sandbox.
    #[arg(long, global = true)]
    pub network: Option<bool>,
    /// Maximum model calls per user turn.
    #[arg(long, global = true, value_name = "N")]
    pub max_turns: Option<u32>,
    /// Maximum output tokens per model call.
    #[arg(long, global = true, value_name = "N")]
    pub max_tokens: Option<u32>,
    /// Resume a session by id, short id, or name.
    #[arg(
        long,
        short = 'r',
        global = true,
        value_name = "ID|NAME",
        conflicts_with = "continue_session"
    )]
    pub resume: Option<String>,
    /// Continue the most recent session for this directory.
    #[arg(
        long = "continue",
        short = 'c',
        global = true,
        conflicts_with = "resume"
    )]
    pub continue_session: bool,
    /// When resuming, fork the session instead of appending to it.
    #[arg(long, global = true)]
    pub fork: bool,
    /// Name a new or resumed session.
    #[arg(long, global = true, value_name = "NAME")]
    pub name: Option<String>,
    /// Approve permission prompts automatically (headless CI).
    #[arg(long, global = true, alias = "auto-approve")]
    pub yes: bool,
    /// Suppress progress messages on stderr.
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,
    /// Output format for headless runs.
    #[arg(
        long = "output-format",
        alias = "format",
        global = true,
        value_enum,
        default_value = "text"
    )]
    pub output_format: OutputFormat,
    /// Run headlessly and print the result.
    #[arg(long, short = 'p', global = true)]
    pub print: bool,
    /// Attach an image (path or data URL; repeatable).
    #[arg(long, global = true, value_name = "PATH|DATA_URL")]
    pub image: Vec<String>,
    /// Media type for raw base64 --image values.
    #[arg(long, global = true, value_name = "MIME")]
    pub image_type: Option<String>,
    /// Disable the TUI even on a terminal.
    #[arg(long, global = true)]
    pub no_tui: bool,
}

impl GlobalArgs {
    fn cwd(&self) -> Result<PathBuf> {
        let cwd = match &self.cwd {
            Some(path) => path.clone(),
            None => std::env::current_dir().context("could not determine the current directory")?,
        };
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("invalid working directory: {}", cwd.display()))?;
        Ok(cwd)
    }

    fn config_overrides(&self) -> ConfigOverrides {
        let permission_mode = if self.dangerously_skip_permissions {
            Some(PermissionMode::Bypass)
        } else {
            self.permission_mode
        };
        ConfigOverrides {
            effort: self.effort,
            max_tokens: self.max_tokens,
            max_turns: self.max_turns,
            permission_mode,
            sandbox_mode: self.sandbox,
            allow_network: self.network,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one prompt headlessly (no TUI).
    Run(RunArgs),
    /// Open the interactive TUI (default when stdin/stdout are terminals).
    Tui(TuiArgs),
    /// List configured providers and models.
    Models(ModelsArgs),
    /// List, inspect, rename, or delete sessions.
    Sessions(SessionsArgs),
    /// Manage provider credentials.
    Auth(AuthArgs),
    /// Inspect MCP servers and discovery.
    Mcp(McpArgs),
    /// List or inspect agents.
    Agents(CatalogArgs),
    /// List or inspect skills.
    Skills(CatalogArgs),
    /// Start the HTTP/SSE server for editor and remote integrations.
    Serve(ServeArgs),
    /// Create configuration and project-instruction scaffolding.
    Init(InitArgs),
    /// Show git status and diffs.
    Diff(DiffArgs),
    /// Run a read-only code review of the working tree.
    Review(ReviewArgs),
    /// Diagnose configuration, credentials, tools, and sandboxing.
    Doctor(DoctorArgs),
    /// Inspect configuration files and the effective configuration.
    Config(ConfigArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Prompt. If omitted and stdin is piped, stdin is used.
    #[arg(
        value_name = "PROMPT",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub prompt: Vec<String>,
}

#[derive(Debug, Args)]
pub struct TuiArgs {
    /// Optional prompt to send immediately after the interface opens.
    #[arg(
        value_name = "PROMPT",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub prompt: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ModelsArgs {
    /// Fetch model lists from provider APIs.
    #[arg(long)]
    pub refresh: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionsArgs {
    #[command(subcommand)]
    pub action: Option<SessionsAction>,
}

#[derive(Debug, Subcommand)]
pub enum SessionsAction {
    /// List recent sessions.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show a session and its transcript.
    Show {
        reference: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete a session and its checkpoints.
    Delete {
        reference: String,
        #[arg(long)]
        yes: bool,
    },
    /// Rename a session.
    Rename { reference: String, name: String },
    /// Delete sessions older than N days.
    Cleanup {
        #[arg(default_value_t = 30)]
        days: u64,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub action: AuthAction,
}

#[derive(Debug, Subcommand)]
pub enum AuthAction {
    /// Store a provider API key in ~/.aaykra/credentials.toml.
    Set {
        provider: String,
        /// API key. If omitted, it is read from stdin.
        key: Option<String>,
    },
    /// Remove a stored API key.
    Remove { provider: String },
    /// List stored credentials (masked).
    List,
    /// Show which providers have usable credentials.
    Status,
}

#[derive(Debug, Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub action: Option<McpAction>,
}

#[derive(Debug, Subcommand)]
pub enum McpAction {
    /// Connection state for each configured server.
    Status,
    /// Connect and list tools, resources, and prompts.
    Discover {
        /// Limit discovery to one server.
        server: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct CatalogArgs {
    /// Name of the agent or skill to inspect.
    pub name: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 4096)]
    pub port: u16,
    /// Require `Authorization: Bearer <token>` on every request.
    #[arg(long, env = "AAYKRA_SERVER_TOKEN")]
    pub token: Option<String>,
    /// Allow browser clients from any origin.
    #[arg(long)]
    pub cors: bool,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Write to ~/.aaykra/config.toml instead of the project.
    #[arg(long)]
    pub global: bool,
    /// Overwrite an existing configuration file.
    #[arg(long)]
    pub force: bool,
    /// Do not create AAYKRA.md.
    #[arg(long)]
    pub no_instructions: bool,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Restrict the diff to a path.
    pub path: Option<String>,
    /// Show only the diffstat.
    #[arg(long)]
    pub stat: bool,
    /// Review staged changes instead of the working tree.
    #[arg(long)]
    pub staged: bool,
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    /// Review focus, e.g. "security" or "concurrency".
    pub focus: Vec<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the configuration files that were loaded.
    Path,
    /// Print the effective configuration (API keys redacted).
    Show,
    /// Print a commented configuration template.
    Template,
}

/// Entry point used by the binaries. Returns a process exit code.
pub async fn run() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(()) => 0,
        Err(error) => {
            if let Some(failure) = error.downcast_ref::<HeadlessFailure>() {
                headless::print_error(failure.format, &failure.message, Some(&failure.session));
            } else {
                eprintln!("error: {error:#}");
            }
            1
        }
    }
}

/// Entry point for the `aaykra` binary: `aaykra agent …` behaves like
/// `aaykra-code …`, and `aaykra code …` / `aaykra tui …` open the TUI
/// directly.
pub async fn run_aaykra_alias() -> i32 {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(arg) = args.get(1).and_then(|arg| arg.to_str()) {
        match arg {
            "agent" => {
                args.remove(1);
            }
            "code" | "tui" => {
                if args.len() == 2 {
                    args.push("tui".into());
                } else {
                    args[1] = "tui".into();
                }
            }
            _ => {}
        }
    }
    let cli = Cli::parse_from(args);
    match dispatch(cli).await {
        Ok(()) => 0,
        Err(error) => {
            if let Some(failure) = error.downcast_ref::<HeadlessFailure>() {
                headless::print_error(failure.format, &failure.message, Some(&failure.session));
            } else {
                eprintln!("error: {error:#}");
            }
            1
        }
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Run(args)) => run_command(cli.global, args).await,
        Some(Command::Tui(args)) => interactive_command(cli.global, args.prompt, true).await,
        Some(Command::Models(args)) => models_command(cli.global, args).await,
        Some(Command::Sessions(args)) => sessions_command(cli.global, args),
        Some(Command::Auth(args)) => auth_command(args),
        Some(Command::Mcp(args)) => mcp_command(cli.global, args).await,
        Some(Command::Agents(args)) => agents_command(cli.global, args),
        Some(Command::Skills(args)) => skills_command(cli.global, args),
        Some(Command::Serve(args)) => serve_command(cli.global, args).await,
        Some(Command::Init(args)) => init_command(cli.global, args),
        Some(Command::Diff(args)) => diff_command(cli.global, args).await,
        Some(Command::Review(args)) => review_command(cli.global, args).await,
        Some(Command::Doctor(args)) => doctor_command(cli.global, args).await,
        Some(Command::Config(args)) => config_command(cli.global, args),
        None => interactive_command(cli.global, cli.prompt, false).await,
    }
}

// ---------------------------------------------------------------------------
// Interactive and headless entry
// ---------------------------------------------------------------------------

async fn interactive_command(
    global: GlobalArgs,
    prompt: Vec<String>,
    force_tui: bool,
) -> Result<()> {
    let services = load_services(&global)?;
    let prompt = prompt.join(" ").trim().to_string();
    let images = load_images(&global)?;
    let interactive = force_tui
        || (!global.print
            && !global.no_tui
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal());

    if !interactive {
        let session = prepare_session(&services, &global, None)?;
        print_warnings(&services, &global);
        let prompt = if prompt.is_empty() {
            read_stdin_prompt()?
        } else {
            prompt
        };
        return execute_headless(
            services,
            session,
            prompt,
            images,
            HeadlessOptions {
                format: global.output_format,
                auto_approve: global.yes,
                show_thinking: false,
                show_tool_output: false,
                quiet: global.quiet,
            },
            global.max_turns,
        )
        .await;
    }

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("the TUI needs an interactive terminal; use `akc run …` for headless use");
    }

    // Build the session, but if it fails because nothing is configured (no
    // credentials, no model), let the in-app setup flow fix it first.
    let session = match prepare_session(&services, &global, None) {
        Ok(session) => Some(session),
        Err(error) => {
            if is_fresh_install(&services) {
                None
            } else {
                return Err(error);
            }
        }
    };
    if let Some(session) = &session
        && session.messages.is_empty()
    {
        println!(
            "Aaykra Code · {}/{} · mode {}",
            session.provider,
            session.model,
            session.permission_mode.label()
        );
    }
    let mode = if global.dangerously_skip_permissions {
        Some(PermissionMode::Bypass)
    } else {
        global.permission_mode
    };
    crate::tui::run(
        services,
        session,
        prompt,
        images,
        crate::tui::setup::SetupContext {
            config_path: global.config.clone(),
            overrides: global.config_overrides(),
            agent: global.agent.clone(),
            mode,
            name: global.name.clone(),
        },
    )
    .await
}

/// True on a first run: no config files were loaded, no credential store
/// exists, and no provider API key is set anywhere.
fn is_fresh_install(services: &Services) -> bool {
    if !services.config_files.is_empty() || services.paths.credentials().exists() {
        return false;
    }
    !services.config.enabled_providers().any(|(name, provider)| {
        provider.requires_key
            && crate::auth::resolve_api_key(&services.config, name, provider, &services.paths)
                .ok()
                .flatten()
                .is_some()
    })
}

async fn run_command(global: GlobalArgs, args: RunArgs) -> Result<()> {
    let services = load_services(&global)?;
    let session = prepare_session(&services, &global, None)?;
    print_warnings(&services, &global);
    let prompt = if args.prompt.is_empty() {
        read_stdin_prompt()?
    } else {
        args.prompt.join(" ")
    };
    let images = load_images(&global)?;
    execute_headless(
        services,
        session,
        prompt,
        images,
        HeadlessOptions {
            format: global.output_format,
            auto_approve: global.yes,
            show_thinking: false,
            show_tool_output: false,
            quiet: global.quiet,
        },
        global.max_turns,
    )
    .await
}

/// A structured headless failure so JSON/stream-json callers still receive a
/// parseable result document.
#[derive(Debug)]
struct HeadlessFailure {
    format: OutputFormat,
    message: String,
    session: Session,
}

impl std::fmt::Display for HeadlessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HeadlessFailure {}

async fn execute_headless(
    services: Services,
    session: Session,
    prompt: String,
    images: Vec<ImageAttachment>,
    options: HeadlessOptions,
    max_turns: Option<u32>,
) -> Result<()> {
    let format = options.format;
    let gateway = Arc::new(HeadlessGateway::new(options));
    let session_id = session.id.clone();
    let runtime = services.runtime(session, gateway.clone());
    let message = user_message(prompt, &images);
    match agent::run_turn(runtime.clone(), message, max_turns).await {
        Ok(outcome) => {
            if let Err(error) = runtime.save_session().await {
                eprintln!("warning: could not save session {session_id}: {error:#}");
            }
            let session = runtime.session_snapshot().await;
            headless::print_result(format, &outcome, &session);
            if format == OutputFormat::Text {
                println!();
            }
            Ok(())
        }
        Err(error) => {
            let _ = runtime.save_session().await;
            let session = runtime.session_snapshot().await;
            Err(HeadlessFailure {
                format,
                message: format!("{error:#}"),
                session,
            }
            .into())
        }
    }
}

// ---------------------------------------------------------------------------
// Session preparation
// ---------------------------------------------------------------------------

fn load_services(global: &GlobalArgs) -> Result<Services> {
    let cwd = global.cwd()?;
    Services::load_with(cwd, global.config.as_deref(), global.config_overrides())
}

fn prepare_session(
    services: &Services,
    global: &GlobalArgs,
    forced_mode: Option<PermissionMode>,
) -> Result<Session> {
    let mode = forced_mode.or(if global.dangerously_skip_permissions {
        Some(PermissionMode::Bypass)
    } else {
        global.permission_mode
    });
    let mut session = if let Some(reference) = &global.resume {
        let mut session = services
            .store
            .resolve(reference)
            .with_context(|| format!("could not resume session `{reference}`"))?;
        services.apply_overrides(
            &mut session,
            global.provider.as_deref(),
            global.model.as_deref(),
            global.agent.as_deref(),
            mode,
        )?;
        if global.fork {
            session = session.fork();
        }
        session
    } else if global.continue_session {
        let mut session = services
            .latest_session_for(&services.cwd)?
            .context("no previous session for this directory; start one without --continue")?;
        services.apply_overrides(
            &mut session,
            global.provider.as_deref(),
            global.model.as_deref(),
            global.agent.as_deref(),
            mode,
        )?;
        if global.fork {
            session = session.fork();
        }
        session
    } else {
        services.new_session(
            global.provider.as_deref(),
            global.model.as_deref(),
            global.agent.as_deref(),
            mode,
        )?
    };
    if let Some(name) = &global.name {
        session.set_name(Some(name.clone()))?;
    }
    if global.resume.is_some() || global.continue_session {
        // Resuming in a new process refreshes the repo map and builds a compact
        // handoff before the first model call.
        session.mark_context_handoff();
        // Persist immediately so a fork has its own stable ID and resumed
        // overrides are durable even if the first request fails.
        services.save(&session)?;
    }
    Ok(session)
}

fn read_stdin_prompt() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("could not read the prompt from stdin")?;
    let prompt = input.trim().to_string();
    if prompt.is_empty() {
        bail!(
            "no prompt provided; pass a prompt, use --print with piped stdin, or run without --print for the TUI"
        );
    }
    Ok(prompt)
}

fn load_images(global: &GlobalArgs) -> Result<Vec<ImageAttachment>> {
    global
        .image
        .iter()
        .map(|value| images::parse_input(value, global.image_type.as_deref()))
        .collect()
}

fn print_warnings(services: &Services, global: &GlobalArgs) {
    if global.quiet || global.output_format != OutputFormat::Text {
        return;
    }
    for warning in &services.warnings {
        eprintln!("warning: {warning}");
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

async fn models_command(global: GlobalArgs, args: ModelsArgs) -> Result<()> {
    let services = load_services(&global)?;
    let known = all_known_models(&services.config);
    let mut output = serde_json::Map::new();
    let mut text = String::new();
    for (provider, models) in known {
        let Some(config) = services.config.provider(&provider) else {
            continue;
        };
        if let Some(requested) = &global.provider
            && requested != &provider
        {
            continue;
        }
        let ready = services.provider_runtime.has_key(&provider);
        let models = if args.refresh && ready {
            match services
                .provider_runtime
                .list_remote_models(&provider)
                .await
            {
                Ok(remote) if !remote.is_empty() => remote,
                Ok(_) => models,
                Err(error) => {
                    if global.output_format == OutputFormat::Text {
                        eprintln!("warning: could not refresh {provider}: {error:#}");
                    }
                    models
                }
            }
        } else {
            models
        };
        if args.json {
            output.insert(
                provider.clone(),
                json!({
                    "ready": ready,
                    "protocol": config.protocol.label(),
                    "base_url": config.base_url,
                    "default_model": config.default_model,
                    "models": models,
                }),
            );
        } else {
            text.push_str(&format!(
                "{provider}  [{}]  {}{}\n",
                config.protocol.label(),
                if ready {
                    "ready"
                } else {
                    "missing credentials"
                },
                if config.enabled { "" } else { "  (disabled)" }
            ));
            if models.is_empty() {
                text.push_str("  (no known models; pass any model id)\n");
            }
            for model in models {
                let marker = if config.default_model.as_deref() == Some(model.as_str()) {
                    "*"
                } else {
                    " "
                };
                text.push_str(&format!("  {marker} {model}\n"));
            }
        }
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if text.is_empty() {
        println!("No providers matched. Configure one in ~/.aaykra/config.toml.");
    } else {
        print!("{text}");
        if let Some(current) = &services.config.model {
            println!("\nConfigured default: {current}");
        }
    }
    Ok(())
}

fn sessions_command(global: GlobalArgs, args: SessionsArgs) -> Result<()> {
    let services = load_services(&global)?;
    match args.action.unwrap_or(SessionsAction::List {
        limit: 20,
        json: false,
    }) {
        SessionsAction::List { limit, json } => {
            let mut summaries = services.store.list()?;
            summaries.truncate(limit);
            if json {
                println!("{}", serde_json::to_string_pretty(&summaries)?);
            } else {
                print!("{}", SessionStore::render_list(&summaries));
            }
        }
        SessionsAction::Show { reference, json } => {
            let session = services.store.resolve(&reference)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&session)?);
            } else {
                print!("{}", render_session(&session));
            }
        }
        SessionsAction::Delete { reference, yes } => {
            let session = services.store.resolve(&reference)?;
            if !yes {
                confirm(&format!(
                    "Delete session {} ({}) and its checkpoints?",
                    &session.id[..8.min(session.id.len())],
                    session.display_title()
                ))?;
            }
            services.store.delete(&session.id)?;
            println!(
                "Deleted session {}.",
                &session.id[..8.min(session.id.len())]
            );
        }
        SessionsAction::Rename { reference, name } => {
            let session = services.store.resolve(&reference)?;
            services.store.rename(&session.id, &name)?;
            println!(
                "Renamed session {} to `{name}`.",
                &session.id[..8.min(session.id.len())]
            );
        }
        SessionsAction::Cleanup { days, yes } => {
            if !yes {
                confirm(&format!(
                    "Delete sessions not updated in the last {days} days?"
                ))?;
            }
            let removed = services.store.cleanup(days)?;
            println!("Removed {removed} session(s).");
        }
    }
    Ok(())
}

fn auth_command(args: AuthArgs) -> Result<()> {
    let paths = AaykraPaths::discover()?;
    match args.action {
        AuthAction::Set { provider, key } => {
            let key = match key {
                Some(key) => key,
                None => read_secret(&format!("API key for {provider}: "))?,
            };
            if key.trim().is_empty() {
                bail!("API key is empty");
            }
            Credentials::set(&paths, &provider, &key)?;
            println!(
                "Stored credentials for `{provider}` in {} (mode 0600).",
                paths.credentials().display()
            );
        }
        AuthAction::Remove { provider } => {
            Credentials::remove(&paths, &provider)?;
            println!("Removed stored credentials for `{provider}`.");
        }
        AuthAction::List => {
            let credentials = Credentials::load(&paths)?;
            if credentials.providers.is_empty() {
                println!("No stored credentials. Set one with `akc auth set <provider>`.");
            } else {
                for (provider, entry) in &credentials.providers {
                    println!("{provider:<20} {}", mask_secret(&entry.api_key));
                }
            }
            println!("\nStore: {}", paths.credentials().display());
        }
        AuthAction::Status => {
            let services = Services::load(std::env::current_dir()?, None)?;
            let mut rows = Vec::new();
            for (name, provider) in &services.config.providers {
                if !provider.enabled {
                    continue;
                }
                let ready = services.provider_runtime.has_key(name);
                let source = if provider
                    .api_key_env
                    .as_ref()
                    .is_some_and(|env| std::env::var(env).is_ok())
                {
                    "environment"
                } else if provider.api_key.is_some() {
                    "config"
                } else if Credentials::load(&paths)?.get(name).is_some() {
                    "credentials.toml"
                } else if !provider.requires_key {
                    "not required"
                } else {
                    "missing"
                };
                rows.push((name.as_str(), ready, source, provider.protocol.label()));
            }
            for (name, ready, source, protocol) in rows {
                println!(
                    "{name:<20} {:<10} {:<16} {protocol}",
                    if ready { "ready" } else { "no key" },
                    source
                );
            }
        }
    }
    Ok(())
}

async fn mcp_command(global: GlobalArgs, args: McpArgs) -> Result<()> {
    let services = load_services(&global)?;
    match args.action.unwrap_or(McpAction::Status) {
        McpAction::Status => println!("{}", services.mcp.render_status().await),
        McpAction::Discover { server } => {
            let names = server.map(|server| vec![server]);
            let discovery = services.mcp.discover(names.as_deref()).await;
            println!("{}", crate::mcp::McpManager::render_discovery(&discovery));
        }
    }
    services.mcp.shutdown().await;
    Ok(())
}

fn agents_command(global: GlobalArgs, args: CatalogArgs) -> Result<()> {
    let services = load_services(&global)?;
    if let Some(name) = args.name {
        let Some(agent) = services.agents.get(&name) else {
            bail!(
                "unknown agent `{name}`; available: {}",
                services
                    .agents
                    .all()
                    .iter()
                    .map(|agent| agent.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        if args.json {
            println!("{}", serde_json::to_string_pretty(agent)?);
        } else {
            println!("Agent: {}", agent.name);
            println!("Source: {}", agent.source.label());
            println!("Description: {}", agent.description);
            println!(
                "Tools: {}",
                agent
                    .allowed_tools
                    .as_ref()
                    .map(|tools| tools.join(", "))
                    .unwrap_or_else(|| "all".into())
            );
            if !agent.denied_tools.is_empty() {
                println!("Denied: {}", agent.denied_tools.join(", "));
            }
            println!(
                "Mode: {}",
                agent
                    .permission_mode
                    .map(PermissionMode::label)
                    .unwrap_or("inherit")
            );
            if let Some(model) = &agent.model {
                println!("Model: {model}");
            }
            println!("\n{}", agent.system_prompt);
        }
    } else if args.json {
        let agents: Vec<_> = services.agents.all();
        println!("{}", serde_json::to_string_pretty(&agents)?);
    } else {
        println!("{}", services.agents.render_catalog());
    }
    Ok(())
}

fn skills_command(global: GlobalArgs, args: CatalogArgs) -> Result<()> {
    let services = load_services(&global)?;
    if let Some(name) = args.name {
        let name = name.trim_start_matches('/');
        let Some(skill) = services.skills.get(name) else {
            bail!(
                "unknown skill `{name}`; available: {}",
                services.skills.names().join(", ")
            );
        };
        if args.json {
            println!("{}", serde_json::to_string_pretty(skill)?);
        } else {
            println!("/{}", skill.name);
            println!("Source: {}", skill.source.label());
            println!("Description: {}", skill.description);
            println!("\n{}", skill.body);
        }
    } else if args.json {
        let skills: Vec<_> = services.skills.all();
        println!("{}", serde_json::to_string_pretty(&skills)?);
    } else {
        println!("{}", services.skills.render_catalog());
    }
    Ok(())
}

async fn serve_command(global: GlobalArgs, args: ServeArgs) -> Result<()> {
    let services = load_services(&global)?;
    let options = crate::server::ServeOptions {
        host: args.host,
        port: args.port,
        token: args.token,
        cors: args.cors,
    };
    crate::server::serve(services, options).await
}

fn init_command(global: GlobalArgs, args: InitArgs) -> Result<()> {
    let cwd = global.cwd()?;
    let paths = AaykraPaths::discover()?;
    let config_path = if args.global {
        paths.global_config()
    } else {
        cwd.join(crate::paths::APP_DIR)
            .join(crate::paths::CONFIG_FILE)
    };
    if config_path.exists() && !args.force {
        println!(
            "Configuration already exists at {} (use --force to overwrite).",
            config_path.display()
        );
    } else {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&config_path, Config::config_template())
            .with_context(|| format!("could not write {}", config_path.display()))?;
        println!("Wrote {}", config_path.display());
    }
    if !args.global {
        for dir in [
            crate::paths::project_agent_dir(&cwd),
            crate::paths::project_skills_dir(&cwd),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        if !args.no_instructions {
            let instructions = cwd.join("AAYKRA.md");
            if instructions.exists() {
                println!("{} already exists; left unchanged.", instructions.display());
            } else {
                std::fs::write(&instructions, INITIAL_INSTRUCTIONS)
                    .with_context(|| format!("could not write {}", instructions.display()))?;
                println!("Wrote {}", instructions.display());
            }
        }
    }
    println!(
        "\nNext steps:\n  akc auth set anthropic      # store a key (or set ANTHROPIC_API_KEY)\n  akc doctor                  # verify your setup\n  akc \"explain this codebase\"  # start working"
    );
    Ok(())
}

const INITIAL_INSTRUCTIONS: &str = "# AAYKRA.md

Project guidance for Aaykra Code and other coding agents. Keep this concise and
specific; update it as the project evolves.

## Overview

Describe what this project does and who it is for.

## Build, test, lint

```bash
# replace with the real commands
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

## Layout

- `src/` — main implementation
- `tests/` — integration tests

## Conventions

- Follow the formatting and naming style already used in each module.
- Prefer small, focused changes with tests.

## Gotchas

- Add environment variables, generated files, and slow test suites here.
";

async fn diff_command(global: GlobalArgs, args: DiffArgs) -> Result<()> {
    let services = load_services(&global)?;
    let cwd = services.cwd.clone();
    let status = git_output(&cwd, &["status", "--short"]).await?;
    if !status.is_empty() {
        println!("{status}\n");
    }
    let mut command: Vec<&str> = vec!["diff", "--no-color", "--no-ext-diff"];
    if args.staged {
        command.push("--cached");
    }
    if args.stat {
        command.push("--stat");
    }
    if let Some(path) = &args.path {
        command.push("--");
        command.push(path);
    }
    let diff = git_output(&cwd, &command).await?;
    if diff.is_empty() {
        println!("No changes.");
    } else {
        println!("{diff}");
    }
    Ok(())
}

async fn review_command(global: GlobalArgs, args: ReviewArgs) -> Result<()> {
    let services = load_services(&global)?;
    let cwd = services.cwd.clone();
    let session = prepare_session(&services, &global, Some(PermissionMode::Plan))?;
    let status = git_output(&cwd, &["status", "--short"])
        .await
        .unwrap_or_default();
    let stat = git_output(&cwd, &["diff", "--stat"])
        .await
        .unwrap_or_default();
    let stat_staged = git_output(&cwd, &["diff", "--cached", "--stat"])
        .await
        .unwrap_or_default();
    if status.is_empty() {
        bail!("working tree is clean; nothing to review");
    }
    let focus = if args.focus.is_empty() {
        "Prioritize correctness bugs, security issues, regressions, and missing tests.".to_string()
    } else {
        format!("Focus on: {}", args.focus.join(" "))
    };
    let prompt = format!(
        "Review the current working-tree changes in this repository. {focus}\n\n\
         Inspect the diff with the Git tool and read the relevant files for context. Report findings ordered by severity \
         with file:line references, explain the concrete failure mode, and suggest a minimal fix. Call out missing tests \
         and risky permission or dependency changes. Do not modify files.\n\n\
         Git status:\n{status}\n\nUnstaged diff stat:\n{stat}\n\nStaged diff stat:\n{stat_staged}"
    );
    print_warnings(&services, &global);
    execute_headless(
        services,
        session,
        prompt,
        Vec::new(),
        HeadlessOptions {
            format: global.output_format,
            auto_approve: global.yes,
            show_thinking: false,
            show_tool_output: false,
            quiet: global.quiet,
        },
        global.max_turns,
    )
    .await
}

async fn doctor_command(global: GlobalArgs, args: DoctorArgs) -> Result<()> {
    let cwd = global.cwd()?;
    let mut report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cwd": cwd,
    });
    let services = match Services::load_with(
        cwd.clone(),
        global.config.as_deref(),
        global.config_overrides(),
    ) {
        Ok(services) => {
            report["config_ok"] = json!(true);
            report["config_files"] = json!(services.config_files.clone());
            report["warnings"] = json!(services.warnings.clone());
            report["sessions"] = json!(services.store.list().map(|list| list.len()).unwrap_or(0));
            let providers = services
                .config
                .providers
                .iter()
                .filter(|(_, provider)| provider.enabled)
                .map(|(name, provider)| {
                    json!({
                        "name": name,
                        "protocol": provider.protocol.label(),
                        "base_url": provider.base_url,
                        "ready": services.provider_runtime.has_key(name),
                        "requires_key": provider.requires_key,
                    })
                })
                .collect::<Vec<_>>();
            report["providers"] = json!(providers);
            report["mcp_servers"] = json!(services.mcp.configured_names());
            report["lsp_servers"] = json!(services.config.lsp.keys().collect::<Vec<_>>());
            let lsp_commands = services
                .config
                .lsp
                .iter()
                .map(|(name, lsp)| {
                    json!({
                        "name": name,
                        "command": lsp.command,
                        "found": command_exists(&lsp.command),
                        "enabled": lsp.enabled,
                    })
                })
                .collect::<Vec<_>>();
            report["lsp"] = json!(lsp_commands);
            Some(services)
        }
        Err(error) => {
            report["config_ok"] = json!(false);
            report["config_error"] = json!(format!("{error:#}"));
            None
        }
    };
    let git = git_output(&cwd, &["rev-parse", "--show-toplevel"]).await;
    report["git_repo"] = json!(git.is_ok());
    if let Ok(root) = &git {
        report["git_root"] = json!(root);
    }
    report["git_version"] = json!(command_version("git"));
    report["bubblewrap"] = json!(command_version("bwrap"));
    report["ripgrep"] = json!(command_version("rg"));
    let paths = AaykraPaths::discover()?;
    report["aaykra_home"] = json!(paths.home.clone());
    report["credentials_file"] = json!(paths.credentials());
    report["credentials_present"] = json!(paths.credentials().is_file());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("Aaykra Code {}", env!("CARGO_PKG_VERSION"));
    println!(
        "Platform: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("Directory: {}", cwd.display());
    println!("Aaykra home: {}", paths.home.display());
    if let Some(services) = &services {
        if services.config_files.is_empty() {
            println!("Config: built-in defaults (no files loaded)");
        } else {
            println!("Config:");
            for file in &services.config_files {
                println!("  {}", file.display());
            }
        }
        for warning in &services.warnings {
            println!("  warning: {warning}");
        }
        println!("Providers:");
        for (name, provider) in services.config.enabled_providers() {
            println!(
                "  {name:<18} {:<10} {:<10} {}",
                provider.protocol.label(),
                if services.provider_runtime.has_key(name) {
                    "ready"
                } else {
                    "no key"
                },
                provider.base_url
            );
        }
        println!(
            "MCP: {} configured{}",
            services.mcp.configured_names().len(),
            if services.mcp.configured_names().is_empty() {
                String::new()
            } else {
                format!(" ({})", services.mcp.configured_names().join(", "))
            }
        );
        println!("LSP:");
        if services.config.lsp.is_empty() {
            println!(
                "  (none configured; common languages auto-detect when servers are installed)"
            );
        } else {
            for (name, lsp) in &services.config.lsp {
                println!(
                    "  {name:<18} {} {}",
                    if command_exists(&lsp.command) {
                        "found"
                    } else {
                        "missing"
                    },
                    lsp.command
                );
            }
        }
    } else {
        println!("Config: FAILED TO LOAD");
        if let Some(error) = report.get("config_error").and_then(|value| value.as_str()) {
            println!("  {error}");
        }
    }
    println!(
        "Git: {}",
        match &git {
            Ok(root) => format!(
                "repository at {root} ({})",
                command_version("git").unwrap_or_else(|| "unknown version".into())
            ),
            Err(_) => "not a git repository".into(),
        }
    );
    println!(
        "Sandbox: {} ({})",
        if command_exists("bwrap") {
            "bubblewrap available"
        } else if cfg!(target_os = "macos") {
            "seatbelt built in"
        } else {
            "process-level only; install bubblewrap for filesystem isolation"
        },
        if global.sandbox.is_some() || global.network.is_some() {
            "CLI override active"
        } else {
            "config default"
        }
    );
    println!("Sessions: {}", paths.sessions.display());
    Ok(())
}

fn config_command(global: GlobalArgs, args: ConfigArgs) -> Result<()> {
    match args.action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Path => {
            let cwd = global.cwd()?;
            let loaded = Config::load(&cwd, global.config.as_deref())?;
            if loaded.files.is_empty() {
                println!("No configuration files loaded; using built-in defaults.");
            } else {
                for file in loaded.files {
                    println!("{}", file.display());
                }
            }
        }
        ConfigAction::Show => {
            let services = load_services(&global)?;
            let mut config = (*services.config).clone();
            for provider in config.providers.values_mut() {
                if provider.api_key.is_some() {
                    provider.api_key = Some("<redacted>".into());
                }
            }
            let body = toml::to_string_pretty(&config)
                .context("could not serialize effective configuration")?;
            println!("{body}");
        }
        ConfigAction::Template => {
            print!("{}", Config::config_template());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn render_session(session: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Session {} — {}\n",
        session.id,
        session.display_title()
    ));
    if let Some(name) = &session.name {
        out.push_str(&format!("Name: {name}\n"));
    }
    out.push_str(&format!(
        "Model: {}/{}\nAgent: {}\nMode: {}\nDirectory: {}\nCreated: {}\nUpdated: {}\nTurns: {} · Messages: {} · Cost: {}\n",
        session.provider,
        session.model,
        session.agent,
        session.permission_mode.label(),
        session.cwd.display(),
        session.created_at.format("%Y-%m-%d %H:%M"),
        session.updated_at.format("%Y-%m-%d %H:%M"),
        session.turns.len(),
        session.messages.len(),
        format_cost(session.cost_usd),
    ));
    if let Some(parent) = &session.parent_id {
        out.push_str(&format!("Forked from: {parent}\n"));
    }
    if !session.files_changed.is_empty() {
        out.push_str(&format!(
            "Files changed: {}\n",
            session.files_changed.join(", ")
        ));
    }
    out.push_str("\nTranscript:\n");
    for message in &session.messages {
        out.push_str(&format!("\n[{}]\n", message.role));
        for block in &message.content {
            match block {
                crate::types::ContentBlock::Text { text } => {
                    out.push_str(text);
                    out.push('\n');
                }
                crate::types::ContentBlock::Thinking { thinking, .. } => {
                    out.push_str(&format!("(thinking) {}\n", truncate_text(thinking, 500)));
                }
                crate::types::ContentBlock::ToolUse { name, input, .. } => {
                    out.push_str(&format!(
                        "→ {name} {}\n",
                        truncate_text(&input.to_string(), 500)
                    ));
                }
                crate::types::ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    out.push_str(&format!(
                        "{} {}\n",
                        if *is_error { "✗" } else { "✓" },
                        truncate_text(content, 1000)
                    ));
                }
                crate::types::ContentBlock::Image { media_type, .. } => {
                    out.push_str(&format!("[image {media_type}]\n"));
                }
            }
        }
    }
    out
}

fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!(
        "{}…",
        text.chars().take(max.saturating_sub(1)).collect::<String>()
    )
}

fn mask_secret(secret: &str) -> String {
    let count = secret.chars().count();
    if count <= 8 {
        "••••".into()
    } else {
        let start: String = secret.chars().take(4).collect();
        let end: String = secret.chars().skip(count - 4).collect();
        format!("{start}••••{end}")
    }
}

fn read_secret(prompt: &str) -> Result<String> {
    use std::io::Write as _;
    if std::io::stdin().is_terminal() {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("could not read the API key")?;
    Ok(line.trim().to_string())
}

fn confirm(question: &str) -> Result<()> {
    use std::io::Write as _;
    if !std::io::stdin().is_terminal() {
        bail!("{question} Refusing in non-interactive mode; pass --yes to confirm.");
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("aborted")
    }
}

fn command_exists(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(command);
            candidate.is_file()
        })
    })
}

fn command_version(command: &str) -> Option<String> {
    let output = std::process::Command::new(command)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
    )
}
