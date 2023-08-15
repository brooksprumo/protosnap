use std::io::Result;
fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=schema");
    prost_build::compile_protos(&["schema/snapshot.proto"], &["schema/"])?;
    Ok(())
}
