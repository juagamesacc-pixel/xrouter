use clap::{Parser, Subcommand};
use dialoguer::{Select, Input, Password};
use xrouter_config::{Config, load, save, config_path, cache_models_path};
use xrouter_core::{is_free, EndpointId};
use xrouter_providers::{OpenAiCompatAdapter, RawModel, ModelCache, Provider, make_provider};
use reqwest::Client;
use std::io::{IsTerminal, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

/// xrouter — a blazing-fast LLM router that load-balances chat/model requests
/// across multiple providers with quota-aware banning, tiers, and device login.
///
/// Typical flow:
///   1. xrouter wizard --web     # open the web UI at http://127.0.0.1:3001
///   2. xrouter serve            # start the router at http://127.0.0.1:3000
const AFTER_HELP: &str = "Storage paths:
  Config:        ~/.config/xrouter/config.toml (0600)
  Device tokens: ~/.config/xrouter/device_accounts.json (0600)
  Model cache:   ~/.cache/xrouter/models.json
  Tracker:       ~/.local/share/xrouter/track.bin (--track)
  Wizard UI:     http://127.0.0.1:3001 (xrouter wizard --web)
  Router:        http://127.0.0.1:3000 (xrouter serve)

Router access control:
  xrouter auth enable   # generate a router API key (Bearer token)
  xrouter auth disable  # remove the key (open access)
  xrouter auth show     # show current key or 'auth disabled'
  When a key is set, clients must send: Authorization: Bearer <key>
  (except GET /healthz). The wizard UI also manages it under Router Access.";

#[derive(Parser)]
#[command(
    name = "xrouter",
    version,
    about = "blazing-fast LLM router (load-balances providers with tiers + device login)",
    after_help = AFTER_HELP,
    after_long_help = AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the interactive setup wizard, or the web wizard UI on :3001.
    Wizard {
        /// Run the wizard as a separate web server on :3001 (not embedded in
        /// the main server hot path). Requires the separate xrouter-wizard
        /// server binary to be installed.
        #[arg(long, default_value="false")]
        web: bool,
    },
    /// Start the LLM router (default port 3000).
    Serve {
        /// TCP port to bind (default 3000).
        #[arg(long, default_value="3000")]
        port: u16,
        /// Host / interface to bind (default 127.0.0.1).
        #[arg(long)]
        host: Option<String>,
        /// Enable the /admin/bench benchmark endpoint (requires 'bench' feature).
        #[arg(long, default_value="false")]
        bench: bool,
        /// Enable per-request tracking (off-RAM unless set). Writes extremely
        /// compressed entries to ~/.local/share/xrouter/track.bin.
        #[arg(long, default_value="false")]
        track: bool,
    },
    /// Manage API keys for a provider.
    #[command(name="keys")]
    Keys {
        #[command(subcommand)]
        sub: KeysCmd,
    },
    /// List available models for a provider (optionally free-only).
    #[command(name="models")]
    Models {
        /// Only list models for this provider (default: all configured providers).
        #[arg(long)]
        provider: Option<String>,
        /// Only show free models.
        #[arg(long, default_value="false")]
        free: bool,
    },
    /// Create or list tiers (ordered model groups used for failover/balancing).
    #[command(name="tier")]
    Tier {
        #[command(subcommand)]
        sub: TierCmd,
    },
    /// Send a test request through a tier to verify it works end-to-end.
    Test {
        /// Tier name to test.
        tier: String,
        /// Allow hitting live providers (required; refuses without this flag).
        #[arg(long)]
        allow_live: bool,
        /// Router port to target (default 3000).
        #[arg(long, default_value="3000")]
        port: Option<u16>,
        /// Router base URL override (default http://127.0.0.1:3000).
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Manage device-login accounts (kiro / antigravity) via the OAuth device flow.
    #[command(name="device")]
    Device {
        #[command(subcommand)]
        sub: DeviceCmd,
    },
    /// Manage the router's own API key (Bearer token clients must present).
    #[command(name="auth")]
    Auth {
        #[command(subcommand)]
        sub: AuthCmd,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Generate a new router API key (disables open access). Printed once.
    Enable,
    /// Remove the router API key (open access, no auth required).
    Disable,
    /// Show the current router API key, or "auth disabled".
    Show,
}

#[derive(Subcommand)]
enum DeviceCmd {
    /// Perform a device login for a provider (kiro/antigravity). Prints the
    /// verification URL + user code, polls until authorization completes, and
    /// derives the account identity automatically (no manual account id).
    Login {
        /// Device provider name (kiro or antigravity).
        provider: String,
    },
    /// List stored device accounts (off-RAM token store).
    List,
    /// Remove a device account.
    Logout {
        /// Device provider name.
        #[arg(long)]
        provider: String,
        /// Account identity to remove. If omitted, all accounts for the
        /// provider are removed.
        #[arg(long)]
        identity: Option<String>,
    },
}

#[derive(Subcommand)]
enum KeysCmd {
    /// Add one or more API keys to a provider (hidden input, stored 0600).
    Add { provider: String },
}

#[derive(Subcommand)]
enum TierCmd {
    /// Add a provider/model entry to a tier (creates the tier if missing).
    Add {
        /// Tier name to create or append to.
        name: String,
        /// Provider that serves the model.
        #[arg(long)]
        provider: String,
        /// Model id to add to the tier.
        #[arg(long)]
        model: String,
        /// Reserved / unused extra spec (kept for forward-compat).
        #[arg(long)]
        more: Option<String>,
    },
    /// List all configured tiers and their entries.
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Commands::Wizard { web } => {
            if web {
                run_wizard_web().await?;
            } else {
                run_wizard().await?;
            }
        }
        Commands::Serve { port, host, bench, track } => {
            let h = host.unwrap_or_else(|| "127.0.0.1".to_string());
            let addr = format!("{}:{}", h, port);
            let cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
            let state = xrouter_server::AppState::new(cfg);
            #[cfg(feature = "bench")]
            let state = state.with_bench(bench);
            let state = state.with_track(track);
            if track {
                println!("request tracking enabled: ~/.local/share/xrouter/track.bin (GET /admin/track)");
            }
            if bench {
                #[cfg(feature = "bench")]
                {
                    println!("bench enabled: /admin/bench available");
                }
                #[cfg(not(feature = "bench"))]
                {
                    println!("bench flag set but 'bench' feature not compiled; rebuild with --features bench");
                }
            }
            // hot-reload watcher: watch config file and update balancer + config
            let cfg_path = config_path();
            let shared_cfg = state.config.clone();
            let balancer = state.balancer.clone();
            let cfg_path_clone = cfg_path.clone();
            let _watcher = match xrouter_config::watch::watch_config_file(cfg_path.clone(), shared_cfg, move |new_cfg| {
                for (prov, pcfg) in &new_cfg.providers {
                    if pcfg.enabled && !pcfg.keys.is_empty() {
                        let keys = pcfg.keys.iter().map(|k| xrouter_core::ApiKey(k.clone())).collect::<Vec<_>>();
                        balancer.update_keys(prov, keys);
                    }
                }
                tracing::info!("hot-reload applied for {}", cfg_path_clone.display());
            }) {
                Ok(w) => { println!("hot-reload enabled watching {}", cfg_path.display()); Some(w) },
                Err(e) => { eprintln!("hot-reload disabled: {}", e); None }
            };
            print_banner(&addr, track, bench);
            println!("xrouter listening on http://{}", addr);
            println!("tiers: {}", state.get_config().tiers.len());
            // graceful shutdown handled inside server
            xrouter_server::run_server(addr, state).await?;
            drop(_watcher);
        }
        Commands::Keys { sub } => match sub {
            KeysCmd::Add { provider } => {
                keys_add(provider).await?;
            }
        },
        Commands::Models { provider, free } => {
            models_list(provider, free).await?;
        }
        Commands::Tier { sub } => match sub {
            TierCmd::Add { name, provider, model, more: _ } => {
                let mut cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
                // find or create tier
                if let Some(t) = cfg.tiers.iter_mut().find(|t| t.name == name) {
                    t.entries.push(xrouter_core::tier::ModelEntry { provider: provider.clone(), model: model.clone(), is_default: t.entries.is_empty(), weight: 1, endpoint_id: EndpointId::new(&provider, &model) });
                } else {
                    let endpoint_id = EndpointId::new(&provider, &model);
                    cfg.tiers.push(xrouter_core::Tier { name: name.clone(), strict: true, default_entry: 0, entries: vec![xrouter_core::tier::ModelEntry { provider, model, is_default: true, weight: 1, endpoint_id }] });
                }
                save(&cfg)?;
                println!("tier '{}' updated", name);
            }
            TierCmd::List => {
                let cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
                for t in &cfg.tiers {
                    println!("tier: {} ({} entries)", t.name, t.entries.len());
                    for e in &t.entries { println!("  - {}:{}", e.provider, e.model); }
                }
            }
        },
        Commands::Test { tier, allow_live, port: _, base_url } => {
            run_test(tier, allow_live, base_url).await?;
        }
        Commands::Device { sub } => match sub {
            DeviceCmd::Login { provider } => {
                device_login(provider).await?;
            }
            DeviceCmd::List => device_list().await?,
            DeviceCmd::Logout { provider, identity } => device_logout(provider, identity).await?,
        },
        Commands::Auth { sub } => match sub {
            AuthCmd::Enable => auth_enable().await?,
            AuthCmd::Disable => auth_disable().await?,
            AuthCmd::Show => auth_show().await?,
        },
    }
    Ok(())
}

/// Print a premium, box-drawn startup banner with the actual host:port baked
/// into the URLs. Respects `NO_COLOR`/non-TTY by dropping ANSI colors. A
/// machine-readable `xrouter listening on ...` line is printed separately (in
/// `main`) for agents to parse.
fn print_banner(addr: &str, track: bool, bench: bool) {
    let use_color = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none();
    let v = |s: &str| if use_color { format!("\x1b[38;5;141m{}\x1b[0m", s) } else { s.to_string() };
    let c = |s: &str| if use_color { format!("\x1b[38;5;51m{}\x1b[0m", s) } else { s.to_string() };
    let e = |s: &str| if use_color { format!("\x1b[38;5;42m{}\x1b[0m", s) } else { s.to_string() };
    let bd = |s: &str| if use_color { format!("\x1b[38;5;51m{}\x1b[0m", s) } else { s.to_string() };

    let base = format!("http://{}", addr);

    // (plain, colored) line pairs. Plain is used for width/padding so ANSI
    // escape codes never throw off alignment.
    let mut plain: Vec<String> = Vec::new();
    let mut colored: Vec<String> = Vec::new();

    let p = "  ⚡ xrouter v0.1.0";
    plain.push(p.to_string());
    colored.push(format!("  {} {}", "⚡", v("xrouter v0.1.0")));

    let p = "  Router online — blazing fast, strict tiers";
    plain.push(p.to_string());
    colored.push(format!("  {}", e("Router online — blazing fast, strict tiers")));

    let p = format!("  Base URL      {}", base);
    plain.push(p.clone());
    colored.push(format!("  {}      {}", v("Base URL"), base));

    let p = "  OpenAI        POST /v1/chat/completions";
    plain.push(p.to_string());
    colored.push(format!("  {}        {} /v1/chat/completions", v("OpenAI"), c("POST")));

    let p = "                POST /v1/responses";
    plain.push(p.to_string());
    colored.push(format!("                {} /v1/responses", c("POST")));

    let p = "                GET  /v1/models";
    plain.push(p.to_string());
    colored.push(format!("                {}  /v1/models", c("GET")));

    let p = "  Anthropic     POST /v1/messages";
    plain.push(p.to_string());
    colored.push(format!("  {}     {} /v1/messages", v("Anthropic"), c("POST")));

    let p = "  Images        POST /v1/images/generations";
    plain.push(p.to_string());
    colored.push(format!("  {}        {} /v1/images/generations", v("Images"), c("POST")));

    let p = "  Health        GET  /healthz";
    plain.push(p.to_string());
    colored.push(format!("  {}        {}  /healthz", v("Health"), c("GET")));

    let p = "  Admin         /admin/tiers · /admin/stats";
    plain.push(p.to_string());
    colored.push(format!("  {}         /admin/tiers · /admin/stats", v("Admin")));

    let p = "                /admin/metrics · /admin/bench";
    plain.push(p.to_string());
    colored.push(format!("                /admin/metrics · /admin/bench"));

    let p = "  Wizard        xrouter wizard --web (:3001)";
    plain.push(p.to_string());
    colored.push(format!("  {}        xrouter wizard --web (:3001)", v("Wizard")));

    if track || bench {
        let mut flags = Vec::new();
        if track { flags.push("--track → ~/.local/share/xrouter/track.bin".to_string()); }
        if bench { flags.push("--bench".to_string()); }
        let fstr = flags.join(" · ");
        let p = format!("  Flags         {}", fstr);
        plain.push(p.clone());
        colored.push(format!("  {}         {}", v("Flags"), fstr));
    }

    let inner: usize = plain.iter().map(|s| s.chars().count()).max().unwrap_or(20);
    let border = "─".repeat(inner + 2);
    let top = bd(&format!("╭{}╮", border));
    let sep = bd(&format!("├{}┤", border));
    let bot = bd(&format!("╰{}╯", border));

    println!("{}", top);
    for i in 0..2 {
        let pad = inner - plain[i].chars().count();
        println!("{}  {}{} {}", bd("│"), colored[i], " ".repeat(pad), bd("│"));
    }
    println!("{}", sep);
    for i in 2..plain.len() {
        let pad = inner - plain[i].chars().count();
        println!("{}  {}{} {}", bd("│"), colored[i], " ".repeat(pad), bd("│"));
    }
    println!("{}", bot);
}

async fn run_wizard() -> anyhow::Result<()> {
    let mut cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    println!("xrouter wizard — configure providers & tiers");
    loop {
        let mut items = vec!["opencode-zen (built-in)".to_string(), "openrouter (built-in)".to_string(), "anthropic (optional)".to_string(), "custom openai-compatible…".to_string(), "done — finish wizard".to_string()];
        // add any custom providers
        for k in cfg.providers.keys() {
            if !["opencode-zen","openrouter","anthropic"].contains(&k.as_str()) {
                if !items.contains(&k.clone()) { items.insert(items.len()-1, k.clone()); }
            }
        }
        let sel = Select::new().with_prompt("Select provider to configure").items(&items).default(0).interact()?;
        if items[sel].starts_with("done") { break; }
        let provider_name = if items[sel].starts_with("opencode-zen") { "opencode-zen".to_string() }
        else if items[sel].starts_with("openrouter") { "openrouter".to_string() }
        else if items[sel].starts_with("anthropic") { "anthropic".to_string() }
        else if items[sel].starts_with("custom") {
            let name: String = Input::new().with_prompt("Provider id (e.g. my-provider)").interact_text()?;
            let url: String = Input::new().with_prompt("Base URL (e.g. https://api.example.com/v1)").interact_text()?;
            let kind: String = Input::new().with_prompt("Kind (openai-compat or anthropic)").default("openai-compat".into()).interact_text()?;
            cfg.providers.entry(name.clone()).or_insert(xrouter_config::ProviderConfig { kind: kind.clone(), base_url: url.clone(), enabled: true, keys: vec![], quota_ban_secs: 300, accounts: vec![] });
            // persist immediately
            save(&cfg)?;
            name
        } else { items[sel].clone() };

        // ensure provider exists
        if !cfg.providers.contains_key(&provider_name) {
            let defaults = Config::default_with_builtins();
            if let Some(def) = defaults.providers.get(&provider_name) { cfg.providers.insert(provider_name.clone(), def.clone()); }
            else { cfg.providers.insert(provider_name.clone(), xrouter_config::ProviderConfig { kind: "openai-compat".into(), base_url: format!("https://{}/v1", provider_name), enabled: true, keys: vec![], quota_ban_secs: 300, accounts: vec![] }); }
        }

        println!("Paste API key(s) for {} (empty line = stop):", provider_name);
        loop {
            let key: String = Password::new().with_prompt("API key (hidden)").allow_empty_password(true).interact()?;
            if key.trim().is_empty() { break; }
            let pcfg = cfg.providers.get_mut(&provider_name).unwrap();
            pcfg.keys.push(key.clone());
            println!("  → key {} saved (masked {})", pcfg.keys.len(), xrouter_core::ApiKey(key.clone()).masked());
            // validate with GET /models
            let client = Client::new();
            let adapter = make_provider(&pcfg.kind, pcfg.base_url.clone(), client);
            match adapter.list_models(&xrouter_core::ApiKey(key.clone())).await {
                Ok(models) => println!("    ✔ validated — {} models", models.len()),
                Err(e) => println!("    ⚠ validation failed: {} (saved anyway)", e),
            }
        }
        save(&cfg)?;
        println!("✔ {} keys stored for {}", cfg.providers.get(&provider_name).map(|p| p.keys.len()).unwrap_or(0), provider_name);

        // Model discovery
        let pcfg = cfg.providers.get(&provider_name).cloned();
        if let Some(pcfg) = pcfg {
            if pcfg.keys.is_empty() { println!("No keys — skipping model discovery"); continue; }
            let client = Client::new();
            let adapter = make_provider(&pcfg.kind, pcfg.base_url.clone(), client);
            let key = xrouter_core::ApiKey(pcfg.keys[0].clone());
            println!("Fetching models from {}…", provider_name);
            match adapter.list_models(&key).await {
                Ok(models) => {
                    println!("✔ {} models", models.len());
                    let free_models: Vec<&RawModel> = models.iter().filter(|m| is_free(&m.id)).collect();
                    println!("Showing FREE models only ({} found):", free_models.len());
                    for m in &free_models { println!("  [ ] {}", m.id); }
                    println!("Options: [a] show all, [c] add custom model manually, Enter to select free models");

                    // Simple selection: ask for tier name and model
                    let show_all: String = Input::new().with_prompt("Show all models? [y/N]").allow_empty(true).default("n".into()).interact_text()?;
                    let models_to_show = if show_all.to_lowercase().starts_with('y') { &models } else { &free_models.iter().map(|m| (*m).clone()).collect::<Vec<_>>() };
                    // Actually need to collect; simplify
                    let display_models: Vec<String> = if show_all.to_lowercase().starts_with('y') {
                        models.iter().map(|m| m.id.clone()).collect()
                    } else {
                        free_models.iter().map(|m| m.id.clone()).collect()
                    };
                    if display_models.is_empty() {
                        println!("No models to select — you can add custom model name manually");
                    } else {
                        for (i, m) in display_models.iter().enumerate() { println!("  {}. {}", i+1, m); }
                    }
                    let sel_models: String = Input::new().with_prompt("Select models (comma-separated numbers, or custom model id, or empty to skip)").allow_empty(true).interact_text()?;
                    let mut chosen: Vec<String> = Vec::new();
                    if !sel_models.trim().is_empty() {
                        // try parse numbers
                        for part in sel_models.split(',') {
                            let p = part.trim();
                            if let Ok(idx) = p.parse::<usize>() {
                                if idx >=1 && idx <= display_models.len() { chosen.push(display_models[idx-1].clone()); }
                            } else if !p.is_empty() {
                                chosen.push(p.to_string());
                            }
                        }
                    }
                    // custom add fallback
                    if chosen.is_empty() {
                        let custom: String = Input::new().with_prompt("Custom model name (or empty to skip tier creation)").allow_empty(true).interact_text()?;
                        if !custom.trim().is_empty() { chosen.push(custom.trim().to_string()); }
                    }
                    if chosen.is_empty() { println!("Skipping tier creation for {}", provider_name); continue; }

                    let alias: String = Input::new().with_prompt("Create a tier/alias name for the selection (e.g. \"fast\", \"smart\" or \"big-pickle\")").default("big-pickle".into()).interact_text()?;
                    // build tier entries
                    let entries: Vec<xrouter_core::tier::ModelEntry> = chosen.iter().enumerate().map(|(i, m)| xrouter_core::tier::ModelEntry { provider: provider_name.clone(), model: m.clone(), is_default: i==0, weight: 1, endpoint_id: EndpointId::new(&provider_name, m) }).collect();
                    // check existing tier
                    if let Some(t) = cfg.tiers.iter_mut().find(|t| t.name == alias) {
                        for e in entries { if !t.entries.iter().any(|x| x.provider==e.provider && x.model==e.model) { t.entries.push(e); } }
                        println!("✔ appended to existing tier '{}'", alias);
                    } else {
                        cfg.tiers.push(xrouter_core::Tier { name: alias.clone(), strict: true, default_entry: 0, entries });
                        println!("✔ created tier '{}'", alias);
                    }
                    // Recompute endpoint ids for the whole config before saving.
                    cfg.finalize();
                    save(&cfg)?;
                    let ask_failover: String = Input::new().with_prompt("If this provider fails, try another provider in this tier? [Y/n]").allow_empty(true).default("y".into()).interact_text()?;
                    if ask_failover.to_lowercase().starts_with('y') {
                        println!("You can re-run wizard and add same tier with another provider to enable cross-provider failover.");
                    }
                }
                Err(e) => println!("Failed to fetch models: {}", e),
            }
        }
    }
    save(&cfg)?;
    println!("Wizard finished. Config at ~/.config/xrouter/config.toml");
    println!("Run `xrouter serve` to start router.");
    Ok(())
}

/// Launch the wizard as a *separate* web server process (default :3001) so it
/// is never linked into the `xrouter-server` hot path. The web UI itself lives
/// in the separate `xrouter-wizard` crate (designer-owned); this just spawns
/// that binary. If the binary is not installed, fail with a clear message.
async fn run_wizard_web() -> anyhow::Result<()> {
    let bin = std::env::var("XROUTER_WIZARD_BIN")
        .unwrap_or_else(|_| "xrouter-wizard-server".to_string());
    println!("launching web wizard via '{}' on :3001", bin);
    match std::process::Command::new(&bin).arg("--port").arg("3001").status() {
        Ok(status) => {
            if !status.success() {
                anyhow::bail!("wizard server exited with status {}", status);
            }
            Ok(())
        }
        Err(e) => anyhow::bail!(
            "failed to launch wizard server '{}': {}. The web wizard is provided by the \
             separate xrouter-wizard crate and must be built/installed independently so it \
             is not embedded in the main server hot path.",
            bin,
            e
        ),
    }
}

async fn keys_add(provider: String) -> anyhow::Result<()> {
    let mut cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    if !cfg.providers.contains_key(&provider) {
        anyhow::bail!("unknown provider {}", provider);
    }
    println!("Paste API key(s) for {} (empty line = stop, hidden input):", provider);
    loop {
        let key: String = Password::new().with_prompt("API key").allow_empty_password(true).interact()?;
        if key.trim().is_empty() { break; }
        cfg.providers.get_mut(&provider).unwrap().keys.push(key);
    }
    save(&cfg)?;
    println!("keys updated for {}", provider);
    Ok(())
}

async fn models_list(provider: Option<String>, free_only: bool) -> anyhow::Result<()> {
    let cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    let client = Client::new();
    let cache = ModelCache::new(cache_models_path());
    let providers: Vec<String> = if let Some(p) = provider { vec![p] } else { cfg.providers.keys().cloned().collect() };
    for prov in providers {
        if let Some(pcfg) = cfg.providers.get(&prov) {
            if pcfg.keys.is_empty() { println!("{}: no keys (using cached if available)", prov);
                if let Some(cached) = cache.get_or_stale(&prov) {
                    println!("{}: {} cached models (stale)", prov, cached.len());
                    for m in cached.iter().filter(|m| !free_only || is_free(&m.id)) {
                        println!("  {} {} (cached)", m.id, if is_free(&m.id) { "[free]" } else { "" });
                    }
                }
                continue;
            }
            // try cache first if fresh
            if cache.is_fresh(&prov) {
                if let Some(cached) = cache.get(&prov) {
                    println!("{}: {} models (cached, fresh)", prov, cached.len());
                    for m in cached.iter().filter(|m| !free_only || is_free(&m.id)) {
                        println!("  {} {}", m.id, if is_free(&m.id) { "[free]" } else { "" });
                    }
                    continue;
                }
            }
            let adapter = make_provider(&pcfg.kind, pcfg.base_url.clone(), client.clone());
            let key = xrouter_core::ApiKey(pcfg.keys[0].clone());
            match adapter.list_models(&key).await {
                Ok(models) => {
                    cache.insert(prov.clone(), models.clone());
                    println!("{}: {} models", prov, models.len());
                    for m in models.iter().filter(|m| !free_only || is_free(&m.id)) {
                        println!("  {} {}", m.id, if is_free(&m.id) { "[free]" } else { "" });
                    }
                }
                Err(e) => {
                    println!("{}: error {} (trying stale cache)", prov, e);
                    if let Some(cached) = cache.get_or_stale(&prov) {
                        println!("{}: {} cached models (stale)", prov, cached.len());
                        for m in cached.iter().filter(|m| !free_only || is_free(&m.id)) {
                            println!("  {} {} (cached stale)", m.id, if is_free(&m.id) { "[free]" } else { "" });
                        }
                    }
                },
            }
        } else { println!("unknown provider {}", prov); }
    }
    Ok(())
}

// Default upstream base URLs for device providers (overridable in config).
fn default_device_base_url(provider: &str) -> String {
    match provider {
        "kiro" => "https://codewhisperer.us-east-1.amazonaws.com".to_string(),
        "antigravity" => "https://daily-cloudcode-pa.googleapis.com".to_string(),
        other => format!("https://{}.invalid/v1", other),
    }
}

async fn device_login(provider: String) -> anyhow::Result<()> {
    let prov_name = xrouter_auth::normalize_provider(&provider).to_string();
    if !xrouter_auth::DEVICE_PROVIDERS.contains(&prov_name.as_str()) {
        anyhow::bail!(
            "unsupported device provider '{}' (supported: kiro, antigravity)",
            provider
        );
    }
    if prov_name == "antigravity" {
        // Google PKCE browser flow with a local loopback callback.
        return device_login_google().await;
    }

    // kiro (and any future device-code provider): device authorization flow.
    println!("Initiating device login for '{}'…", prov_name);
    // No manual account id — identity is derived from the signed-in provider
    // account after the login completes.
    let init = xrouter_auth::initiate_device_login(&prov_name, "", "").await?;
    println!("To complete login, open:");
    println!("  {}", init.verification_uri_complete);
    println!("and enter the code: {}", init.user_code);
    println!("Polling for authorization (this may take a while)…");

    let acct = xrouter_auth::poll_device_login(&init).await?;
    persist_device_account(&provider, &prov_name, &acct)?;
    println!("✔ Logged in as '{}' for '{}'.", acct.account_id, prov_name);
    Ok(())
}

/// Antigravity (Google) login: bind a free loopback port, print the consent
/// URL, wait for the browser to redirect back with `?code=...`, then exchange
/// it for tokens and persist the account.
async fn device_login_google() -> anyhow::Result<()> {
    // Bind a free loopback port for the OAuth callback.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{}/oauth/callback", port);
    let init = xrouter_auth::build_google_auth_url(&redirect_uri);

    println!("Initiating Google (antigravity) login…");
    println!("Open this URL in your browser:");
    println!("  {}", init.auth_url);
    println!("(Waiting up to 5 minutes for the browser callback…)");

    // Accept the single callback on a worker thread; the async task enforces the
    // 5-minute timeout.
    let (tx, rx) = mpsc::channel::<(String, String)>();
    let worker = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let (code, state) = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .map(|path| {
                    let q = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                    let mut code = String::new();
                    let mut state = String::new();
                    for kv in q.split('&') {
                        if let Some((k, v)) = kv.split_once('=') {
                            if k == "code" {
                                code = url_decode(v);
                            } else if k == "state" {
                                state = url_decode(v);
                            }
                        }
                    }
                    (code, state)
                })
                .unwrap_or_default();
            let body = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
                <title>Login complete</title></head>\
                <body style=\"font-family:system-ui;background:#0f1117;color:#e6e6e6;\
                display:flex;min-height:100vh;align-items:center;justify-content:center;margin:0\">\
                <div style=\"text-align:center;padding:32px;border:1px solid #2a2f3a;border-radius:14px\">\
                Login complete — you can close this tab.</div></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            let _ = tx.send((code, state));
        }
    });

    let received = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::task::spawn_blocking(move || rx.recv()),
    )
    .await;
    let (code, state) = match received {
        Ok(Ok(Ok((code, state)))) => (code, state),
        _ => anyhow::bail!("timed out or failed waiting for the Google login callback (5 min)"),
    };
    let _ = worker.join();
    if code.is_empty() {
        anyhow::bail!("callback received without a code (authorization denied?)");
    }

    let acct = xrouter_auth::complete_google_login(&init, &code, &state).await?;
    persist_device_account("antigravity", "antigravity", &acct)?;
    println!("✔ Logged in as '{}' for 'antigravity'.", acct.account_id);
    Ok(())
}

/// Persist a freshly-logged-in device account: tokens go to the off-RAM 0600
/// device store; non-secret metadata goes to the main config.
fn persist_device_account(
    provider: &str,
    prov_name: &str,
    acct: &xrouter_auth::DeviceAccountConfig,
) -> anyhow::Result<()> {
    let mut store = xrouter_auth::DeviceStore::load()
        .unwrap_or_else(|_| xrouter_auth::DeviceStore::empty());
    store.add_account(acct.clone());
    store.save()?;

    let mut cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    let pcfg = cfg.providers.entry(provider.to_string()).or_insert_with(|| {
        xrouter_config::ProviderConfig {
            kind: provider.to_string(),
            base_url: default_device_base_url(prov_name),
            enabled: true,
            keys: vec![],
            quota_ban_secs: 300,
            accounts: vec![],
        }
    });
    if !pcfg
        .accounts
        .iter()
        .any(|a| a.account_id == acct.account_id)
    {
        pcfg.accounts.push(xrouter_config::DeviceAccountConfig {
            account_id: acct.account_id.clone(),
            display_name: acct.display_name.clone(),
            provider: prov_name.to_string(),
            access_token: String::new(),
            refresh_token: None,
            expires_at: None,
            client_id: None,
            client_secret: None,
            profile_arn: None,
        });
    }
    save(&cfg)?;
    Ok(())
}

/// Minimal URL-decode (handles `%XX` and `+`→space) for callback query params.
fn url_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(h as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

async fn device_list() -> anyhow::Result<()> {
    let store = match xrouter_auth::DeviceStore::load() {
        Ok(s) => s,
        Err(_) => {
            println!("no device accounts stored");
            return Ok(());
        }
    };
    if store.accounts.is_empty() {
        println!("no device accounts stored");
        return Ok(());
    }
    for a in &store.accounts {
        let exp = a
            .expires_at
            .map(|t| {
                let secs = t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                format!("expires={}", secs)
            })
            .unwrap_or_else(|| "expires=never".to_string());
        println!(
            "{}:{} ({}) {} refresh={}",
            a.provider,
            a.account_id,
            a.display_name.as_deref().unwrap_or(""),
            exp,
            a.refresh_token.is_some()
        );
    }
    Ok(())
}

async fn device_logout(
    provider: String,
    identity: Option<String>,
) -> anyhow::Result<()> {
    let prov_name = xrouter_auth::normalize_provider(&provider).to_string();
    let mut store =
        xrouter_auth::DeviceStore::load().unwrap_or_else(|_| xrouter_auth::DeviceStore::empty());
    let removed: Vec<String> = match &identity {
        Some(id) => {
            store.remove_account(&prov_name, id);
            vec![id.clone()]
        }
        None => {
            let ids: Vec<String> = store
                .accounts
                .iter()
                .filter(|a| a.provider == prov_name)
                .map(|a| a.account_id.clone())
                .collect();
            store.remove_accounts_for_provider(&prov_name);
            ids
        }
    };
    store.save()?;

    let mut cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    if let Some(p) = cfg.providers.get_mut(&provider) {
        p.accounts
            .retain(|a| !removed.iter().any(|id| id == &a.account_id));
    }
    save(&cfg)?;
    if let Some(id) = &identity {
        println!("✔ removed device account '{}' for '{}'", id, prov_name);
    } else {
        println!(
            "✔ removed {} device account(s) for '{}'",
            removed.len(),
            prov_name
        );
    }
    Ok(())
}

async fn run_test(tier: String, allow_live: bool, base_url_override: Option<String>) -> anyhow::Result<()> {
    if !allow_live {
        println!("refusing to hit live providers without --allow-live");
        return Ok(());
    }
    let cfg = load()?;
    let tier_cfg = cfg.tiers.iter().find(|t| t.name == tier).ok_or_else(|| anyhow::anyhow!("unknown tier {}", tier))?;
    println!("Testing tier '{}' with {} entries", tier, tier_cfg.entries.len());
    let client = Client::new();
    let base = base_url_override.unwrap_or_else(|| format!("http://127.0.0.1:3000"));
    // If router is running, test via router; else try direct? We try router endpoint first
    let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": tier,
        "messages": [{"role":"user","content":"ping"}],
        "max_tokens": 10
    });
    println!("POST {} with model={}", url, tier);
    let resp = client.post(&url).json(&body).send().await;
    match resp {
        Ok(r) => {
            let status = r.status();
            println!("status: {}", status);
            let txt = r.text().await.unwrap_or_default();
            println!("body: {}", &txt[..std::cmp::min(txt.len(), 500)]);
            if status.is_success() { println!("✔ test passed"); } else { println!("✘ upstream error"); }
        }
        Err(e) => {
            println!("request failed: {}", e);
            println!("is router running? `xrouter serve`");
            // fallback direct provider test
            for entry in tier_cfg.entries.iter() {
                if let Some(pcfg) = cfg.providers.get(&entry.provider) {
                    if pcfg.keys.is_empty() { continue; }
                    let adapter = OpenAiCompatAdapter::new(pcfg.base_url.clone(), client.clone());
                    let ctx = xrouter_providers::RequestCtx {
                        provider: entry.provider.clone(),
                        base_url: pcfg.base_url.clone(),
                        model: entry.model.clone(),
                        api_key: xrouter_core::ApiKey(pcfg.keys[0].clone()),
                        body: serde_json::json!({"model": entry.model, "messages": [{"role":"user","content":"ping"}], "max_tokens": 10}),
                        stream: false,
                    };
                    match adapter.send(&ctx).await {
                        Ok(u) => {
                            let body = u.response.bytes().await.unwrap_or_default();
                            let preview = &body[..std::cmp::min(200, body.len())];
                            println!("direct {}:{} -> {} {}", entry.provider, entry.model, u.status, String::from_utf8_lossy(preview))
                        }
                        Err(e) => println!("direct {} failed: {}", entry.provider, e),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Generate a secure router API key: `xr_` + 32 hex chars (16 random bytes).
fn gen_router_key() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let n: u128 = rng.random();
    format!("xr_{:032x}", n)
}

/// `xrouter auth enable` — generate a router API key, persist it, print once.
async fn auth_enable() -> anyhow::Result<()> {
    let mut cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    let key = gen_router_key();
    cfg.settings.api_token = Some(key.clone());
    save(&cfg)?;
    println!("Router API key enabled. This key is shown ONLY ONCE:");
    println!();
    println!("  {}", key);
    println!();
    println!("Clients must now send:  Authorization: Bearer {}", key);
    println!("(GET /healthz stays open. /admin/* and all chat/responses/image routes require the key.)");
    Ok(())
}

/// `xrouter auth disable` — remove the router API key (open access).
async fn auth_disable() -> anyhow::Result<()> {
    let mut cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    cfg.settings.api_token = None;
    save(&cfg)?;
    println!("Router API key disabled — open access (no auth required).");
    Ok(())
}

/// `xrouter auth show` — print the current key or "auth disabled".
async fn auth_show() -> anyhow::Result<()> {
    let cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
    match cfg.settings.api_token {
        Some(ref t) if !t.is_empty() => {
            println!("auth enabled — current token:");
            println!("  {}", t);
        }
        _ => println!("auth disabled (open access)"),
    }
    Ok(())
}
