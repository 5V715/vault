// SPDX-License-Identifier: MIT

//! End-to-end tests of the `vaultctl` binary against in-process S3 / WebDAV mocks.

#[path = "support/mock_s3.rs"]
mod mock_s3;
#[path = "support/mock_webdav.rs"]
mod mock_webdav;

use std::path::Path;
use std::process::{Command, Stdio};

const PASS: &str = "correct horse battery staple";
/// `init` with the binding every test container uses.
const INIT: &[&str] = &[
    "init",
    "--app",
    "test-app",
    "--origin",
    "https://app.example",
];

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn ctl(pass: &str, envs: &[(&str, &str)], stdin: &str, args: &[&str]) -> Out {
    use std::io::Write;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vaultctl"));
    cmd.args(args)
        .env("VAULTCTL_PASSPHRASE", pass)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code().unwrap(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn run(dir: &Path, envs: &[(&str, &str)], args: &[&str]) -> Out {
    let mut a = vec!["--dir", dir.to_str().unwrap()];
    a.extend_from_slice(args);
    ctl(PASS, envs, "", &a)
}

fn recovery_key(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("Recovery key: "))
        .expect("recovery key line")
        .trim()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_create_put_replicate_restore_roundtrip() {
    let s3 = mock_s3::MockS3::start("vault", mock_s3::Mode::Enforce, 1000);
    let dav = mock_webdav::MockWebDav::start(mock_webdav::Mode::Enforce);
    let (endpoint, dav_url) = (s3.endpoint(), dav.base_url());
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("vault");
        let envs = [
            ("VAULTCTL_S3_SECRET_KEY", "s3-secret"),
            ("VAULTCTL_WEBDAV_PASSWORD", mock_webdav::PASSWORD),
        ];

        let o = run(&d, &[], INIT);
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert!(
            o.stdout
                .contains("Bound to app test-app at origin https://app.example"),
            "{}",
            o.stdout
        );
        let rk = recovery_key(&o.stdout);
        assert_eq!(rk.split('-').count(), 16);

        let o = run(&d, &[], &["put", "greeting", "hello"]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        let o = run(&d, &[], &["put", "--base64", "bin", "AAEC/w=="]);
        assert_eq!(o.code, 0, "{}", o.stderr);

        let o = run(
            &d,
            &envs,
            &[
                "replica",
                "add",
                "s3",
                "--id",
                "s3a",
                "--endpoint",
                &endpoint,
                "--region",
                "us-east-1",
                "--bucket",
                "vault",
                "--prefix",
                "replica",
                "--access-key",
                "AKIAEXAMPLE",
                "--path-style",
            ],
        );
        assert_eq!(o.code, 0, "{}", o.stderr);
        let o = run(
            &d,
            &envs,
            &[
                "replica",
                "add",
                "webdav",
                "--id",
                "dav",
                "--url",
                &dav_url,
                "--username",
                mock_webdav::USER,
            ],
        );
        assert_eq!(o.code, 0, "{}", o.stderr);

        let o = run(&d, &[], &["replica", "list"]);
        assert_eq!(o.code, 0);
        assert!(o.stdout.contains("s3a") && o.stdout.contains("dav"));
        assert!(!o.stdout.contains("s3-secret") && !o.stdout.contains(mock_webdav::PASSWORD));

        let o = run(&d, &[], &["replicate"]);
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
        assert!(o.stdout.contains("s3a") && o.stdout.contains("synced"));
        assert!(!o.stdout.contains("s3-secret") && !o.stderr.contains("s3-secret"));

        // Restore from the S3 replica with the recovery key into a new directory.
        let r = tmp.path().join("restored");
        let o = run(
            &r,
            &[
                ("VAULTCTL_S3_SECRET_KEY", "s3-secret"),
                ("VAULTCTL_RECOVERY_KEY", &rk),
            ],
            &[
                "restore",
                "s3",
                "--id",
                "s3a",
                "--endpoint",
                &endpoint,
                "--region",
                "us-east-1",
                "--bucket",
                "vault",
                "--prefix",
                "replica",
                "--access-key",
                "AKIAEXAMPLE",
                "--path-style",
            ],
        );
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
        assert!(o.stdout.contains("s3a"));

        let o = run(&r, &[], &["get", "greeting"]);
        assert_eq!((o.code, o.stdout.as_str()), (0, "hello\n"), "{}", o.stderr);
        let o = run(&r, &[], &["get", "--base64", "bin"]);
        assert_eq!((o.code, o.stdout.trim()), (0, "AAEC/w=="));
        let o = run(&r, &[], &["scan", "g"]);
        assert_eq!(o.stdout, "greeting\thello\n");
        // The binding travels with the header: the restored vault keeps it.
        let o = run(&r, &[], &["info"]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert!(
            o.stdout
                .contains("app: test-app\norigin: https://app.example\n"),
            "{}",
            o.stdout
        );

        // The restored vault keeps replicating to the restored-from replica.
        let o = run(&r, &[], &["put", "more", "data"]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        let o = run(&r, &[], &["replicate"]);
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    })
    .await
    .unwrap();
}

#[test]
fn vaultctl_init_requires_app_and_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("v");
    let origin = "https://app.example";
    // Both flags are required (also through the `create` alias); nothing is created.
    for args in [
        vec!["init"],
        vec!["init", "--app", "test-app"],
        vec!["init", "--origin", origin],
        vec!["create"],
    ] {
        let o = run(&d, &[], &args);
        assert_ne!(o.code, 0, "{args:?}");
        assert!(o.stderr.contains("required"), "{args:?}: {}", o.stderr);
        assert!(!d.exists(), "{args:?}");
    }
    // Invalid values are refused before anything is written.
    for (app, origin) in [
        ("Test", "https://app.example"),
        ("", "https://app.example"),
        ("test-app", "http://app.example"),
        ("test-app", "https://app.example/path"),
        ("test-app", "https://user@app.example"),
    ] {
        let o = run(&d, &[], &["init", "--app", app, "--origin", origin]);
        assert_eq!(o.code, 1, "{app} {origin}");
        assert!(o.stderr.contains("invalid app binding"), "{}", o.stderr);
        assert!(!d.exists(), "{app} {origin}");
    }
    // The origin is stored in canonical form; `info` prints the binding.
    let o = run(
        &d,
        &[],
        &[
            "init",
            "--app",
            "test-app",
            "--origin",
            "HTTPS://App.Example:443",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(&d, &[], &["info"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout,
        "app: test-app\norigin: https://app.example\nseq: 0\n"
    );
    // Other commands are unchanged.
    assert_eq!(run(&d, &[], &["put", "k", "v"]).code, 0);
    let o = run(&d, &[], &["info"]);
    assert!(o.stdout.ends_with("seq: 1\n"), "{}", o.stdout);
}

#[test]
fn wrong_passphrase_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("v");
    assert_eq!(run(&d, &[], INIT).code, 0);
    let o = ctl(
        "not it",
        &[],
        "",
        &["--dir", d.to_str().unwrap(), "get", "k"],
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("passphrase"), "{}", o.stderr);
}

#[test]
fn restore_refuses_nonempty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("v");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("keep.txt"), b"x").unwrap();
    let rk = "AAAA-AAAA";
    let o = run(
        &d,
        &[("VAULTCTL_RECOVERY_KEY", rk)],
        &[
            "restore",
            "webdav",
            "--url",
            "http://127.0.0.1:9/x",
            "--username",
            "u",
        ],
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("not empty"), "{}", o.stderr);
    assert!(d.join("keep.txt").exists());
}

#[test]
fn create_refuses_nonempty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("x"), b"x").unwrap();
    let o = run(tmp.path(), &[], INIT);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("not empty"), "{}", o.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diverged_replica_exits_2_and_adopt_needs_confirmation() {
    let dav = mock_webdav::MockWebDav::start(mock_webdav::Mode::Enforce);
    let url = dav.base_url();
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let envs = [("VAULTCTL_WEBDAV_PASSWORD", mock_webdav::PASSWORD)];
        let add = |d: &Path| {
            assert_eq!(run(d, &[], INIT).code, 0);
            let o = run(
                d,
                &envs,
                &[
                    "replica",
                    "add",
                    "webdav",
                    "--id",
                    "dav",
                    "--url",
                    &url,
                    "--username",
                    mock_webdav::USER,
                ],
            );
            assert_eq!(o.code, 0, "{}", o.stderr);
        };
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        add(&a);
        add(&b);
        assert_eq!(run(&a, &[], &["replicate"]).code, 0);
        let o = run(&b, &[], &["replicate"]);
        assert_eq!(o.code, 2, "{}{}", o.stdout, o.stderr);
        assert!(o.stdout.contains("vaultctl replica adopt dav"));

        let args = |d: &Path| {
            vec![
                "--dir".to_string(),
                d.to_str().unwrap().to_string(),
                "replica".into(),
                "adopt".into(),
                "dav".into(),
            ]
        };
        let argv = args(&b);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let o = ctl(PASS, &envs, "nope\n", &argv);
        assert_eq!(o.code, 1, "{}", o.stdout);
        assert_eq!(run(&b, &[], &["replicate"]).code, 2);
        let o = ctl(PASS, &envs, "dav\n", &argv);
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
        let o = run(&b, &[], &["replicate"]);
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_file_is_private_encrypted_and_corruption_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let dav = mock_webdav::MockWebDav::start(mock_webdav::Mode::Enforce);
    let url = dav.base_url();
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("v");
        let o = run(&d, &[], INIT);
        assert_eq!(o.code, 0);
        let rk = recovery_key(&o.stdout);
        let o = run(
            &d,
            &[("VAULTCTL_WEBDAV_PASSWORD", mock_webdav::PASSWORD)],
            &[
                "replica",
                "add",
                "webdav",
                "--url",
                &url,
                "--username",
                mock_webdav::USER,
            ],
        );
        assert_eq!(o.code, 0, "{}", o.stderr);

        let f = d.join(".vaultctl");
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!d.join(".vaultctl.tmp").exists());
        let mut blob = std::fs::read(&f).unwrap();
        let has =
            |hay: &[u8], needle: &str| hay.windows(needle.len()).any(|w| w == needle.as_bytes());
        let compact = rk.replace('-', "");
        for secret in [
            rk.as_str(),
            compact.as_str(),
            mock_webdav::PASSWORD,
            mock_webdav::USER,
            url.as_str(),
        ] {
            assert!(!has(&blob, secret), "plaintext {secret:?} in secrets file");
        }

        let n = blob.len() - 5;
        blob[n] ^= 1;
        std::fs::write(&f, &blob).unwrap();
        let o = run(&d, &[], &["get", "k"]);
        assert_eq!(o.code, 1);
        assert!(
            o.stderr.contains("wrong passphrase or corrupted"),
            "{}",
            o.stderr
        );
    })
    .await
    .unwrap();
}

#[test]
fn webdav_url_with_credentials_and_empty_passphrase_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("v");
    assert_eq!(run(&d, &[], INIT).code, 0);
    let o = run(
        &d,
        &[("VAULTCTL_WEBDAV_PASSWORD", "x")],
        &[
            "replica",
            "add",
            "webdav",
            "--url",
            "https://bob:hunter2@cloud.example/dav",
            "--username",
            "bob",
        ],
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("must not contain credentials"));
    assert!(!o.stderr.contains("hunter2") && !o.stdout.contains("hunter2"));
    let o = ctl("", &[], "", &["--dir", d.to_str().unwrap(), "get", "k"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("empty"), "{}", o.stderr);
}

#[test]
fn failed_restore_leaves_target_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path().join("new");
    let o = run(
        &r,
        &[
            ("VAULTCTL_RECOVERY_KEY", "AAAA"),
            ("VAULTCTL_WEBDAV_PASSWORD", "x"),
        ],
        &[
            "restore",
            "webdav",
            "--url",
            "http://127.0.0.1:9/x",
            "--username",
            "u",
        ],
    );
    assert_eq!(o.code, 1);
    assert!(!r.exists());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

/// `restore` probes the source's conditional writes like `replica add` does: the
/// source is kept as a replica, so a provider ignoring preconditions is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_probes_source_conditional_writes() {
    use vault_container::backend::{WebDavBackend, WebDavConfig};
    use vault_container::engine::Container;
    let dav = mock_webdav::MockWebDav::start(mock_webdav::Mode::IgnoreConditions);
    let b = WebDavBackend::new(WebDavConfig {
        base_url: dav.base_url(),
        username: mock_webdav::USER.into(),
        password: zeroize::Zeroizing::new(mock_webdav::PASSWORD.into()),
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap();
    let (_c, rk) = Container::create(b, "test-app", "https://app.example")
        .await
        .unwrap();
    let rk = rk.to_display().to_string();
    let url = dav.base_url();
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path().join("restored");
        let o = run(
            &r,
            &[
                ("VAULTCTL_WEBDAV_PASSWORD", mock_webdav::PASSWORD),
                ("VAULTCTL_RECOVERY_KEY", &rk),
            ],
            &[
                "restore",
                "webdav",
                "--id",
                "dav",
                "--url",
                &url,
                "--username",
                mock_webdav::USER,
            ],
        );
        assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
        assert!(!r.exists());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    })
    .await
    .unwrap();
}

const PRE_V1: &str = "unsupported container format (created before vault v1)";

/// Copies the pre-v1 fixture (written by the code from before the split, see its
/// README) into `dir`, with its secrets file as `.vaultctl`.
fn pre_v1_dir(dir: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre-v1");
    std::fs::create_dir_all(dir.join("seg")).unwrap();
    std::fs::copy(src.join("header"), dir.join("header")).unwrap();
    std::fs::copy(src.join("manifest"), dir.join("manifest")).unwrap();
    std::fs::copy(src.join("vaultctl-secrets"), dir.join(".vaultctl")).unwrap();
    for e in std::fs::read_dir(src.join("seg")).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), dir.join("seg").join(e.file_name())).unwrap();
    }
}

/// Every command on a directory from before the split names the format, not a
/// recovery key typo or a wrong passphrase, and changes nothing.
#[test]
fn vaultctl_pre_v1_dir_reports_unsupported_format() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("old");
    pre_v1_dir(&d);
    let before: Vec<_> = ["header", "manifest", ".vaultctl"]
        .iter()
        .map(|n| std::fs::read(d.join(n)).unwrap())
        .collect();
    for args in [
        vec!["info"],
        vec!["get", "greeting"],
        vec!["scan", ""],
        vec!["put", "k", "v"],
        vec!["commit"],
        vec!["replicate"],
        vec!["replica", "list"],
    ] {
        // The fixture's own passphrase, and a wrong one: the header is checked first.
        for pass in ["fixture-passphrase", "not it"] {
            let mut a = vec!["--dir", d.to_str().unwrap()];
            a.extend_from_slice(&args);
            let o = ctl(pass, &[], "", &a);
            assert_eq!(o.code, 1, "{args:?}: {}{}", o.stdout, o.stderr);
            assert!(o.stderr.contains(PRE_V1), "{args:?}: {}", o.stderr);
            assert!(!o.stderr.contains("checksum"), "{args:?}: {}", o.stderr);
        }
    }
    let after: Vec<_> = ["header", "manifest", ".vaultctl"]
        .iter()
        .map(|n| std::fs::read(d.join(n)).unwrap())
        .collect();
    assert_eq!(before, after);
}

/// `restore` from a replica holding a pre-split container reports the format
/// before it parses the (old) recovery key, and creates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vaultctl_restore_from_pre_v1_replica_reports_unsupported_format() {
    use vault_container::backend::{ContainerBackend, LocalBackend, WebDavBackend, WebDavConfig};
    let dav = mock_webdav::MockWebDav::start(mock_webdav::Mode::Enforce);
    let b = WebDavBackend::new(WebDavConfig {
        base_url: dav.base_url(),
        username: mock_webdav::USER.into(),
        password: zeroize::Zeroizing::new(mock_webdav::PASSWORD.into()),
        allow_insecure_http: false,
        extra_root_ca_pem: None,
    })
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("old");
    pre_v1_dir(&old);
    let local = LocalBackend::open(&old).await.unwrap();
    let mut names = vec!["header".to_string(), "manifest".to_string()];
    names.extend(local.list("seg/").await.unwrap());
    for n in names {
        let (data, _) = local.get(&n).await.unwrap().unwrap();
        b.put_if_absent(&n, &data).await.unwrap();
    }
    let url = dav.base_url();
    let base = tmp.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let r = base.join("restored");
        // The recovery key printed by the old code (fixture README).
        let old_key =
            "LDVL-GV7F-BKGR-V24N-MYYQ-CNT2-GDDI-2UCY-AAS6-OD7D-FI7O-2RO6-RJ5E-IEJS-MFBP-OKFJ";
        let o = run(
            &r,
            &[
                ("VAULTCTL_WEBDAV_PASSWORD", mock_webdav::PASSWORD),
                ("VAULTCTL_RECOVERY_KEY", old_key),
            ],
            &[
                "restore",
                "webdav",
                "--id",
                "dav",
                "--url",
                &url,
                "--username",
                mock_webdav::USER,
            ],
        );
        assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
        assert!(o.stderr.contains(PRE_V1), "{}", o.stderr);
        assert!(!r.exists());
    })
    .await
    .unwrap();
}
