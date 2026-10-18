use claude_opendeck::{cli, hook, hooks_install, plugin};

const HELP: &str = "\
claude-opendeck: Claude Usage plugin for OpenDeck / Stream Deck

USAGE:
    claude-opendeck usage [--json]     Print plan usage
    claude-opendeck install-hooks      Add the Claude Code hooks for \"Claude Needs You\"
    claude-opendeck uninstall-hooks    Remove them again
    claude-opendeck hook               (run by Claude Code) record a hook event from stdin
    claude-opendeck --version

OpenDeck starts it with -port/-pluginUUID/-registerEvent/-info to run the plugin.";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("");
    let code = match command {
        // Must never fail or slow down Claude Code: no runtime, no logger.
        "hook" => {
            hook::run();
            0
        }
        "install-hooks" | "uninstall-hooks" => {
            let binary = std::env::current_exe().unwrap_or_default();
            match hooks_install::run(&binary, command == "uninstall-hooks") {
                Ok(message) => {
                    println!("{message}");
                    0
                }
                Err(error) => {
                    eprintln!("claude-opendeck: {error}");
                    1
                }
            }
        }
        "usage" => cli::usage(args.iter().any(|a| a == "--json")),
        "--version" | "-V" => {
            println!("claude-opendeck {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ if args.iter().any(|a| a.eq_ignore_ascii_case("-port")) => run_plugin(args),
        _ => {
            println!("{HELP}");
            if command.is_empty() || command == "--help" || command == "-h" {
                0
            } else {
                2
            }
        }
    };
    std::process::exit(code);
}

fn run_plugin(args: Vec<String>) -> i32 {
    let _ = simplelog::SimpleLogger::init(simplelog::LevelFilter::Info, simplelog::Config::default());
    log::info!("claude-opendeck {} starting", env!("CARGO_PKG_VERSION"));
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("claude-opendeck: {error}");
            return 1;
        }
    };
    match runtime.block_on(plugin::run(args)) {
        Ok(()) => {
            log::info!("connection closed, exiting");
            0
        }
        Err(error) => {
            log::error!("plugin error: {error}");
            1
        }
    }
}
