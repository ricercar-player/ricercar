fn main() {
    ricercar_core::profile::mark_start();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--print-devices") {
        ricercar_daemon::print_devices();
        return;
    }
    let args = match ricercar_daemon::Args::parse(args.into_iter()) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    if !args.no_mpris && ricercar_daemon::forward_to_running(&args) {
        return;
    }
    if args.headless {
        match ricercar_daemon::startup(args, ricercar_daemon::Hooks::default()) {
            Ok(ctx) => ctx.wait_until_quit(),
            Err(e) => {
                eprintln!("ricercar: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Err(e) = ricercar_ui::run(args) {
        startup_failed(&e.to_string());
        std::process::exit(1);
    }
}

/// The app was started from a launcher more often than from a terminal: say
/// why it did not open in the log file and as a desktop notification too.
fn startup_failed(msg: &str) {
    eprintln!("ricercar: {msg}");
    // No-op when logging is already set up; opens the log file otherwise.
    ricercar_daemon::init_logging();
    tracing::error!("could not start: {msg}");
    let log = ricercar_daemon::logging::log_path();
    let body = format!("{msg}\n\nDetails: {}", log.display());
    if let Ok(mut child) = std::process::Command::new("notify-send")
        .args([
            "--app-name=ricercar",
            "--icon=ricercar",
            "--urgency=critical",
        ])
        .arg("ricercar could not start")
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        // notify-send returns at once; reap it before exiting.
        let _ = child.wait();
    }
}
