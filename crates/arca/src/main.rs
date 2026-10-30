//! The `arca` binary. The command logic lives in the library, so its integration tests can reach the same handlers.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match arca::run(&argv) {
        Ok(Some(value)) => println!("{value}"),
        Ok(None) => {}
        Err(arca::Failure::Tool(reply)) => {
            // A tool error is a reply, not a failure of the command: its object goes to stdout, and stderr stays empty.
            println!("{reply}");
            std::process::exit(3);
        }
        Err(failure) => {
            eprintln!("arca: {}", failure.message());
            std::process::exit(failure.code());
        }
    }
}
