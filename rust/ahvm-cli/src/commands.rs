use crate::{
    client::{exit_code, field, Api},
    Result,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::{Parser, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(name = "ahvm", version, about = "Manage Rust microVM sandboxes")]
pub struct Cli {
    #[arg(long, global = true, env = "AHVM_ENDPOINT")]
    endpoint: Option<String>,
    /// Select a saved host (otherwise use the default host).
    #[arg(long, global = true, env = "AHVM_HOST")]
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
    #[command(subcommand)]
    Host(crate::hosts::Hosts),
    /// Update a standalone CLI installation.
    Upgrade,
    #[command(hide = true)]
    CheckUpdates,
    #[command(subcommand)]
    Image(crate::images::Images),
    Health,
    Create {
        name: Option<String>,
        #[arg(long)]
        image: Option<String>,
        #[arg(long, default_value_t = 1)]
        cpus: u8,
        #[arg(long, default_value_t = 512)]
        memory: u32,
    },
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

pub fn run(cli: Cli) -> Result<i32> {
    if matches!(cli.command, Command::Upgrade) {
        return crate::upgrade::run();
    }
    if matches!(cli.command, Command::CheckUpdates) {
        return crate::upgrade::refresh();
    }
    crate::upgrade::notice();
    if let Command::Host(command) = cli.command {
        return crate::hosts::run(command, cli.json);
    }
    let host = crate::hosts::selected(cli.host.as_deref(), cli.endpoint.is_some())?;
    if let Command::Image(command) = cli.command {
        return match host {
            Some(host) => crate::hosts::image(&host, command),
            None => crate::images::run(command),
        };
    }
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
        .unwrap_or("http://127.0.0.1:8080");
    let api = Api::new(endpoint, token, cli.timeout)?;
    let response = match cli.command {
        Command::Host(_) | Command::Image(_) | Command::Upgrade | Command::CheckUpdates => {
            unreachable!()
        }
        Command::Health => api.call(Method::GET, &["healthz"], &[], None)?,
        Command::Create {
            name,
            image,
            cpus,
            memory,
        } => {
            let name = name.unwrap_or_else(|| {
                format!("vm-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
            });
            if image.is_some() {
                let health = api.call(Method::GET, &["healthz"], &[], None)?;
                if !health["features"]
                    .as_array()
                    .is_some_and(|f| f.iter().any(|v| v == "named-images-v1"))
                {
                    return Err("server does not support named images; upgrade it first".into());
                }
            }
            if cpus == 0 || memory < 128 {
                return Err("use at least 1 CPU and 128 MiB RAM".into());
            }
            api.call(
                Method::POST,
                &["sandboxes"],
                &[],
                Some(json!({"name":name,"cpus":cpus,"memory_mb":memory,"image":image})),
            )?
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
        Command::Shell { id } => {
            crate::session::require_terminal()?;
            let v = api.call(
                Method::POST,
                &["sandboxes", &id, "sessions"],
                &[],
                Some(json!({"argv":["/bin/sh"],"pty":true})),
            )?;
            let sid = field(&v, "session_id")?;
            eprintln!("Session {sid}; Ctrl-] detaches. Reattach: ahvm session attach {id} {sid}");
            return crate::session::attach(&api, &id, sid, 0);
        }
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
