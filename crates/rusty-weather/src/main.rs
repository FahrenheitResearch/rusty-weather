fn main() {
    eprintln!(
        "rusty-weather {}: daemon not built yet (see docs/superpowers/specs/). \
         Use the smoke_direct / smoke_derived binaries to validate the extraction.",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}
