// Generated from build.rcl. Do not edit.

pub fn build_platform_for(target: &str) -> &'static str {
    match target {
        "aarch64-apple-darwin" => "Darwin arm64",
        "aarch64-unknown-linux-musl" => "Linux aarch64",
        "armv7-unknown-linux-musleabihf" => "Linux armv7l",
        "x86_64-unknown-linux-musl" => "Linux x86_64",
        "aarch64-unknown-linux-gnu" => "Linux aarch64",
        "x86_64-unknown-linux-gnu" => "Linux x86_64",
        other => panic!("deptool: unsupported target triple {other:?}; add to build.rcl"),
    }
}
