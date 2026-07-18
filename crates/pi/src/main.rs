//! `pi` executable.

fn main() -> std::process::ExitCode {
    pi::cli::entry::run(
        std::env::args().skip(1).collect(),
        pi::cli::entry::Io::real(),
    )
}
