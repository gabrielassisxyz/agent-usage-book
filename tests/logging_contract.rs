use std::process::Command;

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

fn json_string_field(line: &str, field: &str) -> String {
    let needle = format!("\"{field}\":\"");
    let remainder = line.split_once(&needle).expect("field must exist").1;
    remainder
        .split_once('"')
        .expect("field must terminate")
        .0
        .to_owned()
}

#[test]
fn fixture_keeps_report_on_stdout_and_typed_events_on_stderr_with_one_run() {
    let output = aub()
        .args(["-v", "__logging-fixture"])
        .output()
        .expect("fixture must run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert_eq!(stderr.lines().count(), 2);
    assert!(stdout.contains("\"run\":\""));
    assert!(!stdout.contains("\"event\""));

    let lines: Vec<_> = stderr.lines().collect();
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with('{') && line.ends_with('}'))
    );
    assert_eq!(json_string_field(lines[0], "event"), "run_started");
    assert_eq!(json_string_field(lines[1], "event"), "report_rendered");
    assert_eq!(json_string_field(lines[0], "level"), "info");
    assert_eq!(
        json_string_field(lines[0], "run"),
        json_string_field(lines[1], "run")
    );
    assert_eq!(
        json_string_field(lines[0], "run"),
        json_string_field(&stdout, "run")
    );
    for line in lines {
        assert!(line.contains("\"ts\":"));
    }
}

#[test]
fn status_is_quiet_by_default_and_logging_does_not_open_its_projection() {
    let default = aub().arg("status").output().expect("status must run");
    assert!(default.status.success());
    assert!(default.stderr.is_empty());

    let raised = aub()
        .args(["-v", "status"])
        .output()
        .expect("status must run");
    assert!(raised.status.success());
    let stderr = String::from_utf8(raised.stderr).unwrap();
    assert!(stderr.contains("\"event\":\"run_started\""));
    assert!(stderr.contains("\"command\":\"status\""));
}

/// Every `.rs` source file of the `cli` module, paired with its repository-relative
/// path: `src/cli.rs` when it exists, plus every file under `src/cli/`. A module may
/// exist as a flat file, a directory, or both, and the guard below resolves the union
/// rather than one literal path because reading `src/cli.rs` alone would keep passing
/// over whatever share of the module had moved into `src/cli/`, with nothing saying so
/// (aub-pbx4.2). The shell boundary rules resolve the same set through
/// `bin/checks/boundary-rules/lib/module-files.sh`.
fn cli_module_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    if root.join("src/cli.rs").is_file() {
        paths.push(root.join("src/cli.rs"));
    }
    collect_rs_files(&root.join("src/cli"), &mut paths);
    paths.sort();
    assert!(
        !paths.is_empty(),
        "the cli module must have at least one source file: neither src/cli.rs nor src/cli/ exists"
    );
    paths
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{relative} must be readable: {error}"));
            (relative, source)
        })
        .collect()
}

/// Recursively collects `.rs` files under `dir` into `out`. A missing directory
/// contributes nothing, which is the ordinary case for a module that is a flat file.
fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The body of `declaration`, from its first character to the next column-zero `fn `.
/// The column-zero anchor is what makes the end of the body a declaration boundary
/// rather than any nested `fn` inside it.
fn function_body(source: &str, declaration: &str) -> String {
    let start = source
        .find(declaration)
        .unwrap_or_else(|| panic!("source must declare {declaration}"));
    let rest = &source[start..];
    let end = rest[declaration.len()..]
        .find("\nfn ")
        .map(|offset| offset + declaration.len() + 1)
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

/// The body of `declaration` from whichever file of the `cli` module declares it, with
/// that file's path so a failure names where the offending reference is. A declaration
/// present in no file of the module panics rather than contributing an empty body: an
/// absent subject must be louder than a clean one.
fn cli_function_body(sources: &[(String, String)], declaration: &str) -> (String, String) {
    for (path, source) in sources {
        if source.contains(declaration) {
            return (path.clone(), function_body(source, declaration));
        }
    }
    let paths: Vec<&str> = sources.iter().map(|(path, _)| path.as_str()).collect();
    panic!("the cli module must declare {declaration}, in one of {paths:?}");
}

/// Every statement of `body` that constructs the diagnostic logger or emits through
/// it: from each `DiagnosticLogger::new(` or `.emit(` to the `;` that terminates the
/// statement. Those statements are what raising verbosity adds to a command, so they
/// are the subject of the guard below, and slicing them individually keeps the
/// surrounding work out of the scan.
fn logging_statements(body: &str) -> Vec<String> {
    let mut statements = Vec::new();
    for anchor in ["DiagnosticLogger::new(", ".emit("] {
        let mut cursor = 0;
        while let Some(offset) = body[cursor..].find(anchor) {
            let start = cursor + offset;
            let end = body[start..]
                .find(';')
                .map(|semicolon| start + semicolon + 1)
                .unwrap_or(body.len());
            statements.push(body[start..end].to_string());
            cursor = end;
        }
    }
    statements
}

/// Raising verbosity on `status` must not open a file. The behavioural companion in
/// `status_is_quiet_by_default_and_logging_does_not_open_its_projection` runs the real
/// binary; this is the source-level one, and its subject is the logger construction and
/// the emissions themselves rather than the whole of `fn status(`, which performs the
/// one bounded projection read that invariant 15 allows it. Reading the region between
/// `fn status` and `fn logging_fixture` is what this guard used to do, and that region
/// had silently collapsed onto `fn status_clock_skew_envelope`'s four lines once a
/// declaration whose name also starts with `fn status` landed above it, so the guard
/// asserted nothing at all while staying green (aub-pbx4.2).
#[test]
fn raised_status_logging_has_no_file_access_path() {
    let sources = cli_module_sources();
    let (path, status_body) = cli_function_body(&sources, "fn status(");
    let statements = logging_statements(&status_body);
    assert!(
        !statements.is_empty(),
        "fn status( in {path} must construct a logger and emit through it: a guard over no \
         statement asserts nothing"
    );
    for statement in &statements {
        for forbidden in ["std::fs", "File::", "read_to_string", "projection"] {
            assert!(
                !statement.contains(forbidden),
                "status logging must not add file access through {forbidden}, found in {path}: \
                 {statement}"
            );
        }
    }
}

/// The negatives that keep the guard above a test rather than a ritual. Each mutation
/// is one step from a tree the guard passes, and the dimension it changes is the only
/// difference.
#[test]
fn the_status_logging_scan_catches_file_access_and_a_missing_subject() {
    let clean = "fn status(l: Level) {\n    let mut logger = DiagnosticLogger::new(io::stderr(), l);\n    logger.emit(ts, Event::RunStarted, &[]).unwrap();\n}\n";
    let clean_statements = logging_statements(clean);
    assert_eq!(clean_statements.len(), 2);
    assert!(
        clean_statements
            .iter()
            .all(|statement| !statement.contains("read_to_string"))
    );

    let poisoned = clean.replace("&[]", "&[(\"path\", &std::fs::read_to_string(p).unwrap())]");
    assert!(
        logging_statements(&poisoned)
            .iter()
            .any(|statement| statement.contains("std::fs")),
        "a file read smuggled into an emission must be caught"
    );

    let sinks_to_a_file = clean.replace("io::stderr()", "File::create(p).unwrap()");
    assert!(
        logging_statements(&sinks_to_a_file)
            .iter()
            .any(|statement| statement.contains("File::")),
        "a logger sunk into a file must be caught"
    );

    let elsewhere = vec![
        ("src/cli.rs".to_string(), "fn main() {}\n".to_string()),
        ("src/cli/status_workflow.rs".to_string(), clean.to_string()),
    ];
    let (resolved, _) = cli_function_body(&elsewhere, "fn status(");
    assert_eq!(
        resolved, "src/cli/status_workflow.rs",
        "the guard must follow fn status( out of src/cli.rs into the cli module"
    );

    let absent = vec![("src/cli.rs".to_string(), "fn main() {}\n".to_string())];
    let missing = std::panic::catch_unwind(|| cli_function_body(&absent, "fn status("));
    assert!(
        missing.is_err(),
        "a cli module declaring no fn status( must fail loudly rather than scan an empty body"
    );
}
