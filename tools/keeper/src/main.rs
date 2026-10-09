fn main() {
    setl8_admin::cli::install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let deps = setl8_keeper::cli::Deps::real();
    let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| setl8_keeper::cli::execute(&args, &deps)))
        .unwrap_or_else(|_| {
            eprintln!("internal error: the keeper stopped unexpectedly (details withheld so no secret can leak)");
            70
        });
    std::process::exit(code);
}
