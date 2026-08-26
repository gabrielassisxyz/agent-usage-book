//! Checked regeneration for compile-fail captures (aub-tojp).
//!
//! A compile-fail capture is a property of the crate's whole trait graph, not of its
//! fixture: a bead that adds an impl anywhere can add a `help:` block to another
//! bead's capture, with no dependency between the two. Regenerating a capture is
//! therefore a checked operation, and the check is on the error code. This binary
//! runs the trybuild overwrite pass and then compares each capture against what the
//! compiler produced: same error code, keep the new output; changed error code,
//! restore the old capture and refuse, naming both codes. `--override` is the
//! explicit override for a deliberate change.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

/// The fixture directory, relative to the crate root.
const FIXTURE_DIR: &str = "tests/compile_fail";

/// Every `error[E....]` code in the text, sorted and deduplicated.
fn extract_error_codes(text: &str) -> Vec<String> {
    let mut codes = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("error[E") {
        let after = &rest[pos + "error[E".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            codes.push(format!("E{digits}"));
        }
        rest = after;
    }
    codes.sort();
    codes.dedup();
    codes
}

/// The verdict of comparing a captured `.stderr` against the newly produced output.
enum Verdict {
    /// The error codes are unchanged; the new output may replace the capture.
    Allow,
    /// The error codes differ, or either side has none; the capture must not be
    /// regenerated without an explicit override.
    Refuse {
        old_codes: Vec<String>,
        new_codes: Vec<String>,
    },
}

/// Regeneration is allowed only when both sides carry the same non-empty set of
/// error codes. Additive `help:` text under an unchanged code is the compiler
/// getting more informative; a changed code means the fixture fails for a different
/// reason, and blessing the new output would destroy the test.
fn compare_captures(old: &str, new: &str) -> Verdict {
    let old_codes = extract_error_codes(old);
    let new_codes = extract_error_codes(new);
    if old_codes.is_empty() || new_codes.is_empty() {
        return Verdict::Refuse {
            old_codes,
            new_codes,
        };
    }
    if old_codes == new_codes {
        Verdict::Allow
    } else {
        Verdict::Refuse {
            old_codes,
            new_codes,
        }
    }
}

/// Reads every `.stderr` file in the fixture directory, keyed by file name.
fn read_captures(fixture_dir: &Path) -> BTreeMap<String, String> {
    let mut captures = BTreeMap::new();
    let Ok(entries) = fs::read_dir(fixture_dir) else {
        return captures;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("stderr") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if let Ok(content) = fs::read_to_string(&path) {
            captures.insert(name, content);
        }
    }
    captures
}

fn format_codes(codes: &[String]) -> String {
    if codes.is_empty() {
        "(none)".to_string()
    } else {
        codes.join(", ")
    }
}

fn print_refusal(name: &str, old_codes: &[String], new_codes: &[String]) {
    eprintln!("refused: {name}");
    eprintln!("  captured codes: {}", format_codes(old_codes));
    eprintln!("  produced codes: {}", format_codes(new_codes));
    if old_codes.is_empty() {
        eprintln!("  no captured error code to compare against; a new capture is a deliberate act");
    } else {
        eprintln!("  a changed error code means the fixture now fails for a different reason;");
        eprintln!("  regenerating would bless that and destroy the test");
    }
    eprintln!("  Re-run with --override to force");
}

fn usage() {
    eprintln!("usage: compile-fail-regenerate [--override] [--crate-root <dir>]");
}

fn main() {
    let mut override_flag = false;
    let mut crate_root = PathBuf::from(".");
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--override" => override_flag = true,
            "--crate-root" => {
                crate_root = match args.next() {
                    Some(value) => PathBuf::from(value),
                    None => {
                        usage();
                        exit(2);
                    }
                };
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
                exit(2);
            }
        }
    }

    let fixture_dir = crate_root.join(FIXTURE_DIR);
    if !fixture_dir.is_dir() {
        eprintln!("no fixture directory at {}", fixture_dir.display());
        exit(1);
    }

    let old_captures = read_captures(&fixture_dir);

    let status = match Command::new("cargo")
        .args(["test", "--test", "compile_fail"])
        .env("TRYBUILD", "overwrite")
        .current_dir(&crate_root)
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            eprintln!("failed to run cargo test --test compile_fail: {err}");
            exit(1);
        }
    };

    if !status.success() {
        // The overwrite pass did not complete; leave the tree as it was.
        for (name, content) in &old_captures {
            let _ = fs::write(fixture_dir.join(name), content);
        }
        for name in read_captures(&fixture_dir).keys() {
            if !old_captures.contains_key(name) {
                let _ = fs::remove_file(fixture_dir.join(name));
            }
        }
        eprintln!(
            "cargo test --test compile_fail failed (exit {:?}); all captures restored",
            status.code()
        );
        exit(status.code().unwrap_or(1));
    }

    if override_flag {
        println!("override: kept every newly produced capture");
        exit(0);
    }

    let new_captures = read_captures(&fixture_dir);
    let mut refused: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    let mut regenerated = 0;

    for (name, old) in &old_captures {
        match new_captures.get(name) {
            Some(new) => match compare_captures(old, new) {
                Verdict::Allow => regenerated += 1,
                Verdict::Refuse {
                    old_codes,
                    new_codes,
                } => {
                    let _ = fs::write(fixture_dir.join(name), old);
                    refused.push((name.clone(), old_codes, new_codes));
                }
            },
            None => {
                let _ = fs::write(fixture_dir.join(name), old);
                refused.push((name.clone(), Vec::new(), Vec::new()));
            }
        }
    }

    for (name, new) in &new_captures {
        if !old_captures.contains_key(name) {
            let _ = fs::remove_file(fixture_dir.join(name));
            refused.push((name.clone(), Vec::new(), extract_error_codes(new)));
        }
    }

    for (name, old_codes, new_codes) in &refused {
        print_refusal(name, old_codes, new_codes);
    }

    if !refused.is_empty() {
        eprintln!(
            "refused {} capture(s); {} regenerated",
            refused.len(),
            regenerated
        );
        exit(1);
    }

    println!("regenerated {regenerated} capture(s)");
}

#[cfg(test)]
mod tests {
    use super::{Verdict, compare_captures, extract_error_codes};

    const OLD_E0277: &str = "\
error[E0277]: the trait bound `f64: DomainQuantity` is not satisfied
 --> tests/compile_fail/interval_over_primitive.rs:9:13
  |
9 |     let _ = Interval::<f64>::new(0.0, 1.0);
  |             ^^^^^^^^^^^^^^^ the trait `DomainQuantity` is not implemented for `f64`
  |
note: required by a bound in `Interval`
 --> src/domain/interval.rs
  |
  | pub struct Interval<T: DomainQuantity> {
  |                        ^^^^^^^^^^^^^^ required by this bound in `Interval`
";

    const NEW_E0277_WITH_HELP: &str = "\
error[E0277]: the trait bound `f64: DomainQuantity` is not satisfied
 --> tests/compile_fail/interval_over_primitive.rs:9:13
  |
9 |     let _ = Interval::<f64>::new(0.0, 1.0);
  |             ^^^^^^^^^^^^^^^ the trait `DomainQuantity` is not implemented for `f64`
  |
help: the trait `DomainQuantity` is implemented for `TokenCount`
 --> src/domain/tokens.rs
  |
  | impl DomainQuantity for TokenCount {
  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
note: required by a bound in `Interval`
 --> src/domain/interval.rs
  |
  | pub struct Interval<T: DomainQuantity> {
  |                        ^^^^^^^^^^^^^^ required by this bound in `Interval`
";

    /// The shape that actually occurred: the same E0277 with an added `help:` block
    /// naming a new implementor. This is the allowed case.
    #[test]
    fn same_code_with_added_help_is_allowed() {
        assert!(matches!(
            compare_captures(OLD_E0277, NEW_E0277_WITH_HELP),
            Verdict::Allow
        ));
    }

    /// A changed code is refused, and the refusal carries both codes so the reader
    /// can decide without diffing by hand.
    #[test]
    fn different_code_is_refused_and_names_both() {
        let new = "error[E0308]: mismatched types\n --> fixture.rs:1:9\n";
        match compare_captures(OLD_E0277, new) {
            Verdict::Refuse {
                old_codes,
                new_codes,
            } => {
                assert_eq!(old_codes, ["E0277"]);
                assert_eq!(new_codes, ["E0308"]);
            }
            Verdict::Allow => panic!("a changed error code must be refused"),
        }
    }

    /// No parseable code on either side is refused, never allowed by default. A
    /// brand-new fixture has no old capture to compare against, so it is refused
    /// too and needs the explicit override.
    #[test]
    fn no_parseable_code_in_either_is_refused() {
        assert!(matches!(
            compare_captures("no error code here", NEW_E0277_WITH_HELP),
            Verdict::Refuse { .. }
        ));
        assert!(matches!(
            compare_captures(OLD_E0277, "no error code here"),
            Verdict::Refuse { .. }
        ));
        assert!(matches!(compare_captures("a", "b"), Verdict::Refuse { .. }));
        assert!(matches!(
            compare_captures("", NEW_E0277_WITH_HELP),
            Verdict::Refuse { .. }
        ));
    }

    /// The comparison is over the set of codes: losing or gaining any code is a
    /// change, even when one code survives.
    #[test]
    fn a_code_set_change_is_refused() {
        let old = "error[E0277]: x\nerror[E0599]: y\n";
        let new = "error[E0277]: x\n";
        assert!(matches!(compare_captures(old, new), Verdict::Refuse { .. }));
    }

    #[test]
    fn codes_are_sorted_and_deduplicated() {
        assert_eq!(
            extract_error_codes("error[E0277]: a\nerror[E0599]: b\nerror[E0277]: c"),
            ["E0277", "E0599"]
        );
    }
}
