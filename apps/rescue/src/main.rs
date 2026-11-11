#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod confirm;
mod ui;

fn main() {
    std::process::exit(ui::window::run());
}
