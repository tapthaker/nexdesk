use clap::Parser;
use nexdesk::cli::{Cli, Command};

#[test]
fn public_library_exposes_cli_parsing_and_dispatch() {
    let cli = Cli::try_parse_from(["nexdesk", "discover"]).expect("discover command should parse");
    assert!(matches!(cli.command, Command::Discover));

    // Async function bodies do not run until polled. Constructing and dropping
    // this future proves integration tests can reach the public dispatch API
    // without starting real discovery.
    let dispatch = nexdesk::run(cli);
    drop(dispatch);
}

#[test]
fn public_cli_parser_rejects_unknown_commands() {
    let error = Cli::try_parse_from(["nexdesk", "not-a-command"])
        .err()
        .expect("unknown commands must be rejected");
    assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
}
