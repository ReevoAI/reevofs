//! ReevoFS — FUSE filesystem backed by the Reevo API.

use reevofs_api::ReevoClient;
#[cfg(feature = "fuse")]
mod fs;

use clap::{Parser, Subcommand};
use log::info;

#[derive(Parser, Debug)]
#[command(name = "reevofs", about = "Mount Reevo's AgentFS as a local filesystem")]
struct Cli {
    /// Reevo API base URL
    #[arg(long, env = "REEVO_API_URL", default_value = "http://localhost:8000", global = true)]
    api_url: String,

    /// Reevo API token (optional if using user-id/org-id headers)
    #[arg(long, env = "REEVO_API_TOKEN", default_value = "", global = true)]
    token: String,

    /// Reevo user ID (x-reevo-user-id header)
    #[arg(long, env = "REEVO_USER_ID", global = true)]
    user_id: Option<String>,

    /// Reevo org ID (x-reevo-org-id header)
    #[arg(long, env = "REEVO_ORG_ID", global = true)]
    org_id: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

impl Cli {
    fn client(&self) -> ReevoClient {
        ReevoClient::with_ids(
            &self.api_url,
            &self.token,
            self.user_id.as_deref(),
            self.org_id.as_deref(),
        )
    }
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Mount the filesystem (requires macFUSE or libfuse)
    Mount {
        /// Mount point path
        mountpoint: String,

        /// Allow other users to access the mount
        #[arg(long)]
        allow_other: bool,
    },

    /// List files via the Reevo API (no FUSE required)
    Ls {
        /// Namespace (e.g. "skills")
        #[arg(short, long, default_value = "skills")]
        namespace: String,

        /// Scope (system, org, user)
        #[arg(short, long, default_value = "org")]
        scope: String,

        /// Directory path to list
        #[arg(default_value = "/")]
        path: String,
    },

    /// Read a file via the Reevo API (no FUSE required)
    Cat {
        /// Namespace (e.g. "skills")
        #[arg(short, long, default_value = "skills")]
        namespace: String,

        /// Scope (system, org, user)
        #[arg(short, long, default_value = "org")]
        scope: String,

        /// File path
        path: String,
    },

    /// Write a file via the Reevo API (no FUSE required)
    Write {
        /// Namespace (e.g. "skills")
        #[arg(short, long, default_value = "skills")]
        namespace: String,

        /// Scope (system, org, user)
        #[arg(short, long, default_value = "org")]
        scope: String,

        /// File path
        path: String,

        /// Content to write (reads from stdin if not provided)
        #[arg(short, long)]
        content: Option<String>,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    match &cli.command {
        Commands::Mount {
            mountpoint,
            allow_other,
        } => {
            #[cfg(feature = "fuse")]
            {
                use fuser::MountOption;

                info!("Mounting ReevoFS at {} (API: {})", mountpoint, cli.api_url);

                let client = cli.client();
                let filesystem = fs::ReevoFS::new(client);

                let mut config = fuser::Config::default();
                config.mount_options.push(MountOption::FSName("reevofs".to_string()));

                if *allow_other {
                    config.acl = fuser::SessionACL::All;
                }
                config.acl = fuser::SessionACL::RootAndOwner;
                config.mount_options.push(MountOption::AutoUnmount);

                fuser::mount2(filesystem, mountpoint, &config)
                    .expect("failed to mount filesystem");
            }

            #[cfg(not(feature = "fuse"))]
            {
                let _ = (mountpoint, allow_other);
                eprintln!("FUSE support not compiled. Rebuild with: cargo build --features fuse");
                eprintln!("Requires macFUSE (macOS) or libfuse (Linux) to be installed.");
                std::process::exit(1);
            }
        }

        Commands::Ls {
            namespace,
            scope,
            path,
        } => {
            let client = cli.client();
            match client.list_dir(namespace, scope, path) {
                Ok(resp) => {
                    for entry in &resp.entries {
                        if entry.is_directory {
                            println!("{}/", entry.name);
                        } else {
                            println!("{}", entry.name);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }

        Commands::Cat {
            namespace,
            scope,
            path,
        } => {
            use std::io::Write;
            let client = cli.client();
            match client.read_file(namespace, scope, path) {
                Ok(bytes) => {
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    if lock.write_all(&bytes).is_err() {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }

        Commands::Write {
            namespace,
            scope,
            path,
            content,
        } => {
            let bytes: Vec<u8> = match content {
                Some(s) => s.clone().into_bytes(),
                None => {
                    use std::io::Read;
                    let mut buf = Vec::new();
                    std::io::stdin().read_to_end(&mut buf).expect("failed to read stdin");
                    buf
                }
            };

            let client = cli.client();
            match client.write_file(namespace, scope, path, &bytes) {
                Ok(_) => info!("Written: {path}"),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
