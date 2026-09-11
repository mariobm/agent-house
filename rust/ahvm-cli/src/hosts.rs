//! Saved SSH destinations. Credentials stay on the server; tunnels live only
//! for the foreground command. OpenSSH retains host-key and agent handling.
use crate::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Subcommand)]
pub enum Hosts {
    Add {
        name: String,
        #[arg(long)]
        ssh: String,
        #[arg(long)]
        install: bool,
        #[arg(long, default_value_t = 8080)]
        api_port: u16,
    },
    Upgrade {
        name: String,
    },
    List,
    Use {
        name: String,
    },
    Remove {
        name: String,
    },
}
#[derive(Default, Serialize, Deserialize)]
struct Config {
    default: Option<String>,
    hosts: BTreeMap<String, Host>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Host {
    ssh: String,
    api_port: u16,
}
fn directory() -> Result<PathBuf> {
    let dir = match std::env::var_os("AHVM_CONFIG_DIR") {
        Some(p) => PathBuf::from(p),
        None => {
            PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unset")?).join(".config/ahvm")
        }
    };
    fs::create_dir_all(&dir)?;
    Ok(dir)
}
fn load() -> Result<Config> {
    match fs::read(directory()?.join("hosts.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}
fn valid(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-@[]:".contains(&b))
}
fn validate(host: &Host) -> Result<()> {
    if !valid(&host.ssh) || host.api_port == 0 {
        return Err("invalid SSH destination or API port".into());
    }
    Ok(())
}
pub fn selected(name: Option<&str>, explicit_endpoint: bool) -> Result<Option<Host>> {
    if explicit_endpoint {
        if name.is_some() {
            return Err("--host and --endpoint cannot be combined".into());
        }
        return Ok(None);
    }
    let config = load()?;
    match name.or(config.default.as_deref()) {
        Some(name) => Ok(Some(
            config
                .hosts
                .get(name)
                .ok_or_else(|| format!("unknown host {name}; use ahvm host list"))?
                .clone(),
        )),
        None => Ok(None),
    }
}
pub fn run(command: Hosts, json: bool) -> Result<i32> {
    // Serialize read-modify-write across CLI processes, including first default.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory()?.join("hosts.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut config = load()?;
    match command {
        Hosts::Upgrade { name } => {
            let host = config.hosts.get(&name).ok_or("unknown host")?;
            validate(host)?;
            // Verify compatibility metadata before asking the host to upgrade.
            let catalog = crate::distribution::catalog(&crate::distribution::catalog_url())?;
            if catalog
                .server
                .get("linux-x86_64")
                .is_none_or(|a| a.guest_abi != 1 || a.state_abi != 1)
            {
                return Err("no compatible server release available".into());
            }
            return provision(host, true);
        }
        Hosts::List => {
            if json {
                println!("{}", serde_json::to_string(&config)?);
            } else {
                for (name, host) in config.hosts {
                    println!(
                        "{}\t{}{}",
                        name,
                        host.ssh,
                        if config.default.as_ref() == Some(&name) {
                            "\t(default)"
                        } else {
                            ""
                        }
                    );
                }
            }
            return Ok(0);
        }
        Hosts::Add {
            name,
            ssh,
            install,
            api_port,
        } => {
            if !valid(&name) {
                return Err("invalid host name".into());
            }
            if config.hosts.contains_key(&name) {
                return Err("host already exists; remove it before replacing it".into());
            }
            let host = Host { ssh, api_port };
            validate(&host)?;
            if install && provision(&host, false)? != 0 {
                return Err("server installation failed".into());
            }
            let connection = Connection::open(&host)?;
            crate::client::Api::new(&connection.endpoint, connection.token.clone(), 30)?.call(
                reqwest::Method::GET,
                &["sandboxes"],
                &[("limit", "1".into())],
                None,
            )?;
            if config.default.is_none() {
                config.default = Some(name.clone());
            }
            config.hosts.insert(name, host);
        }
        Hosts::Use { name } => {
            if !config.hosts.contains_key(&name) {
                return Err("unknown host".into());
            }
            config.default = Some(name);
        }
        Hosts::Remove { name } => {
            if config.hosts.remove(&name).is_none() {
                return Err("unknown host".into());
            }
            if config.default.as_ref() == Some(&name) {
                config.default = config.hosts.keys().next().cloned();
            }
        }
    }
    let dir = directory()?;
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer_pretty(&mut file, &config)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(dir.join("hosts.json"))?;
    if json {
        println!("{}", serde_json::to_string(&config)?);
    } else {
        eprintln!(
            "Host configuration saved. Default: {}",
            config.default.as_deref().unwrap_or("none")
        );
    }
    Ok(0)
}

pub struct Connection {
    child: Child,
    _directory: tempfile::TempDir,
    pub endpoint: String,
    pub token: String,
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Connection {
    pub fn open(host: &Host) -> Result<Self> {
        validate(host)?;
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("ssh");
        // ExitOnForwardFailure ensures another local process cannot capture the
        // port between selection and SSH bind; never send a token after failure.
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let child = Command::new("ssh")
            .args(["-M", "-S"])
            .arg(&socket)
            .args([
                "-T",
                "-o",
                "ControlPersist=no",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-L",
            ])
            .arg(format!("127.0.0.1:{port}:127.0.0.1:{}", host.api_port))
            .args(["--", &host.ssh])
            .arg("cat >/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;
        let mut result = Self {
            child,
            _directory: dir,
            endpoint: format!("http://127.0.0.1:{port}"),
            token: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if result.child.try_wait()?.is_some() {
                return Err("SSH tunnel failed; check SSH access and host keys".into());
            }
            if socket.exists()
                && Command::new("ssh")
                    .arg("-S")
                    .arg(&socket)
                    .args(["-O", "check", "--", &host.ssh])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?
                    .success()
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err("timed out opening SSH tunnel".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let output = Command::new("ssh").arg("-S").arg(&socket).args(["--", &host.ssh, "if test -r /etc/ahvm-rust/admin.token; then cat /etc/ahvm-rust/admin.token; else sudo -n cat /etc/ahvm-rust/admin.token; fi"]).output()?;
        if !output.status.success() {
            return Err("cannot read the server token; SSH as root or configure sudo access to /etc/ahvm-rust/admin.token".into());
        }
        let token = String::from_utf8(output.stdout)?.trim().to_owned();
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("server returned an invalid admin token".into());
        }
        result.token = token;
        Ok(result)
    }
}
/// Image administration happens on the selected host, not the client machine.
pub fn image(host: &Host, command: crate::images::Images) -> Result<i32> {
    validate(host)?;
    let args = match command {
        crate::images::Images::List => "list".into(),
        crate::images::Images::Available => "available".into(),
        crate::images::Images::Pull { name } | crate::images::Images::Default { name }
            if !valid(&name) =>
        {
            return Err("invalid image name".into())
        }
        crate::images::Images::Pull { name } => format!("pull '{name}'"),
        crate::images::Images::Default { name } => format!("default '{name}'"),
    };
    let status = Command::new("ssh")
        .args(["-T", "--", &host.ssh])
        .arg(format!("sudo -n /opt/ahvm-rust/bin/ahvm image {args}"))
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn provision(host: &Host, upgrade: bool) -> Result<i32> {
    validate(host)?;
    let script = include_str!("../../../scripts/server-bootstrap.py")
        .replace(
            "@PUBLIC_KEY@",
            include_str!("../../../packaging/keys/releases.pem"),
        )
        .replace(
            "CATALOG = 'https://images.ahvm.app/catalog.json'",
            &format!(
                "CATALOG = {}",
                serde_json::to_string(&crate::distribution::catalog_url())?
            ),
        );
    let mut child = Command::new("ssh")
        .args(["--", &host.ssh])
        .arg(if upgrade {
            "sudo -n python3 - --upgrade"
        } else {
            "sudo -n python3 -"
        })
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing SSH stdin")?
        .write_all(script.as_bytes())?;
    Ok(child.wait()?.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destinations_reject_shell_and_option_injection() {
        for value in [
            "-oProxyCommand=sh",
            "root@host;id",
            "a b",
            "x\n",
            "$(id)",
            "",
        ] {
            assert!(!valid(value));
        }
        for value in ["agent_house", "root@192.168.1.50", "root@[::1]"] {
            assert!(valid(value));
        }
    }
}
