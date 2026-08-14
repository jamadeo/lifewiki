mod app;
mod build;
mod git;
mod model;
mod okf;
mod reconcile;
mod tools;
mod event;
mod log;
mod paths;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
