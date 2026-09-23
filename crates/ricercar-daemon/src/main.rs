use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--print-devices") {
        ricercar_daemon::print_devices();
        return;
    }
    match ricercar_daemon::Config::from_args(args.into_iter()) {
        Ok(cfg) => {
            if let Err(e) = ricercar_daemon::run(cfg) {
                eprintln!("ricercar: {e}");
                std::process::exit(1);
            }
        }
        Err(msg) => {
            let _ = writeln!(std::io::stderr(), "{msg}");
            std::process::exit(2);
        }
    }
}
