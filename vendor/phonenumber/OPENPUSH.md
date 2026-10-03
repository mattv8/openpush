# OpenPush vendored `phonenumber`

This directory is a maintained local copy of crates.io package
[`phonenumber` 0.3.10+9.0.33](https://crates.io/crates/phonenumber/0.3.10),
licensed under Apache-2.0. The downloaded crate archive SHA-256 is:

`66d85d3cc5477bfe2f357939e7f02a5a566632508633af1a2c37577d0bef87d9`

All retained upstream source, build, license, README, manifest-original, test,
example, benchmark, and `assets/PhoneNumberMetadata.xml` files are byte-for-byte
copies of that package. `PhoneNumberMetadata.xml` is read by `build.rs`.

The upstream carrier, geocoding, alternate-format, short-number, and test
metadata assets are not referenced by the retained build, source, or tests and
are intentionally omitted, along with registry bookkeeping and the upstream
lockfile.

`Cargo.toml` has only these dependency-manifest fixes:

- runtime and build `postcard` use `default-features = false`; the build
  dependency retains `use-std`;
- runtime and build `quick-xml` are pinned to `=0.41.0`.

These changes remove the `heapless`/`atomic-polyfill` default feature chain and
select the patched XML parser without changing upstream runtime algorithms.

## Audit coverage

`cargo-deny` skips advisories for path dependencies, so the vendored local copy
is not covered by root and desktop advisory graphs. The `upstream-audit.lock`
file pins the upstream registry package version and is validated and audited
separately by `infra/audit/check-vendored.py`, which is run in CI after
`cargo-deny`. This ensures upstream advisories are still checked automatically
without suppression.

To refresh `upstream-audit.lock` when updating the vendored package:

1. Update the package in `vendor/phonenumber/` with a new archive from
   `crates.io` and verify the new version in the manifest.
2. Edit `upstream-audit.lock` to match the new package name and version,
   and update the checksum from the crates.io package details.
3. Run `just audit-dependencies` to validate the lock and audit it.
4. Commit both the updated manifest and the refreshed lock.
