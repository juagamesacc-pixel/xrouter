# clap 4.5.48 derive
- #[derive(Parser)] struct Cli { #[command(subcommand)] command: Commands }
- #[derive(Subcommand)] enum Commands { Serve{ port: u16 }, Wizard, ...}
