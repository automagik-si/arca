fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(arca_janitor::cli::run(args));
}
