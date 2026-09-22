use crate::{
    client::{exit_code, field, Api},
    Result,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::{Parser, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(name = "ahvm", version, about = "Manage Rust microVM sandboxes")]
pub struct Cli {
    #[arg(long, global = true, env = "AHVM_ENDPOINT")]
    endpoint: Option<String>,
    /// Select Cloud or a saved SSH host for this command.
    #[arg(long, global = true, env = "AHVM_CONTEXT", conflicts_with_all = ["cloud", "host", "endpoint"])]
    context: Option<String>,
    /// Use the workspace approved by ahvm login instead of a self-hosted daemon.
    #[arg(long, global = true, hide = true, conflicts_with_all = ["host", "endpoint", "token_file"])]
    cloud: bool,
    /// Reuse a cloud lifecycle request after an interrupted response.
    #[arg(long, global = true)]
    idempotency_key: Option<String>,
    /// Select a saved host (otherwise use the default host).
    #[arg(
        long,
        global = true,
        hide = true,
        env = "AHVM_HOST",
        conflicts_with = "endpoint"
    )]
    host: Option<String>,
    /// File containing a Bearer token (otherwise read AHVM_TOKEN).
    #[arg(long, global = true, env = "AHVM_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// HTTP request timeout in seconds; does not change guest exec's 300s limit.
    #[arg(long, global = true, default_value_t = 600)]
    timeout: u64,
    /// Emit API JSON, including exec results and session cursors.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Select the default connection for subsequent commands.
    Use {
        name: String,
    },
    /// Show the selected connection and its authentication status.
    Context,
    /// List Cloud and saved SSH connections.
    Contexts,
    /// Sign in to AHVM Cloud and approve workspace access in your browser.
    Login(crate::cloud::Login),
    /// Show your AHVM Cloud account and approved workspace.
    Whoami,
    /// Revoke this CLI cloud login and remove its local credentials.
    Logout,
    #[command(subcommand)]
    Host(crate::hosts::Hosts),
    /// Update a standalone CLI installation.
    Upgrade,
    /// Show AHVM licensing and upstream notices.
    License,
    #[command(hide = true)]
    CheckUpdates,
    #[command(subcommand)]
    Image(crate::images::Images),
    Health,
    /// Create a VM and open Bash when running in an interactive terminal.
    Create {
        /// Return after creation without opening a shell. JSON and pipes also skip it.
        #[arg(long)]
        no_shell: bool,
        /// Disk persistence mode; self-hosted default is local.
        #[arg(long, value_parser = ["local", "replicated"])]
        storage: Option<String>,
        name: Option<String>,
        /// Use the Ubuntu desktop image (XFCE, terminal and browser).
        #[arg(long)]
        desktop: bool,
        #[arg(long)]
        image: Option<String>,
        /// vCPUs for self-hosted VMs; Cloud sizing is administrator-managed.
        #[arg(long)]
        cpus: Option<u8>,
        /// Self-hosted RAM in MiB (default: 2048; Ubuntu desktop: 4096; Omarchy: 8192).
        #[arg(long)]
        memory: Option<u32>,
    },
    /// Open the optional native desktop viewer (desktop-enabled VMs only).
    Desktop {
        id: String,
        /// Explicit path to the optional ahvm-desktop helper.
        #[arg(long)]
        viewer: Option<PathBuf>,
    },
    /// Inspect replication or wait for remote durability (self-hosted).
    #[command(subcommand)]
    Storage(Storage),
    List,
    Get {
        id: String,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Delete {
        id: String,
    },
    /// Run argv without a shell. Use -- before guest arguments.
    Exec {
        id: String,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Create and attach a persistent PTY shell; Ctrl-] detaches.
    Shell {
        id: String,
        /// Shell executable in the guest (override for minimal images).
        #[arg(long)]
        shell: Option<String>,
    },
    #[command(subcommand)]
    Files(Files),
    #[command(subcommand)]
    Session(Session),
    #[command(subcommand)]
    Snapshot(Snapshot),
    #[command(subcommand)]
    Preview(Preview),
}

#[derive(Subcommand)]
enum Storage {
    /// Show disk mode, admitted capacity and replication health.
    Status { id: String },
    /// Wait for remote durability; stop the replicated VM first.
    Sync { id: String },
}

#[derive(Subcommand)]
enum Files {
    List {
        id: String,
        #[arg(default_value = "/")]
        path: String,
    },
    /// Download to a local file atomically, or - for stdout.
    Get {
        id: String,
        path: String,
        output: PathBuf,
    },
    /// Stream a file atomically; use - to upload stdin.
    Put {
        id: String,
        input: PathBuf,
        path: String,
    },
}
#[derive(Subcommand)]
enum Session {
    Create {
        id: String,
        #[arg(long)]
        pty: bool,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    List {
        id: String,
    },
    /// Read output once or follow until exit. --json preserves next_seq.
    Read {
        id: String,
        session: String,
        #[arg(long, default_value_t = 0)]
        from_seq: u64,
        #[arg(long)]
        follow: bool,
    },
    /// Attach interactively to a PTY session; Ctrl-] detaches without killing it.
    Attach {
        id: String,
        session: String,
        #[arg(long, default_value_t = 0)]
        from_seq: u64,
    },
    /// Send stdin bytes (bounded to 64 KiB).
    Input {
        id: String,
        session: String,
    },
    Resize {
        id: String,
        session: String,
        rows: u16,
        cols: u16,
    },
    Kill {
        id: String,
        session: String,
    },
    Delete {
        id: String,
        session: String,
    },
}
#[derive(Subcommand)]
enum Snapshot {
    Create {
        id: String,
        name: String,
    },
    Get {
        snapshot: String,
    },
    /// Delete the snapshot record; stored bundles are not garbage-collected yet.
    Delete {
        snapshot: String,
    },
    Restore {
        snapshot: String,
        name: String,
    },
}
#[derive(Subcommand)]
enum Preview {
    List {
        id: String,
    },
    Enable {
        id: String,
        port: u16,
    },
    Revoke {
        id: String,
        port: u16,
    },
    /// Issue a scoped one-hour token; --base-url also prints a browser link.
    Access {
        id: String,
        port: u16,
        #[arg(long, env = "AHVM_PREVIEW_URL")]
        base_url: Option<String>,
    },
}

fn require_storage_api(health: &Value) -> Result<()> {
    if !health["features"]
        .as_array()
        .is_some_and(|features| features.iter().any(|v| v == "replicated-storage-v1"))
    {
        return Err("server does not advertise replicated-storage-v1; upgrade the server before using storage options".into());
    }
    Ok(())
}

fn show(v: &Value) -> Result<()> {
    if !v.is_null() {
        writeln!(io::stdout(), "{}", serde_json::to_string_pretty(v)?)?;
    }
    Ok(())
}
fn limited_read(reader: impl Read, max: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(format!("input exceeds {max} bytes").into());
    }
    Ok(bytes)
}
pub fn decoded(v: &Value) -> Result<Vec<u8>> {
    Ok(B64.decode(field(v, "data_b64")?)?)
}

pub fn run(mut cli: Cli) -> Result<i32> {
    match &cli.command {
        Command::Use { name } => {
            crate::hosts::set_context(name, false)?;
            if cli.json {
                println!("{}", json!({"context": name}));
            } else {
                println!("Using {name}");
            }
            return Ok(0);
        }
        Command::Contexts => return crate::hosts::show_contexts(cli.json),
        _ => {}
    }
    let needs_context = !matches!(
        &cli.command,
        Command::Login(_)
            | Command::Whoami
            | Command::Logout
            | Command::Host(_)
            | Command::Upgrade
            | Command::CheckUpdates
            | Command::License
            | Command::Image(_)
    );
    let mut destination = None;
    if (needs_context || matches!(cli.command, Command::Image(_))) && cli.endpoint.is_none() {
        destination = if cli.cloud {
            Some("cloud".to_owned())
        } else {
            cli.context
                .clone()
                .or(cli.host.clone())
                .or(crate::hosts::default_context()?)
        };
        if needs_context
            && destination.is_none()
            && !cli.json
            && io::stdin().is_terminal()
            && io::stderr().is_terminal()
        {
            let names = crate::hosts::context_names()?;
            eprintln!("Choose a default connection: {}", names.join(", "));
            eprint!("Context: ");
            io::stderr().flush()?;
            let mut name = String::new();
            io::stdin().read_line(&mut name)?;
            let name = name.trim();
            crate::hosts::set_context(name, false)?;
            destination = Some(name.to_owned());
        }
        if needs_context && destination.is_none() {
            return Err("no default connection; run ahvm login, ahvm use cloud, or ahvm use <host>; use --endpoint for a direct daemon".into());
        }
        if let Some(name) = destination.as_deref() {
            cli.cloud = name == "cloud";
            if !cli.cloud {
                cli.host = Some(name.to_owned());
            }
        }
    }
    if cli.cloud && cli.token_file.is_some() {
        return Err(
            "Cloud uses ahvm login credentials; --token-file is for direct connections".into(),
        );
    }
    if cli.idempotency_key.is_some() && !cli.cloud {
        return Err("--idempotency-key requires the cloud context".into());
    }
    if matches!(cli.command, Command::Context) {
        if cli.cloud {
            eprintln!("Context: cloud");
            return crate::cloud::whoami(cli.json);
        }
        let name = destination
            .as_deref()
            .or(cli.endpoint.as_deref())
            .unwrap_or("none");
        if cli.endpoint.is_none() {
            crate::hosts::selected(Some(name), false)?;
        }
        if cli.json {
            println!(
                "{}",
                json!({"context": name, "authentication": "self-hosted"})
            );
        } else {
            println!("Context: {name} (self-hosted)");
        }
        return Ok(0);
    }

    if cli.cloud
        && matches!(
            &cli.command,
            Command::Create { cpus: Some(_), .. }
                | Command::Create {
                    memory: Some(_),
                    ..
                }
        )
    {
        return Err("Cloud CPU and RAM are managed by the administrator; omit --cpus and --memory (self-hosted only)".into());
    }

    if cli.cloud
        && matches!(
            &cli.command,
            Command::Storage(_)
                | Command::Create {
                    storage: Some(_),
                    ..
                }
        )
    {
        return Err("cloud storage is managed by the service; storage selection and sync are not available in Cloud yet".into());
    }
    if matches!(cli.command, Command::License) {
        print!(
            "{}\n{}\n{}",
            include_str!("../../../LICENSE"),
            include_str!("../../../NOTICE"),
            include_str!("../../../licenses/Apache-2.0.txt")
        );
        return Ok(0);
    }
    if matches!(cli.command, Command::Upgrade) {
        return crate::upgrade::run();
    }
    if matches!(cli.command, Command::CheckUpdates) {
        return crate::upgrade::refresh();
    }
    match &cli.command {
        Command::Login(options) => {
            let code = crate::cloud::login(options)?;
            if code == 0 {
                crate::hosts::set_context("cloud", true)?;
            }
            return Ok(code);
        }
        Command::Whoami => return crate::cloud::whoami(cli.json),
        Command::Logout => return crate::cloud::logout(),
        _ => {}
    }
    crate::upgrade::notice();
    if let Command::Host(command) = cli.command {
        return crate::hosts::run(command, cli.json);
    }
    if needs_context {
        eprintln!(
            "Connection: {}",
            destination
                .as_deref()
                .or(cli.endpoint.as_deref())
                .unwrap_or("none")
        );
    }
    let host = if cli.cloud {
        None
    } else {
        crate::hosts::selected(cli.host.as_deref(), cli.endpoint.is_some())?
    };
    if let Command::Image(command) = cli.command {
        if cli.cloud {
            return Err("cloud images are managed by the service".into());
        }
        return match host {
            Some(host) => crate::hosts::image(&host, command),
            None => crate::images::run(command),
        };
    }
    let cloud = if cli.cloud {
        Some(crate::cloud::connection()?)
    } else {
        None
    };
    let connection = host
        .as_ref()
        .map(crate::hosts::Connection::open)
        .transpose()?;
    let token = match connection.as_ref() {
        Some(connection) => connection.token.clone(),
        None => match cli.token_file {
            Some(path) => std::fs::read_to_string(path)?.trim().to_owned(),
            None => std::env::var("AHVM_TOKEN").unwrap_or_default(),
        },
    };
    let endpoint = connection
        .as_ref()
        .map(|c| c.endpoint.as_str())
        .or(cli.endpoint.as_deref())
        .or(cloud.as_ref().map(|c| c.0.as_str()))
        .ok_or("no endpoint selected; run ahvm use <context>")?;
    let api = match cloud {
        Some((origin, credential)) => {
            Api::new(&origin, credential, cli.timeout)?.cloud(cli.idempotency_key)?
        }
        None => Api::new(endpoint, token, cli.timeout)?,
    };
    let response = match cli.command {
        Command::Use { .. }
        | Command::Context
        | Command::Contexts
        | Command::Login(_)
        | Command::Whoami
        | Command::Logout
        | Command::Host(_)
        | Command::Image(_)
        | Command::Upgrade
        | Command::CheckUpdates
        | Command::License => {
            unreachable!()
        }
        Command::Desktop { id, viewer } => return crate::desktop::launch(&api, &id, viewer),
        Command::Health => api.call(Method::GET, &["healthz"], &[], None)?,
        Command::Storage(command) => {
            let health = api.call(Method::GET, &["healthz"], &[], None)?;
            require_storage_api(&health)?;
            match command {
                Storage::Status { id } => {
                    api.call(Method::GET, &["sandboxes", &id, "storage"], &[], None)?
                }
                Storage::Sync { id } => api.call(
                    Method::POST,
                    &["sandboxes", &id, "storage", "sync"],
                    &[],
                    None,
                )?,
            }
        }
        Command::Create {
            no_shell,
            storage,
            name,
            desktop,
            image,
            cpus,
            memory,
        } => {
            let omarchy = image.as_deref() == Some("omarchy-desktop");
            let desktop = desktop || omarchy || image.as_deref() == Some("ubuntu-desktop");
            let cpus = cpus.unwrap_or(if omarchy {
                4
            } else if desktop {
                2
            } else {
                1
            });
            let memory = memory.unwrap_or(if omarchy {
                8192
            } else if desktop {
                4096
            } else {
                2048
            });
            if cpus == 0 || memory < 128 {
                return Err("use at least 1 CPU and 128 MiB RAM".into());
            }
            let name = name.unwrap_or_else(|| {
                format!("vm-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
            });
            if desktop
                && image
                    .as_deref()
                    .is_some_and(|name| !matches!(name, "ubuntu-desktop" | "omarchy-desktop"))
            {
                return Err("desktop requires ubuntu-desktop or omarchy-desktop".into());
            }
            if desktop || image.is_some() {
                let feature = if omarchy {
                    "omarchy-desktop-v1"
                } else if desktop {
                    "desktop-v1"
                } else {
                    "named-images-v1"
                };
                let health = api.call(Method::GET, &["healthz"], &[], None)?;
                if !health["features"]
                    .as_array()
                    .is_some_and(|f| f.iter().any(|v| v == feature))
                {
                    return Err(format!("server does not advertise {feature}; enable it on a compatible server first").into());
                }
                let requested = image.as_deref().unwrap_or("ubuntu-desktop");
                let installed = health["desktop_images"][requested]
                    .as_bool()
                    .unwrap_or(!omarchy && health["desktop_image_installed"] != false);
                if desktop && !installed {
                    let name = requested;
                    let host = host.as_ref().ok_or("install ubuntu-desktop on the server with ahvm image pull ubuntu-desktop, or use a saved SSH host for automatic installation")?;
                    eprintln!("Installing {name} on the selected host (first desktop only)...");
                    if crate::hosts::pull_for_create(host, name.into())? != 0 {
                        return Err("desktop image installation failed".into());
                    }
                }
            }
            let mut body = json!({"name":name,"image":image,"desktop":desktop});
            if !cli.cloud {
                body["cpus"] = json!(cpus);
                body["memory_mb"] = json!(memory);
            }
            if let Some(storage) = storage {
                let health = api.call(Method::GET, &["healthz"], &[], None)?;
                require_storage_api(&health)?;
                body["storage_mode"] = json!(storage);
            }
            let created = api.call(Method::POST, &["sandboxes"], &[], Some(body))?;
            if !no_shell && !cli.json && io::stdin().is_terminal() && io::stdout().is_terminal() {
                let id = field(&created, "id")?;
                eprintln!("Created {id}. Opening Bash; exit leaves the VM available.");
                // First-time image preparation may outlive a Cloud access
                // token. Refresh credentials before creating the shell session.
                let result = api
                    .clone()
                    .renew_stream_auth()
                    .and_then(|api| open_shell(&api, id, None));
                if result.is_err() {
                    eprintln!("VM {id} was created and was not deleted. Use ahvm shell {id} with the same connection options to reconnect.");
                }
                return result;
            }
            created
        }
        Command::List => {
            let mut all = Vec::new();
            let mut query = vec![("limit", "100".to_owned())];
            loop {
                let v = api.call(Method::GET, &["sandboxes"], &query, None)?;
                all.extend(
                    v["sandboxes"]
                        .as_array()
                        .ok_or("missing sandboxes")?
                        .iter()
                        .cloned(),
                );
                match v["next_cursor"].as_str() {
                    Some(cursor) => {
                        if query.iter().any(|(k, v)| *k == "after" && v == cursor) {
                            return Err("non-progressing list cursor".into());
                        }
                        query = vec![("limit", "100".into()), ("after", cursor.into())];
                    }
                    None => break,
                }
            }
            if !cli.json {
                let mut out = io::stdout().lock();
                writeln!(out, "ID\tSTATE\tCPU\tMEMORY MiB")?;
                for row in all {
                    writeln!(
                        out,
                        "{}\t{}\t{}\t{}",
                        field(&row, "id")?,
                        field(&row, "state")?,
                        row["cpus"],
                        row["memory_mb"]
                    )?;
                }
                return Ok(0);
            }
            json!({"sandboxes":all})
        }
        Command::Get { id } => api.call(Method::GET, &["sandboxes", &id], &[], None)?,
        Command::Start { id } => api.call(Method::POST, &["sandboxes", &id, "start"], &[], None)?,
        Command::Stop { id } => api.call(Method::POST, &["sandboxes", &id, "stop"], &[], None)?,
        Command::Delete { id } => api.call(Method::DELETE, &["sandboxes", &id], &[], None)?,
        Command::Exec { id, argv } => {
            let v = api.call(
                Method::POST,
                &["sandboxes", &id, "exec"],
                &[],
                Some(json!({"argv":argv})),
            )?;
            if cli.json {
                show(&v)?;
            } else {
                io::stdout().write_all(field(&v, "stdout")?.as_bytes())?;
                io::stderr().write_all(field(&v, "stderr")?.as_bytes())?;
                if v["truncated"] == true {
                    eprintln!("ahvm: guest output truncated");
                }
            }
            return exit_code(&v);
        }
        Command::Shell { id, shell } => return open_shell(&api, &id, shell.as_deref()),
        Command::Files(command) => return files(&api, command),
        Command::Session(command) => return session(&api, command, cli.json),
        Command::Snapshot(command) => match command {
            Snapshot::Create { id, name } => api.call(
                Method::POST,
                &["sandboxes", &id, "snapshots"],
                &[],
                Some(json!({"name":name})),
            )?,
            Snapshot::Get { snapshot } => {
                api.call(Method::GET, &["snapshots", &snapshot], &[], None)?
            }
            Snapshot::Delete { snapshot } => {
                api.call(Method::DELETE, &["snapshots", &snapshot], &[], None)?
            }
            Snapshot::Restore { snapshot, name } => api.call(
                Method::POST,
                &["snapshots", &snapshot, "restore"],
                &[],
                Some(json!({"new_id":name})),
            )?,
        },
        Command::Preview(command) => match command {
            Preview::List { id } => {
                api.call(Method::GET, &["sandboxes", &id, "previews"], &[], None)?
            }
            Preview::Enable { id, port } => api.call(
                Method::PUT,
                &["sandboxes", &id, "previews", &port.to_string()],
                &[],
                None,
            )?,
            Preview::Revoke { id, port } => api.call(
                Method::DELETE,
                &["sandboxes", &id, "previews", &port.to_string()],
                &[],
                None,
            )?,
            Preview::Access { id, port, base_url } => {
                // Validate before rotating a working grant.
                let mut base = base_url.map(|s| reqwest::Url::parse(&s)).transpose()?;
                if let Some(url) = base.as_ref() {
                    let host = url.host_str().ok_or("preview URL needs a domain")?;
                    let local = host == "localhost" || host.ends_with(".localhost");
                    if !(url.scheme() == "https" || (local && url.scheme() == "http"))
                        || !url.username().is_empty()
                        || url.password().is_some()
                        || url.path() != "/"
                        || url.query().is_some()
                        || url.fragment().is_some()
                    {
                        return Err(
                            "preview base must be an HTTPS origin (HTTP allowed for localhost)"
                                .into(),
                        );
                    }
                }
                let mut v = api.call(
                    Method::POST,
                    &["sandboxes", &id, "previews", &port.to_string(), "access"],
                    &[],
                    None,
                )?;
                if let Some(url) = base.as_mut() {
                    let host = format!(
                        "{}.{}",
                        field(&v, "host_label")?,
                        url.host_str().ok_or("missing preview host")?
                    );
                    url.set_host(Some(&host))?;
                    url.query_pairs_mut()
                        .append_pair("ahvm_token", field(&v, "token")?);
                    v["url"] = json!(url.as_str());
                }
                v
            }
        },
    };
    show(&response)?;
    Ok(0)
}

// Image-owned entry point selects its interactive user. Legacy/custom images
// retain Bash; explicit --shell remains a direct executable override.
fn shell_argv(shell: Option<&str>) -> Vec<&str> {
    match shell {
        Some(shell) => vec![shell],
        None => vec!["/bin/sh", "-c", "if [ -x /usr/local/bin/ahvm-shell ]; then exec /usr/local/bin/ahvm-shell; else exec /bin/bash; fi"],
    }
}

fn open_shell(api: &Api, id: &str, shell: Option<&str>) -> Result<i32> {
    crate::session::require_terminal()?;
    let v = api.call(
        Method::POST,
        &["sandboxes", id, "sessions"],
        &[],
        Some(json!({"argv":shell_argv(shell),"pty":true})),
    )?;
    let sid = field(&v, "session_id")?;
    eprintln!("Session {sid}; Ctrl-] detaches. Reattach: ahvm session attach {id} {sid} (use the same connection options)");
    crate::session::attach(api, id, sid, 0)
}

fn files(api: &Api, command: Files) -> Result<i32> {
    match command {
        Files::List { id, path } => {
            let mut offset = 0;
            let mut entries = Vec::new();
            loop {
                let v = api.call(
                    Method::GET,
                    &["sandboxes", &id, "dir"],
                    &[
                        ("path", path.clone()),
                        ("offset", offset.to_string()),
                        ("limit", "256".into()),
                    ],
                    None,
                )?;
                entries.extend(
                    v["entries"]
                        .as_array()
                        .ok_or("missing entries")?
                        .iter()
                        .cloned(),
                );
                match v["next_offset"].as_u64() {
                    Some(n) if n > offset => offset = n,
                    None => break,
                    _ => return Err("non-progressing directory cursor".into()),
                }
            }
            show(&json!({"entries":entries}))?;
        }
        Files::Put { id, input, path } => {
            let (body, expected) = if input.as_os_str() == "-" {
                (reqwest::blocking::Body::new(io::stdin()), None)
            } else {
                let file = std::fs::File::open(&input)?;
                let size = file.metadata()?.len();
                (reqwest::blocking::Body::sized(file, size), Some(size))
            };
            let v = api.upload(&id, &path, body)?;
            let actual = v["bytes"].as_u64().ok_or("upload reply missing bytes")?;
            if expected.is_some_and(|size| size != actual) {
                return Err("incomplete upload".into());
            }
            show(&v)?;
        }
        Files::Get { id, path, output } => {
            let mut temp = if output.as_os_str() == "-" {
                None
            } else {
                let parent = output
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."));
                Some(tempfile::NamedTempFile::new_in(parent)?)
            };
            let mut offset = 0;
            loop {
                let v = api.call(
                    Method::GET,
                    &["sandboxes", &id, "files"],
                    &[
                        ("path", path.clone()),
                        ("offset", offset.to_string()),
                        ("limit", "65536".into()),
                    ],
                    None,
                )?;
                let bytes = decoded(&v)?;
                if let Some(f) = temp.as_mut() {
                    f.write_all(&bytes)?;
                } else {
                    io::stdout().write_all(&bytes)?;
                }
                offset += bytes.len() as u64;
                if v["eof"].as_bool().ok_or("missing eof")? {
                    break;
                }
                if bytes.is_empty() {
                    return Err("file download made no progress".into());
                }
            }
            if let Some(f) = temp {
                f.as_file().sync_all()?;
                f.persist(output)?;
            }
        }
    }
    Ok(0)
}

fn session(api: &Api, command: Session, json_output: bool) -> Result<i32> {
    let v = match command {
        Session::Create { id, pty, argv } => api.call(
            Method::POST,
            &["sandboxes", &id, "sessions"],
            &[],
            Some(json!({"argv":argv,"pty":pty})),
        )?,
        Session::List { id } => {
            api.call(Method::GET, &["sandboxes", &id, "sessions"], &[], None)?
        }
        Session::Input { id, session } => {
            let bytes = limited_read(io::stdin(), 65536)?;
            api.call(
                Method::POST,
                &["sandboxes", &id, "sessions", &session, "input"],
                &[],
                Some(json!({"data_b64":B64.encode(&bytes)})),
            )?
        }
        Session::Kill { id, session } => api.call(
            Method::POST,
            &["sandboxes", &id, "sessions", &session, "kill"],
            &[],
            None,
        )?,
        Session::Delete { id, session } => api.call(
            Method::DELETE,
            &["sandboxes", &id, "sessions", &session],
            &[],
            None,
        )?,
        Session::Resize {
            id,
            session,
            rows,
            cols,
        } => api.call(
            Method::POST,
            &["sandboxes", &id, "sessions", &session, "resize"],
            &[],
            Some(json!({"rows":rows,"cols":cols})),
        )?,
        Session::Attach {
            id,
            session,
            from_seq,
        } => return crate::session::attach(api, &id, &session, from_seq),
        Session::Read {
            id,
            session,
            mut from_seq,
            follow,
        } => loop {
            let v = api.call(
                Method::GET,
                &["sandboxes", &id, "sessions", &session, "read"],
                &[
                    ("from_seq", from_seq.to_string()),
                    ("budget_ms", "1000".into()),
                ],
                None,
            )?;
            from_seq = v["next_seq"].as_u64().ok_or("missing next_seq")?;
            if json_output {
                writeln!(io::stdout(), "{v}")?;
            } else {
                io::stdout().write_all(&decoded(&v)?)?;
                io::stdout().flush()?;
                if v["truncated"] == true {
                    eprintln!("ahvm: session scrollback truncated");
                }
            }
            if v["eof"] == true {
                return exit_code(&v);
            }
            if !follow {
                return Ok(0);
            }
        },
    };
    show(&v)?;
    Ok(0)
}
