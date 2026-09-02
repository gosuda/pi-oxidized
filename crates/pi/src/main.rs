//! `pi` executable.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    pi::cli::entry::run(args, pi::cli::entry::Io::real())
}
