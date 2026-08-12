# wrf-rust provenance

This directory contains the `wrf-core` and `wrf-formula` sources pinned
from `https://github.com/FahrenheitResearch/wrf-rust.git` at commit
`9874474d9566a7536a90f457a48be30caa5f973a`.

That repository declares `MIT` in its workspace package metadata. The pinned
`wrf-core` manifest omitted `license.workspace = true`, so this vendored copy
adds that inheritance line and carries the workspace's MIT license text. The
vendored manifests also add non-behavioral version annotations for the local
`ecape-rs` and `wrf-core` path dependencies so packaged dependency-policy tools
can identify them without wildcard-version ambiguity. Local source integration
is otherwise limited to one Rust 2024 closure-pattern compatibility adjustment
in `wrf-core/src/file.rs` and mechanical `rustfmt 1.92` formatting after the
crates became members of this workspace. No diagnostic or numerical behavior
was intentionally changed.
