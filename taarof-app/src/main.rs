fn main() {
    // Headless config subcommands (config-paths / config-default / config-validate)
    // are handled here, before any GTK initialization or runtime setup, so they
    // work in a plain shell with no display. When the first argument is not a
    // config subcommand, fall through to the normal GUI launch.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(exit_code) = taarof_app::config::run_config_cli(&args) {
        std::process::exit(exit_code);
    }

    taarof_app::run();
}
