fn main() {
    let dirs = vec![std::path::PathBuf::from("/tmp/scratch_pad/bundlecheck")];
    let out = slooper::vsthost::scan(&dirs);
    for e in &out {
        println!("{} kind={:?} inst={} err={:?}", e.name, e.kind, e.instrument, e.error);
    }
    assert!(out.iter().any(|e| e.error.is_none()), "bundle layout not found");
    println!("BUNDLE SCAN OK");
}
