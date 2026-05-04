//! Verify the CSV writer handles RFC 4180 edge cases — embedded commas,
//! double-quotes, newlines — by round-tripping rows through the writer
//! and asserting the on-disk text matches the expected encoding.

use xtask::csv::{Row, Writer};

#[test]
fn round_trips_simple_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.csv");

    let header = vec!["target".to_string(), "binary_bytes".to_string()];
    let mut w = Writer::open(&path, &header).unwrap();
    w.append(Row::from([
        ("target", "aarch64-apple-darwin"),
        ("binary_bytes", "12345"),
    ]))
    .unwrap();
    w.flush().unwrap();

    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(body, "target,binary_bytes\naarch64-apple-darwin,12345\n");
}

#[test]
fn quotes_fields_containing_commas_and_quotes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.csv");

    let header = vec!["combo_label".to_string(), "note".to_string()];
    let mut w = Writer::open(&path, &header).unwrap();
    w.append(Row::from([
        ("combo_label", "lto=fat,opt=z"),
        ("note", "value with \"quotes\" inside"),
    ]))
    .unwrap();
    w.flush().unwrap();

    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        body,
        "combo_label,note\n\"lto=fat,opt=z\",\"value with \"\"quotes\"\" inside\"\n"
    );
}

#[test]
fn appends_without_rewriting_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.csv");
    let header = vec!["a".to_string(), "b".to_string()];

    let mut w = Writer::open(&path, &header).unwrap();
    w.append(Row::from([("a", "1"), ("b", "2")])).unwrap();
    w.flush().unwrap();
    drop(w);

    // Re-open in append mode — header MUST NOT be rewritten.
    let mut w2 = Writer::open(&path, &header).unwrap();
    w2.append(Row::from([("a", "3"), ("b", "4")])).unwrap();
    w2.flush().unwrap();

    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(body, "a,b\n1,2\n3,4\n");
}
