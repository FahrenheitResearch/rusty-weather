# Third-party notices

Rusty Weather is distributed under the MIT License in `LICENSE`. That license
does not replace the terms that apply to third-party software, fonts, static
assets, or weather data.

## Bundled static assets

### Natural Earth basemaps

The `assets/basemap/natural_earth_10m` and
`assets/basemap/natural_earth_110m` directories contain Natural Earth
coastline, land, ocean, lake, national-boundary, and state/province-boundary
data. Natural Earth data is public domain. Source:
<https://www.naturalearthdata.com/>.

### United States county boundaries

`assets/basemap/us_counties_5m` contains the U.S. Census Bureau 2023
cartographic boundary county file (`cb_2023_us_county_5m`). Works prepared by
U.S. federal government employees as part of their official duties are public
domain in the United States. Source:
<https://www.census.gov/geographies/mapping-files/time-series/geo/cartographic-boundary.html>.
No Census Bureau endorsement is implied.

### Source Sans 3

`crates/rustwx-render/assets/fonts/SourceSans3-Regular.ttf` is Source Sans 3,
Copyright 2010-2024 Adobe, with Reserved Font Name "Source". It is licensed
under the SIL Open Font License 1.1. The complete text is bundled at
`crates/rustwx-render/assets/fonts/SourceSans3-LICENSE.md` and must accompany
distributions containing the embedded font.

## Vendored software

- `vendor/grib-core`: MIT; extracted from this MIT-licensed Fahrenheit Research
  codebase and covered by the repository `LICENSE`.
- `vendor/sharprs`: MIT AND BSD-3-Clause. Its bundled `LICENSE` contains both
  Fahrenheit Research's MIT notice and the SHARPpy BSD-3-Clause copyright,
  conditions, and disclaimer. `vendor/sharprs/PROVENANCE.md` records the known
  import boundary and the intentionally unresolved standalone upstream revision.
- `vendor/ecape-rs`: MIT; bundled text at `vendor/ecape-rs/LICENSE`.
- `vendor/netcrust`: MIT OR Apache-2.0; bundled texts at
  `vendor/netcrust/LICENSE-MIT` and `vendor/netcrust/LICENSE-APACHE`.
- `vendor/netcrust/vendor/hdf5-reader`: MIT OR Apache-2.0; covered by the
  bundled netcrust license copies.
- `vendor/wrf-rust` (`wrf-core` and `wrf-formula`): MIT; pinned source
  provenance is recorded in `vendor/wrf-rust/PROVENANCE.md` and the complete
  text is bundled at `vendor/wrf-rust/LICENSE`.
- `vendor/metrust`, `vendor/wx-core`, `vendor/wx-field`, `vendor/wx-math`, and
  `vendor/wx-radar`: their Cargo package metadata declares MIT.

No automated notice generator can safely choose terms for those components.
The copyright holders must supply consistent metadata and license files. The
dependency policy intentionally leaves missing or inconsistent licensing as a
release failure rather than silently assuming MIT.

## Registry dependencies and SBOM

Every release produces CycloneDX 1.5 JSON SBOMs for the `rw-server` and
`rw-scheduler` binaries from the locked Cargo graph. The SBOMs are the
authoritative version inventories; they are not substitutes for the
corresponding license texts or a legal review.

To regenerate it from a trusted checkout:

    cargo install --locked --version 0.5.9 cargo-cyclonedx
    export SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
    cargo metadata --locked --format-version 1 --no-deps > /dev/null
    cargo cyclonedx --manifest-path crates/rw-server/Cargo.toml \
      --format json --spec-version 1.5 --describe binaries --target all
    cargo cyclonedx --manifest-path crates/rw-scheduler/Cargo.toml \
      --format json --spec-version 1.5 --describe binaries --target all

Those commands create `crates/rw-server/rw-server_bin.cdx.json` and
`crates/rw-scheduler/rw-scheduler_bin.cdx.json`. The release workflow also
checks that generation leaves `Cargo.lock` byte-for-byte unchanged.

Run `cargo deny check` to audit advisories, allowed registries, duplicate
versions, and declared licenses. Review all generated notices whenever
`Cargo.lock`, vendored sources, embedded assets, or model-data packaging
changes.

## Weather data is not bundled software

Downloaded or operator-supplied model and observation data retains its source
terms. See `docs/DATA_SOURCES.md` before caching or redistributing any dataset.
