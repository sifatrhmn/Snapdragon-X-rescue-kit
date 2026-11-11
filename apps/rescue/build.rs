use embed_manifest::manifest::{DpiAwareness, ExecutionLevel};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // `new_manifest` already declares Microsoft.Windows.Common-Controls 6.0.0.0.
        embed_manifest(
            new_manifest("WoA.RescueKit")
                .requested_execution_level(ExecutionLevel::RequireAdministrator)
                .dpi_awareness(DpiAwareness::PerMonitorV2),
        )
        .expect("unable to embed the application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
