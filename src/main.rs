fn main() {
    // Lossy: a file name that is not UTF-8 must not make the process panic (hooks never fail).
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    std::process::exit(torpor::cli::run(args));
}
