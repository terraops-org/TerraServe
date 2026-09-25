//! One grid id, one grid. Found 2026-09-25 while testing issue #21: loading the published eCH-0056
//! file next to the corrected fixture (both `"id": "SwissLV95CellSizes"`) was accepted silently, and
//! the layer advertised the id twice. `/tileMatrixSets/{id}` answers with the first match, so a
//! duplicate is ambiguous. These run the real binary: `serve` must refuse to start.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_terraserve");
const ECH: &str = "fixtures/grids/eCH-0056_SwissLV95CellSizes.json";

/// Run `serve` and expect it to EXIT (a startup refusal). A server that starts instead is killed
/// after the deadline and reported, so a missing check fails rather than hangs.
fn serve_must_refuse(args: &[&str]) -> String {
    let mut child = Command::new(BIN)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("serve")
        .args(args)
        .args(["--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn terraserve");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve started instead of refusing: {args:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(!status.success(), "must exit non-zero:\n{out}");
    out
}

/// A temp copy of the eCH grid, optionally with its origin moved: same id, maybe different grid.
fn ech_copy(tag: &str, move_origin: bool) -> std::path::PathBuf {
    let mut json = std::fs::read_to_string(ECH).unwrap();
    if move_origin {
        json = json.replace("2419995.75", "2420000.0");
    }
    let p = std::env::temp_dir().join(format!("ts_grid_ids_{}_{tag}.json", std::process::id()));
    std::fs::write(&p, json).unwrap();
    p
}

#[test]
fn one_layer_listing_a_grid_id_twice_refuses_to_start() {
    let copy = ech_copy("same", false);
    let out = serve_must_refuse(&[
        "--vector",
        "fixtures/vector/countries.geojson",
        "--vec-style",
        "fixtures/styles/countries.vec.json",
        "--src-crs",
        "EPSG:4326",
        "--tms-grid",
        ECH,
        "--tms-grid",
        copy.to_str().unwrap(),
    ]);
    let _ = std::fs::remove_file(&copy);
    assert!(
        out.contains("grid id 'SwissLV95CellSizes' is listed twice"),
        "{out}"
    );
}

#[test]
fn two_layers_with_different_grids_under_one_id_refuse_to_start() {
    let moved = ech_copy("moved", true);
    let dir = env!("CARGO_MANIFEST_DIR");
    let config = std::env::temp_dir().join(format!("ts_grid_ids_{}.yaml", std::process::id()));
    std::fs::write(
        &config,
        format!(
            "layers:\n\
             \x20 - name: a\n    vector: {dir}/fixtures/vector/countries.geojson\n    \
             vec_style: {dir}/fixtures/styles/countries.vec.json\n    src_crs: EPSG:4326\n    \
             grids: [{dir}/{ECH}]\n\
             \x20 - name: b\n    vector: {dir}/fixtures/vector/countries.geojson\n    \
             vec_style: {dir}/fixtures/styles/countries.vec.json\n    src_crs: EPSG:4326\n    \
             grids: [{}]\n",
            moved.display()
        ),
    )
    .unwrap();
    let out = serve_must_refuse(&["--config", config.to_str().unwrap()]);
    let _ = std::fs::remove_file(&moved);
    let _ = std::fs::remove_file(&config);
    assert!(
        out.contains("grid id 'SwissLV95CellSizes' means two different grids"),
        "{out}"
    );
    assert!(
        out.contains("layer 'a'") && out.contains("layer 'b'"),
        "{out}"
    );
}
