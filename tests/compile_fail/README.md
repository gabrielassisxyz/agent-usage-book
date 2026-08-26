Each `*.rs` fixture here must fail to compile; its expected compiler output is captured
in the same-named `*.stderr` file. Regenerate after changing a fixture or the toolchain:
`TRYBUILD=overwrite cargo test --test compile_fail`.
