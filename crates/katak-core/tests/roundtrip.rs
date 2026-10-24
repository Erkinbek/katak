//! End-to-end save tests over the checked-in fixtures in testdata/fixtures:
//! open with auto-detection, save, and compare bytes.

use std::fs;
use std::path::{Path, PathBuf};

use katak_core::document::{Document, OpenOptions};
use katak_core::job::{Job, JobState, no_notify};
use katak_core::logical::LKind;
use katak_core::save::save_file;

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures");
    let mut v: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("testdata/fixtures exists")
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    assert!(
        v.len() >= 10,
        "fixtures missing; run python testdata/make_fixtures.py"
    );
    v
}

fn open(path: &Path) -> Document {
    let doc = Document::open(path, OpenOptions::default(), no_notify()).unwrap();
    if let Some(j) = doc.index_job() {
        j.join();
    }
    doc
}

fn save_to(doc: &Document, target: &Path) {
    let input = doc.save_input(8);
    let t = target.to_path_buf();
    let job = Job::spawn("save", no_notify(), move |ctl| {
        save_file(&input, &t, false, ctl)
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    assert_eq!(job.join(), JobState::Done, "{:?}", job.error());
}

#[test]
fn every_fixture_saves_byte_identically_without_edits() {
    let tmp = tempfile::tempdir().unwrap();
    for f in fixtures() {
        let doc = open(&f);
        let out = tmp.path().join(f.file_name().unwrap());
        save_to(&doc, &out);
        assert_eq!(
            fs::read(&out).unwrap(),
            fs::read(&f).unwrap(),
            "{}",
            f.display()
        );
    }
}

#[test]
fn an_edit_changes_only_its_record() {
    let tmp = tempfile::tempdir().unwrap();
    for f in fixtures() {
        let original = fs::read(&f).unwrap();
        let path = tmp.path().join(f.file_name().unwrap());
        fs::write(&path, &original).unwrap();
        let mut doc = open(&path);
        let first = doc.first_lpos();
        let Ok((rows, _)) = doc.lrows_from(first, 1) else {
            continue;
        };
        let Some(row) = rows.first().copied() else {
            continue;
        };
        let LKind::Src(span) = row.kind else { continue };
        doc.set_values(vec![(row.row_ref(), 0, "EDITED, with comma".into())]);
        save_to(&doc, &path);
        let saved = fs::read(&path).unwrap();
        // Bytes before and after the edited record are untouched.
        assert_eq!(
            &saved[..span.start as usize],
            &original[..span.start as usize],
            "{}",
            f.display()
        );
        let tail = &original[span.content_end as usize..];
        assert!(saved.ends_with(tail), "{}", f.display());
        // Reading back gives the new value.
        doc.reopen().unwrap();
        if let Some(j) = doc.index_job() {
            j.join();
        }
        let first = doc.first_lpos();
        let (rows, _) = doc.lrows_from(first, 1).unwrap();
        assert_eq!(
            doc.value(&rows[0], 0, 1 << 20).unwrap(),
            "EDITED, with comma",
            "{}",
            f.display()
        );
    }
}
