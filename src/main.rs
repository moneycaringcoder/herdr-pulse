//! pulse — per-workspace agent activity history for herdr.
//!
//! Verb dispatch only; every verb is implemented in the library crate.

use pulse::{config, daemon, history, render, setup, Result};

const USAGE: &str = "\
pulse — agent activity history and sparklines for herdr

Usage: pulse [VERB]

Reporting:
  --once                    Print a one-shot activity report and exit
  --week                    Print a week of hourly history and exit
  --json                    Print the recorded history as JSON and exit
  --watch                   Live activity view, refreshing on an interval

Sampler:
  --enable                  Start the background sampler
  --disable                 Stop it and clear every badge this plugin set
  --toggle                  Stop it if running, otherwise start it
  --restore                 Restart it only if it was enabled (herdr startup hook)
  --daemon                  Run the sampler in the foreground (internal)

History:
  --forget                  Delete the recorded history and start over

Sidebar setup:
  --setup                   Add pulse's tokens to herdr's config.toml and reload
  --setup-rollback          Restore the config.toml backup taken by --setup

Other:
  --interval <SECS>         Seconds between snapshots (default: 5)
  --bucket-seconds <SECS>   Wall clock per history bucket (default: 60)
  --retention-buckets <N>   Buckets retained per workspace (default: 240)
  --columns <N>             Sparkline columns in the badge (default: 8)
  --agents                  Record and show one series per agent (costs more disk)
  --version                 Print version and exit
  --help                    Show this help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = run(&args) {
        eprintln!("pulse: {err}");
        std::process::exit(1);
    }
}

/// Options that take a value, and so must never be mistaken for the verb.
const VALUED: [&str; 4] = [
    "--interval",
    "--bucket-seconds",
    "--retention-buckets",
    "--columns",
];

/// The verb is the first argument that is not an option or an option's value, so
/// `pulse --interval 10 --once` works as readily as `pulse --once --interval 10`.
/// Ordering that matters is a papercut nobody should have to learn.
fn verb_of(args: &[String]) -> &str {
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        let name = arg.split('=').next().unwrap_or(arg);
        if VALUED.contains(&name) {
            // `--interval=5` carries its value; bare `--interval 5` does not.
            skip_value = !arg.contains('=');
            continue;
        }
        return arg;
    }
    "--once"
}

fn run(args: &[String]) -> Result<()> {
    let verb = verb_of(args);
    match verb {
        "--once" => render::run_once(&config::load_with_args(args)?),
        "--week" => render::run_week(&config::load_with_args(args)?),
        "--json" => render::run_json(&config::load_with_args(args)?),
        "--watch" => render::run_watch(&config::load_with_args(args)?),
        "--enable" => daemon::enable(args),
        "--disable" => daemon::disable(),
        "--toggle" => daemon::toggle(args),
        "--restore" => daemon::restore(),
        // The daemon records why it stopped on its way out, including this exit:
        // an error that ends the run is a reason a gap can carry, and losing it
        // would leave the run indistinguishable from one that was killed.
        "--daemon" => daemon::run_daemon(&config::load_with_args(args)?),
        "--forget" => history::forget(),
        "--setup" => setup::run_setup(),
        "--setup-rollback" => setup::run_rollback(),
        "--version" => {
            println!("pulse {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown verb `{other}`\n\n{USAGE}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::verb_of;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_verb_is_found_whatever_the_order() {
        assert_eq!(verb_of(&args(&["--once"])), "--once");
        assert_eq!(verb_of(&args(&["--week"])), "--week");
        assert_eq!(verb_of(&args(&["--json", "--interval", "5"])), "--json");
        assert_eq!(verb_of(&args(&["--interval", "5", "--json"])), "--json");
        assert_eq!(verb_of(&args(&["--interval=5", "--json"])), "--json");
        assert_eq!(
            verb_of(&args(&["--bucket-seconds", "30", "--watch"])),
            "--watch"
        );
    }

    #[test]
    fn no_arguments_means_a_one_shot_report() {
        assert_eq!(verb_of(&args(&[])), "--once");
        // Options alone still leave the default verb in place.
        assert_eq!(verb_of(&args(&["--interval", "5"])), "--once");
    }

    #[test]
    fn an_option_value_is_never_mistaken_for_a_verb() {
        // A value that looks like a verb must still be treated as a value.
        assert_eq!(verb_of(&args(&["--columns", "--json"])), "--once");
    }
}
