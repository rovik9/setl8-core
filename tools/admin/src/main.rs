fn main() {
    setl8_admin::cli::install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut host = setl8_admin::host::RealHost;
    std::process::exit(setl8_admin::cli::run_guarded(&mut host, &args));
}
