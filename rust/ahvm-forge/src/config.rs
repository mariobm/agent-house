//! Startup configuration: flags beat env, env beats defaults.

#[derive(Debug, Clone)]
pub struct Config {
    pub listen_addr: String,
    pub token: String,
    /// Max captured bytes per stream (stdout/stderr/file reads).
    pub max_output_bytes: usize,
    /// Wall-clock cap per exec.
    pub exec_timeout_secs: u64,
    /// Jail root for file operations; `..` escapes are rejected.
    pub root: std::path::PathBuf,
    /// AF_VSOCK listen port (guest CID_ANY). 0 disables; otherwise forge
    /// serves vsock IN ADDITION to TCP.
    pub vsock_port: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        Self {
            listen_addr: get("AHVM_FORGE_LISTEN").unwrap_or_else(|| "127.0.0.1:10240".into()),
            token: get("AHVM_FORGE_TOKEN").unwrap_or_default(),
            max_output_bytes: get("AHVM_FORGE_MAX_OUTPUT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(8 << 20),
            exec_timeout_secs: get("AHVM_FORGE_EXEC_TIMEOUT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            root: get("AHVM_FORGE_ROOT")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "/".into()),
            vsock_port: get("AHVM_FORGE_VSOCK_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsock_port_parsing() {
        // Only this test in the binary touches this var, so no cross-test
        // env race; restore the default on the way out for hygiene.
        std::env::remove_var("AHVM_FORGE_VSOCK_PORT");
        assert_eq!(Config::from_env().vsock_port, 1024);
        std::env::set_var("AHVM_FORGE_VSOCK_PORT", "0");
        assert_eq!(Config::from_env().vsock_port, 0);
        std::env::set_var("AHVM_FORGE_VSOCK_PORT", "2048");
        assert_eq!(Config::from_env().vsock_port, 2048);
        std::env::set_var("AHVM_FORGE_VSOCK_PORT", "not-a-port");
        assert_eq!(Config::from_env().vsock_port, 1024);
        std::env::remove_var("AHVM_FORGE_VSOCK_PORT");
    }
}
