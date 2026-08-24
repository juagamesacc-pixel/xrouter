use clap::{Parser, Subcommand};
use dialoguer::{Select, Input, Confirm, Password};
use xrouter_config::{Config, load, save, config_path, cache_models_path};
use xrouter_core::is_free;
use xrouter_providers::{OpenAiCompatAdapter, RawModel, ModelCache, Provider};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Parser)]
#[command(name="xrouter", version, about="blazing-fast LLM router")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Wizard,
    Serve {
        #[arg(long, default_value="3000")]
        port: u16,
        #[arg(long)]
        host: Option<String>,
    },
    #[command(name="keys")]
    Keys {
        #[command(subcommand)]
        sub: KeysCmd,
    },
    #[command(name="models")]
    Models {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value="false")]
        free: bool,
    },
    #[command(name="tier")]
    Tier {
        #[command(subcommand)]
        sub: TierCmd,
    },
    Test {
        tier: String,
        #[arg(long)]
        allow_live: bool,
        #[arg(long, default_value="3000")]
        port: Option<u16>,
        #[arg(long)]
        base_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum KeysCmd {
    Add { provider: String },
}

#[derive(Subcommand)]
enum TierCmd {
    Add {
        name: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        more: Option<String>,
    },
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Commands::Wizard => run_wizard().await?,
        Commands::Serve { port, host } => {
            let h = host.unwrap_or_else(|| "127.0.0.1".to_string());
            let addr = format!("{}:{}", h, port);
            let cfg = load().unwrap_or_else(|_| Config::default_with_builtins());
            let state = xrouter_server::AppState::new(cfg);
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
            println!("xrouter serving on http://{}", addr);
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
                    t.entries.push(xrouter_core::tier::ModelEntry { provider: provider.clone(), model: model.clone(), is_default: t.entries.is_empty(), weight: 1 });
                } else {
                    cfg.tiers.push(xrouter_core::Tier { name: name.clone(), strict: true, default_entry: 0, entries: vec![xrouter_core::tier::ModelEntry { provider, model, is_default: true, weight: 1 }] });
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
    }
    Ok(())
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
            cfg.providers.entry(name.clone()).or_insert(xrouter_config::ProviderConfig { kind: kind.clone(), base_url: url.clone(), enabled: true, keys: vec![], rr_cursor: 0 });
            // persist immediately
            save(&cfg)?;
            name
        } else { items[sel].clone() };

        // ensure provider exists
        if !cfg.providers.contains_key(&provider_name) {
            let defaults = Config::default_with_builtins();
            if let Some(def) = defaults.providers.get(&provider_name) { cfg.providers.insert(provider_name.clone(), def.clone()); }
            else { cfg.providers.insert(provider_name.clone(), xrouter_config::ProviderConfig { kind: "openai-compat".into(), base_url: format!("https://{}/v1", provider_name), enabled: true, keys: vec![], rr_cursor: 0 }); }
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
            let adapter = OpenAiCompatAdapter::new(pcfg.base_url.clone(), client);
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
            let adapter = OpenAiCompatAdapter::new(pcfg.base_url.clone(), client);
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
                    let entries: Vec<xrouter_core::tier::ModelEntry> = chosen.iter().enumerate().map(|(i, m)| xrouter_core::tier::ModelEntry { provider: provider_name.clone(), model: m.clone(), is_default: i==0, weight: 1 }).collect();
                    // check existing tier
                    if let Some(t) = cfg.tiers.iter_mut().find(|t| t.name == alias) {
                        for e in entries { if !t.entries.iter().any(|x| x.provider==e.provider && x.model==e.model) { t.entries.push(e); } }
                        println!("✔ appended to existing tier '{}'", alias);
                    } else {
                        cfg.tiers.push(xrouter_core::Tier { name: alias.clone(), strict: true, default_entry: 0, entries });
                        println!("✔ created tier '{}'", alias);
                    }
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
            let adapter = OpenAiCompatAdapter::new(pcfg.base_url.clone(), client.clone());
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
