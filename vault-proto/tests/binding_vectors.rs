// SPDX-License-Identifier: MIT
//! Pins the binding rules to the shared vectors in `tests/vectors/`. The Android
//! app's Kotlin port (`android/core`, `VaultIdentityTest`) runs the same files, so
//! both sides agree byte for byte on which vault an offer belongs to.

use vault_proto::binding::{normalize_origin, validate_app_id, validate_origin};

fn cases(file: &str) -> Vec<(&str, &str)> {
    let v: Vec<_> = file
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split_once('\t').expect("input<TAB>expected"))
        .collect();
    assert!(v.len() > 10, "vector file looks truncated");
    v
}

#[test]
fn origin_vectors() {
    for (input, expected) in cases(include_str!("vectors/origins.tsv")) {
        match (normalize_origin(input), expected) {
            (Err(_), "!") => {}
            (Ok(got), "!") => panic!("{input:?} must be rejected, got {got:?}"),
            (Err(e), want) => panic!("{input:?} must normalize to {want:?}, got {e:?}"),
            (Ok(got), want) => {
                assert_eq!(got, want, "{input:?}");
                // The canonical form is a fixed point and the only accepted form.
                assert_eq!(normalize_origin(&got).unwrap(), got);
                assert!(validate_origin(&got).is_ok());
                assert_eq!(validate_origin(input).is_ok(), input == got, "{input:?}");
            }
        }
    }
}

#[test]
fn app_id_vectors() {
    assert!(validate_app_id("").is_err());
    for (input, expected) in cases(include_str!("vectors/app-ids.tsv")) {
        assert_eq!(
            validate_app_id(input).is_ok(),
            expected == "ok",
            "{input:?}"
        );
    }
}
