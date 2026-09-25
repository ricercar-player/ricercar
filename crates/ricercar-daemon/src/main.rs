fn main() {
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
    match ricercar_daemon::startup(args, ricercar_daemon::Hooks::default()) {
        Ok(ctx) => ctx.wait_until_quit(),
        Err(e) => {
            eprintln!("ricercar: {e}");
            std::process::exit(1);
        }
    }
}
