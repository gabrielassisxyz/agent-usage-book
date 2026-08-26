Each `*.rs` fixture here must fail to compile; its expected compiler output is captured
in the same-named `*.stderr` file.

Regenerate captures with the checked guard, never with a bare overwrite:

    cargo run --bin compile_fail_regenerate

The guard runs `TRYBUILD=overwrite cargo test --test compile_fail` and then compares
the error code of each capture against what the compiler produced. A capture may be
regenerated only when the error code is unchanged: additive `help:` text under the same
code is the compiler getting more informative, and the new output is kept. A changed
code means the fixture now fails for a different reason, and the guard restores the old
capture and refuses, naming both codes. `--override` is the explicit override for a
deliberate change, and for the initial capture of a brand-new fixture.
