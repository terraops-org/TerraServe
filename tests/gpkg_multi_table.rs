//! Issue #22 (public repo): "how does one reference a table within a geopackage?"
//!
//! `fixtures/gpkg/stlouis/` is the reporter's own case: the StLouis.gpkg from ngageoint's
//! geopackage-js examples (MIT, raster `tiles` table dropped), whose feature tables are, in order,
//! PointsOfInterest, Parks and Pizza, plus his two SLDs verbatim.
//!
//! Before the fix a GeoPackage layer always read the FIRST feature table, so both of his layers
//! served PointsOfInterest: `Parks` rendered blank (a polygon style over points) and `Pizza` only
//! looked right because its point style happened to fit the points of interest.
//!
//! These tests run the real binary on his config. The table choice travels YAML -> layer spec ->
//! GeoPackage reader, and a unit test of any one hop still passes with another hop dropped: the
//! `serve.rs` line that copies `vec_layer:` into the spec is covered nowhere else.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_terraserve");
const DIR: &str = "fixtures/gpkg/stlouis";
/// Covers every feature of all three tables (EPSG:4326, WMS 1.3.0 lat/lon axis order).
const BBOX: &str = "38.55,-90.33,38.70,-90.18";
/// His Parks SLD fills its one rule (`name = 'Benton Park'`) with #d7191c.
const PARKS_RED: [u8; 3] = [0xd7, 0x19, 0x1c];
/// His Pizza SLD draws every pizza place as a #becf50 circle.
const PIZZA_GREEN: [u8; 3] = [0xbe, 0xcf, 0x50];

/// A `terraserve` child whose stdout and stderr are drained line by line into one channel, so a
/// chatty server never blocks on a full pipe. Killed on drop, whatever the test outcome.
struct Proc {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Proc {
    fn spawn(args: &[&str]) -> Proc {
        let mut child = Command::new(BIN)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn terraserve");
        let (tx, lines) = channel();
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(l);
            }
        });
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx2.send(l);
            }
        });
        Proc {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Collect output until a line contains `marker` (true) or the process closes both pipes (false).
    fn wait_for(&mut self, marker: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(l) => {
                    let hit = l.contains(marker);
                    self.seen.push(l);
                    if hit {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
        false
    }

    fn output(&self) -> String {
        self.seen.join("\n")
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Start `serve` with `args` plus a free port, and wait until it is listening.
fn serve(args: &[&str]) -> (Proc, u16) {
    let port = free_port();
    let port_s = port.to_string();
    let mut all = vec!["serve"];
    all.extend_from_slice(args);
    all.extend_from_slice(&["--port", &port_s]);
    let mut p = Proc::spawn(&all);
    assert!(
        p.wait_for("TerraServe serving on"),
        "server did not start:\n{}",
        p.output()
    );
    (p, port)
}

/// A WMS GetMap for `layer`, decoded to RGBA.
fn get_map(port: u16, layer: &str) -> Vec<u8> {
    let path = format!(
        "/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={layer}&STYLES=&CRS=EPSG:4326\
         &BBOX={BBOX}&WIDTH=600&HEIGHT=600&FORMAT=image/png"
    );
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).expect("read response");
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP header end");
    let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "GetMap {layer}: {head}");
    assert!(
        !head.contains("transfer-encoding: chunked"),
        "this helper reads a plain body: {head}"
    );
    let mut dec = png::Decoder::new(&buf[split + 4..]);
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info().expect("PNG header");
    let mut px = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut px).expect("PNG frame");
    px.truncate(info.buffer_size());
    match info.color_type {
        png::ColorType::Rgba => px,
        png::ColorType::Rgb => px
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        other => panic!("unexpected PNG colour type {other:?}"),
    }
}

fn count(rgba: &[u8], rgb: [u8; 3]) -> usize {
    rgba.chunks_exact(4)
        .filter(|p| p[3] > 0 && p[..3] == rgb)
        .count()
}

#[test]
fn the_reported_config_refuses_to_start_and_lists_the_tables() {
    let port = free_port().to_string();
    let config = format!("{DIR}/issue22.yaml");
    let mut p = Proc::spawn(&["serve", "--config", &config, "--port", &port]);
    assert!(
        !p.wait_for("TerraServe serving on"),
        "a multi-table GeoPackage with no table named must not start:\n{}",
        p.output()
    );
    let status = p.child.wait().expect("wait");
    assert!(!status.success(), "must exit non-zero");
    let out = p.output();
    assert!(
        out.contains("layer 'Parks'"),
        "must say which layer to fix:\n{out}"
    );
    assert!(
        out.contains("PointsOfInterest, Parks, Pizza"),
        "must list the tables:\n{out}"
    );
    assert!(out.contains("vec_layer"), "must say how to choose:\n{out}");
}

#[test]
fn vec_layer_serves_each_table_with_its_own_style() {
    let config = format!("{DIR}/stlouis.yaml");
    let (p, port) = serve(&["--config", &config]);
    let out = p.output();
    assert!(
        out.contains("layer 'Parks': vector (12 features, table Parks)"),
        "Parks must read its own 12 rows:\n{out}"
    );
    assert!(
        out.contains("layer 'Pizza': vector (9 features, table Pizza)"),
        "Pizza must read its own 9 rows:\n{out}"
    );

    // His Parks SLD, unchanged: the `Benton Park` rule now has polygons to draw. Before the fix
    // this count was 0, because the layer held points.
    let parks = get_map(port, "Parks");
    assert!(
        count(&parks, PARKS_RED) > 0,
        "Benton Park must be drawn in #d7191c"
    );
    assert_eq!(
        count(&parks, PIZZA_GREEN),
        0,
        "Parks must not draw pizza places"
    );

    let pizza = get_map(port, "Pizza");
    assert!(
        count(&pizza, PIZZA_GREEN) > 0,
        "pizza places must be drawn in #becf50"
    );
    assert_eq!(count(&pizza, PARKS_RED), 0, "Pizza must not draw parks");
}

#[test]
fn vector_layer_flag_picks_the_table_for_a_single_layer_serve() {
    let gpkg = format!("{DIR}/stlouis.gpkg");
    let style = format!("{DIR}/parks.sld");
    let (p, _port) = serve(&[
        "--vector",
        &gpkg,
        "--vector-layer",
        "Parks",
        "--vec-style",
        &style,
        "--src-crs",
        "EPSG:4326",
    ]);
    let out = p.output();
    assert!(
        out.contains("(12 features, table Parks)"),
        "--vector-layer must reach the reader:\n{out}"
    );
}

/// Run a one-shot subcommand to completion; returns (success, combined output).
fn run(args: &[&str]) -> (bool, String) {
    let mut p = Proc::spawn(args);
    let status = p.child.wait().expect("wait");
    // Both pipes close at exit, so this drains everything the process printed.
    p.wait_for("\u{0}");
    (status.success(), p.output())
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ts_issue22_{}_{name}", std::process::id()))
}

#[test]
fn vector_layer_flag_reaches_build_pmtiles() {
    let out = tmp("parks.pmtiles");
    let (gpkg, style) = (format!("{DIR}/stlouis.gpkg"), format!("{DIR}/parks.sld"));
    let (ok, log) = run(&[
        "build-pmtiles",
        "--vector",
        &gpkg,
        "--vector-layer",
        "Parks",
        "--vec-style",
        &style,
        "--src-crs",
        "EPSG:4326",
        "--min-zoom",
        "10",
        "--max-zoom",
        "10",
        "--out",
        out.to_str().unwrap(),
    ]);
    let _ = std::fs::remove_file(&out);
    assert!(ok, "bake failed:\n{log}");
    assert!(
        log.contains("(12 features, table Parks)"),
        "--vector-layer must reach the bake's reader:\n{log}"
    );
}

#[test]
fn vector_layer_flag_reaches_extract() {
    let out = tmp("pizza.gpkg");
    let _ = std::fs::remove_file(&out);
    let (gpkg, style) = (format!("{DIR}/stlouis.gpkg"), format!("{DIR}/pizza.sld"));
    let (ok, log) = run(&[
        "extract",
        "--vector",
        &gpkg,
        "--vector-layer",
        "Pizza",
        "--vec-style",
        &style,
        "--src-crs",
        "EPSG:4326",
        "--min-zoom",
        "10",
        "--max-zoom",
        "10",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "extract failed:\n{log}");
    let conn = rusqlite::Connection::open(&out).expect("open the extract");
    let table: String = conn
        .query_row(
            "SELECT table_name FROM gpkg_contents WHERE data_type='features'",
            [],
            |r| r.get(0),
        )
        .expect("the extract's feature table");
    let rows: i64 = conn
        .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| {
            r.get(0)
        })
        .expect("count");
    drop(conn);
    let _ = std::fs::remove_file(&out);
    assert_eq!(rows, 9, "the subset must come from Pizza (9 rows):\n{log}");
}
