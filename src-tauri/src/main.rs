#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // the dedicated updater program: a copy of this exe, run outside the install dir
        Some("--updater") => std::process::exit(plumb_lib::update::run_cli(&args[1..])),
        // used by the updater to check what got installed
        Some("--version") => println!("{}", plumb_lib::update::current_version()),
        _ => plumb_lib::run(),
    }
}
