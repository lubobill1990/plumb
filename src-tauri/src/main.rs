#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // the updater program of Plumb <= 0.1.5 asks the installed exe what it is
        Some("--version") => println!("{}", plumb_lib::update::current_version()),
        // that updater program (`--updater`) is gone: never run as one
        Some("--updater") => std::process::exit(2),
        _ => plumb_lib::run(),
    }
}
