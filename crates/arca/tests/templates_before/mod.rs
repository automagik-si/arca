//! Shipped template files as an earlier release shipped them, for the tests of `templates install`, `arca init` and
//! the installer. No copy of an old file is kept here: each is made from this release's file and proven by its hash.

/// A shipped Postgres file exactly as release 0.12.0 shipped it, made from this release's file by undoing what
/// changed since. The caller checks its hash against `cmd_templates::EARLIER`, which proves it is that file byte for
/// byte.
pub fn postgres_file_of_0_12_0(name: &str) -> String {
    let now = arca::cmd_templates::SHIPPED
        .iter()
        .find(|(shipped, _)| *shipped == name)
        .map(|(_, text)| *text)
        .expect("a shipped file");
    let before = match name {
        "postgres-autopg.json" => now
            .replace(", which Arca makes at create and never shows", "")
            .replace("  \"generate\": { \"POSTGRES_PASSWORD\": \"\" },\n", "")
            .replace(
                "  \"data\": { \"path\": \"/var/lib/autopg/data\", \"uid\": 1000, \"gid\": 1000, \"size\": \"20GiB\" },\n",
                "",
            ),
        "postgres-autopg.sh" => now.replace(
            "is not set: this service was created from a template that did not make it (arca templates install, then create it again; or service_secret_set)",
            "is not set (service_secret_set)",
        ),
        other => panic!("{other} did not change since 0.12.0"),
    };
    let digest = arca_kernel::canon::Digest::of(before.as_bytes()).to_hex();
    assert!(
        arca::cmd_templates::EARLIER
            .iter()
            .any(|(earlier, hash)| *earlier == name && *hash == digest),
        "{name} as rebuilt here is not the file 0.12.0 shipped ({digest})"
    );
    before
}
