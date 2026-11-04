use caim::error::{Error, FAILED, USAGE};
use caim::{completions, digests, tasks, waits};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let status = if args.first().is_some_and(|mode| digests::MODES.contains(&mode.as_str())) {
        match digests::dispatch(&args) {
            Ok(status) => status,
            Err(Error::Usage(message)) => {
                eprintln!("caim {}: {message}", args[0]);
                USAGE
            }
            Err(error) => {
                eprintln!("caim {}: {error}", args[0]);
                FAILED
            }
        }
    } else if args.first().is_some_and(|mode| waits::MODES.contains(&mode.as_str())) {
        match waits::dispatch(&args) {
            Ok(status) => status,
            Err(Error::Usage(message)) => {
                eprintln!("caim {}: {message}", args[0]);
                USAGE
            }
            Err(error) => {
                eprintln!("caim {}: the watch failed: {error}", args[0]);
                FAILED
            }
        }
    } else if args.first().is_some_and(|mode| mode == completions::MODE) {
        completions::dispatch(&args)
    } else {
        tasks::main(&args)
    };
    std::process::exit(status);
}
