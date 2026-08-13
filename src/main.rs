mod api;
mod bottle;
mod bundle;
mod doctor;
mod env;
mod formula;
mod install;
mod postinstall;
mod relocate;
mod resolver;
mod ui;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "dram", version, about = "A wee pour of Homebrew — fast bottle installs, no Ruby")]
struct Cli {
    /// Install prefix (default: ~/.dram)
    #[arg(long, global = true)]
    prefix: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install one or more formulae (and their runtime dependencies)
    #[command(visible_aliases = ["add", "i"])]
    Install { names: Vec<String> },
    /// Show metadata for a formula
    #[command(visible_alias = "show")]
    Info { name: String },
    /// Print the resolved dependency order for a formula
    Deps { name: String },
    /// List installed kegs
    #[command(visible_alias = "ls")]
    List,
    /// Remove installed kegs, their links, and no-longer-needed deps
    #[command(visible_aliases = ["remove", "rm"])]
    Uninstall {
        names: Vec<String>,
        /// Remove even if other installed kegs depend on it (may break them)
        #[arg(long = "force-remove", short = 'f')]
        force: bool,
    },
    /// Re-download the formula index
    Update,
    /// Create or extend dram.toml for a per-project environment
    Init { names: Vec<String> },
    /// Pin the manifest's resolved versions + bottle digests into dram.lock
    Lock,
    /// Materialize the locked environment into ./.dram/bin
    Sync,
    /// Sync, then enter a subshell with the environment on PATH
    Shell,
    /// Print shell code to put the environment on PATH (for eval / direnv)
    Env,
    /// Install every formula listed in a Dramfile (--dump writes one instead)
    Bundle {
        /// Path to the file (default: ./Dramfile)
        file: Option<PathBuf>,
        /// Write the explicitly-installed formulae to the file instead
        #[arg(long)]
        dump: bool,
    },
    /// Search formulae by name or description
    #[command(visible_alias = "find")]
    Search { term: String },
    /// List installed formulae with a newer version in the index
    Outdated,
    /// Upgrade named formulae, or everything outdated
    #[command(visible_alias = "up")]
    Upgrade { names: Vec<String> },
    /// Check Cellar health: dangling links, dylib resolution, signatures
    Doctor,
}

#[derive(Clone)]
pub struct Ctx {
    pub prefix: PathBuf,
}

impl Ctx {
    fn cellar(&self) -> PathBuf {
        self.prefix.join("Cellar")
    }
    fn opt(&self) -> PathBuf {
        self.prefix.join("opt")
    }
    fn bin(&self) -> PathBuf {
        self.prefix.join("bin")
    }
    fn cache(&self) -> PathBuf {
        self.prefix.join("cache")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly on closed pipes (`dram ls | head`) like every Unix tool,
    // instead of Rust's default panic-on-EPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let cli = Cli::parse();
    let prefix = match cli.prefix {
        Some(p) => p,
        None => dirs_home()?.join(".dram"),
    };
    let ctx = Ctx { prefix };
    std::fs::create_dir_all(ctx.cellar())?;
    std::fs::create_dir_all(ctx.opt())?;
    std::fs::create_dir_all(ctx.bin())?;
    std::fs::create_dir_all(ctx.cache())?;

    match cli.command {
        Command::Update => {
            api::fetch_index(&ctx, true).await?;
            println!("{} index updated", ui::CHECK);
        }
        Command::Info { name } => {
            let index = api::fetch_index(&ctx, false).await?;
            let f = index.get(&name).with_context(|| format!("no such formula: {name}"))?;
            println!("{}: {}", f.name, f.versions.stable.as_deref().unwrap_or("?"));
            if let Some(d) = &f.desc {
                println!("  {d}");
            }
            println!("  deps: {}", if f.dependencies.is_empty() { "none".into() } else { f.dependencies.join(", ") });
            println!("  keg-only: {}", f.keg_only);
            match bottle::pick(f) {
                Ok(b) => println!("  bottle: {} (cellar: {})", b.tag, b.file.cellar),
                Err(e) => println!("  bottle: unavailable ({e})"),
            }
        }
        Command::Deps { name } => {
            let index = api::fetch_index(&ctx, false).await?;
            let plan = resolver::resolve(&index, &[name])?;
            for f in plan {
                println!("{}", f.name);
            }
        }
        Command::Install { names } => {
            if names.is_empty() {
                bail!("nothing to install");
            }
            let index = api::fetch_index(&ctx, false).await?;
            let plan = resolver::resolve(&index, &names)?;
            // Canonicalize: the user may have typed an alias (python -> python@3.x).
            let roots: Vec<String> = names
                .iter()
                .filter_map(|n| index.get(n).map(|f| f.name.clone()))
                .collect();
            install::install_all(&ctx, &plan, &roots).await?;
        }
        Command::List => {
            let cellar = ctx.cellar();
            let mut entries: Vec<_> = std::fs::read_dir(&cellar)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().to_string();
                let versions: Vec<String> = std::fs::read_dir(e.path())?
                    .filter_map(|v| v.ok())
                    .map(|v| v.file_name().to_string_lossy().to_string())
                    .collect();
                println!("{name} {}", versions.join(" "));
            }
        }
        Command::Uninstall { names, force } => {
            if names.is_empty() {
                bail!("nothing to uninstall");
            }
            install::uninstall(&ctx, &names, force)?;
        }
        Command::Init { names } => {
            env::init(&names)?;
        }
        Command::Lock => {
            env::lock(&ctx).await?;
        }
        Command::Sync => {
            env::sync(&ctx).await?;
        }
        Command::Shell => {
            env::shell(&ctx).await?;
        }
        Command::Env => {
            env::print_env()?;
        }
        Command::Bundle { file, dump } => {
            let path = file.unwrap_or_else(|| PathBuf::from(bundle::DEFAULT_FILE));
            if dump {
                bundle::dump(&ctx, &path)?;
            } else {
                bundle::install(&ctx, &path).await?;
            }
        }
        Command::Search { term } => {
            let index = api::fetch_index(&ctx, false).await?;
            let t = term.to_lowercase();
            let mut by_name = Vec::new();
            let mut by_desc = Vec::new();
            for f in index.all() {
                if f.name.to_lowercase().contains(&t)
                    || f.aliases.iter().any(|a| a.to_lowercase().contains(&t))
                {
                    by_name.push(f);
                } else if f.desc.as_deref().is_some_and(|d| d.to_lowercase().contains(&t)) {
                    by_desc.push(f);
                }
            }
            let total = by_name.len() + by_desc.len();
            for f in by_name.into_iter().chain(by_desc).take(25) {
                let installed = ctx.cellar().join(&f.name).exists();
                println!(
                    "{}{} {} {}",
                    if installed { format!("{} ", ui::CHECK) } else { "  ".into() },
                    f.name,
                    ui::dim(f.versions.stable.as_deref().unwrap_or("?")),
                    ui::dim(f.desc.as_deref().unwrap_or("")),
                );
            }
            if total > 25 {
                println!("{}", ui::dim(&format!("  … and {} more", total - 25)));
            } else if total == 0 {
                println!("no matches for '{term}'");
            }
        }
        Command::Outdated => {
            let index = api::fetch_index(&ctx, false).await?;
            let out = install::outdated_list(&ctx, &index)?;
            if out.is_empty() {
                println!("{} everything up to date", ui::CHECK);
            } else {
                for o in out {
                    println!("{} {} → {}", o.name, ui::dim(&o.installed.join("/")), o.latest);
                }
            }
        }
        Command::Upgrade { names } => {
            let index = api::fetch_index(&ctx, false).await?;
            install::upgrade(&ctx, &index, &names).await?;
        }
        Command::Doctor => {
            doctor::doctor(&ctx)?;
        }
    }
    Ok(())
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}
