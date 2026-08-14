mod agent;
mod app;
mod build;
mod event;
mod git;
mod index;
mod log;
mod model;
mod okf;
mod paths;
mod progress;
mod prompts;
mod tools;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
