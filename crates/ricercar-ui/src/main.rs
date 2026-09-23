use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let headless = args.iter().any(|a| a == "--headless");
    let cfg_args: Vec<String> = args.into_iter().filter(|a| a != "--headless").collect();
    let cfg = match ricercar_daemon::Config::from_args(cfg_args.into_iter()) {
        Ok(cfg) => cfg,
        Err(msg) => {
            let _ = writeln!(std::io::stderr(), "{msg}");
            std::process::exit(2);
        }
    };
    let ctx = match ricercar_daemon::startup(cfg) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("ricercar: {e}");
            std::process::exit(1);
        }
    };
    if headless {
        ctx.wait_until_quit();
        return;
    }
    if let Err(e) = ricercar_ui::run_ui(ctx.ctl) {
        eprintln!("ui: {e}");
        std::process::exit(1);
    }
}
