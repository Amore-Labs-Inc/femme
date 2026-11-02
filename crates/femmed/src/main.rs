//! The `femmed` binary: parses the command line and runs it ([`femmed::cli`]).

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use femmed::cli::Cli;

fn main() -> ExitCode {
    // `femmed hook` exits 0 whatever happens, a usage error or a panic included: a Claude hook
    // that exits 2 blocks the tool call and feeds its stderr to the model.
    let hook = std::env::args_os().nth(1).is_some_and(|a| a == "hook");
    if hook {
        std::panic::set_hook(Box::new(|info| {
            let _ = writeln!(
                std::io::stderr(),
                "femmed hook: {}",
                info.to_string().replace('\n', " ")
            );
            std::process::exit(0);
        }));
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if hook => {
            let _ = e.print();
            return ExitCode::SUCCESS;
        }
        Err(e) => e.exit(),
    };
    let runtime = tokio::runtime::Runtime::new().expect("femmed: cannot start the tokio runtime");
    let code = runtime.block_on(femmed::cli::run(cli));
    // tokio reads stdin on a blocking thread that cannot be cancelled: dropping the runtime would
    // wait for that read (a proxy whose daemon hung up, a hook whose agent never closed stdin).
    runtime.shutdown_background();
    code
}
