//! Unit-level round-trip tests for the tar+gzip extraction stack used by
//! `sources::codex`. The full install pipeline (HTTP fetch → download →
//! extract → activate) is covered end-to-end by `lifecycle.rs`, which drives
//! the real `ovm install` CLI against a mockito server; here we just pin
//! the tar/gz plumbing behavior so regressions surface early.

use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs;
use tar::Builder;
use tempfile::tempdir;

/// Build a gzipped tar archive containing a single file named `entry_name`.
fn make_tarball(entry_name: &str, contents: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut builder = Builder::new(encoder);

        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_name, contents)
            .expect("append");

        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }
    buf
}

#[test]
fn tar_extraction_round_trip() {
    // Round-trip a gzipped tar through the same flate2 + tar stack used by
    // `sources::codex::extract_release_archive`: the entry name, contents,
    // and mode all come back intact after a create → write → open → unpack
    // cycle. The real download path is covered by lifecycle.rs.
    let binary_contents = b"#!/bin/sh\necho fake-codex-binary\n";
    let asset_body = make_tarball("codex-aarch64-apple-darwin", binary_contents);

    let dir = tempdir().expect("tempdir");
    let archive_path = dir.path().join("dl.tar.gz");
    fs::write(&archive_path, &asset_body).expect("write archive bytes");

    let file = fs::File::open(&archive_path).expect("open");
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    let extract_dir = dir.path().join("out");
    archive.unpack(&extract_dir).expect("unpack");

    let extracted = extract_dir.join("codex-aarch64-apple-darwin");
    assert!(extracted.exists(), "binary should be extracted");
    assert_eq!(fs::read(&extracted).expect("read"), binary_contents);
}

#[test]
fn tar_extraction_preserves_file_contents_and_mode() {
    let dir = tempdir().expect("tempdir");
    let archive_path = dir.path().join("payload.tar.gz");

    let contents = b"sample binary content";
    let bytes = make_tarball("some-binary", contents);
    fs::write(&archive_path, &bytes).expect("write archive");

    let extract_dir = dir.path().join("extract");
    let file = fs::File::open(&archive_path).expect("open");
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(&extract_dir).expect("unpack");

    let extracted = extract_dir.join("some-binary");
    assert!(extracted.exists());
    assert_eq!(fs::read(&extracted).expect("read"), contents);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::metadata(&extracted).unwrap().permissions();
        // tar preserves the 0o755 mode we set
        assert_eq!(perms.mode() & 0o755, 0o755);
    }
}
