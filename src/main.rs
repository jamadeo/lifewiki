mod agent;
mod app;
mod build;
#[cfg(feature = "local")]
mod daemon;
mod event;
mod git;
mod index;
mod log;
mod model;
mod okf;
mod paths;
mod progress;
#[cfg(feature = "local")]
mod protocol;
mod prompts;
mod tools;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
