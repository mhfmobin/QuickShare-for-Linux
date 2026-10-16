fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = [
        "offline_wire_formats",
        "ukey",
        "securemessage",
        "securegcm",
        "device_to_device_messages",
        "wire_format",
        "sharing_enums",
    ]
    .map(|f| format!("proto/{f}.proto"));
    // protox = pure-Rust protobuf compiler, so no system protoc is needed
    let fds = protox::compile(&files, ["proto"])?;
    prost_build::Config::new().compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
