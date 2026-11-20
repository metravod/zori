# Releasing Zori

1. Update the package version in Cargo.toml and Cargo.lock. Run the Rust and web test suites.
2. Push the reviewed commit to main, then push an annotated tag matching the package version, for example `v0.1.0`.
3. The [release workflow](../.github/workflows/release.yml) runs tests, Clippy and an echo flow on each native target. It publishes four archives and SHA256SUMS after every build passes. Linux binaries target GNU/Linux and are built on Ubuntu 22.04.
4. The [tap](https://github.com/metravod/homebrew-tap) checks the latest stable release daily. To publish the formula immediately, dispatch its “Update Zori” workflow. The tap reads the four archive digests from GitHub and updates its own formula.
5. Check the release from a fresh installation with `brew install metravod/tap/zori` and `zori --version`.
