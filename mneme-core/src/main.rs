use clap::{Parser, Subcommand};
use mneme_common::MnemeConfig;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "mneme-core", about = "MnemeCache god node (Mnemosyne)")]
struct Cli {
    #[arg(short, long, default_value = "/etc/mneme/mneme.toml")]
    config: String,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Create a user in users.db and exit. Does not start the node.
    ///
    /// Example:
    ///   mneme-core --config /etc/mneme/mneme.toml adduser \
    ///     --username admin --password mysecret --role admin
    Adduser {
        /// Username to create or update.
        #[arg(long)]
        username: String,
        /// Plaintext password (min 8 chars recommended).
        #[arg(long)]
        password: String,
        /// Role to assign: admin | readwrite | readonly  (default: admin)
        #[arg(long, default_value = "admin")]
        role: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Must be called before any rustls usage when multiple providers are compiled in.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mneme_core=info,mneme_common=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = MnemeConfig::from_file(&cli.config).unwrap_or_else(|e| {
        tracing::warn!("Cannot load config from {}: {e} — using defaults", cli.config);
        MnemeConfig::default()
    });

    match cli.cmd {
        // ── adduser: write users.db and exit ────────────────────────────────
        Some(Cmd::Adduser { username, password, role }) => {
            use mneme_core::auth::users::UsersDb;

            let db_path = &config.auth.users_db;
            // Ensure parent directory exists before opening.
            if let Some(parent) = std::path::Path::new(db_path).parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("create dir {}: {e}", parent.display()))?;
            }
            let db = UsersDb::open(db_path)
                .map_err(|e| anyhow::anyhow!("open users.db at {db_path}: {e}"))?;
            let uid = db.create_user(&username, &password, &role)
                .map_err(|e| anyhow::anyhow!("create_user: {e}"))?;
            println!("OK: created user '{}' (id={uid}) in {db_path}", username);
            Ok(())
        }

        // ── default: start the node ──────────────────────────────────────────
        None => {
            info!(
                node_id = %config.node.node_id,
                role    = ?config.node.role,
                client_addr = %config.client_addr(),
                "Starting Mnemosyne (mneme-core)"
            );
            mneme_core::core::mnemosyne::Mnemosyne::start(config).await
        }
    }
}
