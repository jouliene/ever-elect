use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=EVER_ELECT_BUILD_COMMIT={commit}");
}
