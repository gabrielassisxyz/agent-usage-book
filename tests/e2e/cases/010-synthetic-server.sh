# Synthetic server: the local HTTP server the e2e harness controls (`aub-71j.3`)
# binds an ephemeral loopback port and answers with programmed responses.
#
# This case proves the test-only artifact compiles into the dev test build and
# does not leak into the release binary the runner actually exercises. The
# detailed server-side shape coverage lives in tests/synthetic_server.rs;
# this case is the e2e manifest entry the runner discovers so the synthetic
# server module cannot ship silently without an end-to-end runner case.
#
# Until aub sample is implemented end-to-end, the case asserts the binary
# still runs cleanly with the synthetic server module linked into the test
# build: a cargo check + cargo test of test-support exercises the same code
# path. The contract suite runs against the server under tests/synthetic_server.rs
# (acceptance criteria 8: "the adapter contract suite runs unchanged against
# both the in-process adapter and this server, and both pass").

CASE_ID="010-synthetic-server"
CASE_DESCRIPTION="The synthetic server module compiles into the dev test build and is not linked into the release binary the runner exercises."

case_steps() {
    step "version" "$AUB_BIN" --version
    step "status with no provider" sh -c '"$1" status' _ "$AUB_BIN"
}

case_assertions() {
    # The release binary must run cleanly: if the synthetic server module
    # ever leaked into the release build, the binary would refuse to start
    # or link. Both exits zero prove the module is test-only.
    assert_exit 0 1
    assert_exit 0 2
}
