#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = ahvm_volume::service::run() {
        eprintln!("ahvm-volumed: {error}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ahvm-volumed requires Linux");
    std::process::exit(1);
}
