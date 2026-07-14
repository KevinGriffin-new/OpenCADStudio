//! Round-trip fidelity tests for the CAD leg of the plat2json pipeline:
//! plan-JSON → `LS_IMPORTPLAN` (real landsurvey plugin process) → document
//! entities → `EXPORTPDF <path>` (real dispatch, dialog-free, #369).
//!
//! The import goes through the REAL plugin: the test spawns the
//! `opencad-landsurvey-plugin` cdylib as a plugin runner process (the same
//! `--ocs-plugin-runner` protocol the app uses) and dispatches
//! `LS_IMPORTPLAN <fixture>` against a `HostSession` wrapping the test app,
//! so every entity lands in the app's own document exactly as it would in a
//! live session. The only piece bypassed is the `try_dispatch` string-prefix
//! router (which needs the startup plugin registry).
//!
//! External artifacts required (the tests SKIP with a message when absent):
//! - `OCS_LS_PLUGIN_DLL` — path to the built landsurvey plugin cdylib
//!   (`cargo build` in the sibling `opencad-landsurvey-plugin` repo).
//! - a built host binary to act as the plugin runner: `OCS_PLUGIN_RUNNER_EXE`,
//!   or `cargo build --bin OpenCADStudio` so it sits in the same target dir
//!   as the test binary (`cargo test` images are libtest mains and cannot act
//!   as the runner themselves).
//!
//! See `tools/ROUNDTRIP.md` for the full loop, including the PDF vector
//! verification (`tools/check_roundtrip_pdf.py`).

use crate::app::plugin_host::HostSession;
use crate::app::OpenCADStudio;
use acadrust::EntityType;
use ocs_plugin_api::process::PluginProcess;
use std::path::PathBuf;

const TOL: f64 = 1e-6;

/// The committed plan-JSON fixture (schema per plat2json's README:
/// lines / arcs / circles / texts / polylines-with-bulge).
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/roundtrip_plan.json")
}

/// The landsurvey plugin cdylib, from `OCS_LS_PLUGIN_DLL`. No default: the
/// plugin lives in a sibling repo and its build location is the caller's.
fn plugin_dll() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("OCS_LS_PLUGIN_DLL")?);
    p.exists().then_some(p)
}

/// Resolve the host binary used as the plugin runner and export it through
/// `OCS_PLUGIN_RUNNER_EXE` for `PluginProcess::spawn`. `cargo test` binaries
/// are libtest images that don't understand `--ocs-plugin-runner`, so the
/// runner must be a real host build: the env var if set, else
/// `OpenCADStudio(.exe)` in the same target profile dir as the test binary.
fn ensure_runner_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCS_PLUGIN_RUNNER_EXE") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let exe = std::env::current_exe().ok()?; // <target>/debug/deps/opencadstudio-*.exe
    let profile_dir = exe.parent()?.parent()?;
    let name = if cfg!(windows) {
        "OpenCADStudio.exe"
    } else {
        "OpenCADStudio"
    };
    let host = profile_dir.join(name);
    if host.exists() {
        std::env::set_var("OCS_PLUGIN_RUNNER_EXE", &host);
        Some(host)
    } else {
        None
    }
}

/// Import the fixture through the real plugin process into a fresh test app.
/// Returns `None` (after an explanatory SKIP message) when the external
/// artifacts aren't available; panics on genuine failures.
fn import_fixture() -> Option<(OpenCADStudio, usize)> {
    let Some(dll) = plugin_dll() else {
        eprintln!(
            "SKIP plan_roundtrip: set OCS_LS_PLUGIN_DLL to the built \
             opencad-landsurvey-plugin cdylib"
        );
        return None;
    };
    let Some(_runner) = ensure_runner_exe() else {
        eprintln!(
            "SKIP plan_roundtrip: no host exe to act as plugin runner — \
             `cargo build --bin OpenCADStudio` first or set OCS_PLUGIN_RUNNER_EXE"
        );
        return None;
    };
    let fixture = fixture_path();
    assert!(fixture.exists(), "missing fixture {}", fixture.display());

    let mut app = OpenCADStudio::new_for_test();
    app.automation_op(r#"{"op":"new"}"#); // leave the Start tab
    let tab = app.active_tab;
    {
        let mut host = HostSession::new(&mut app, tab);
        let process = PluginProcess::spawn(&dll, &mut host).expect("spawn landsurvey plugin");
        let handled = process
            .dispatch(
                &mut host,
                &format!("LS_IMPORTPLAN {}", fixture.display()),
                &mut |_| {},
            )
            .expect("dispatch LS_IMPORTPLAN");
        process.shutdown();
        assert!(handled, "landsurvey plugin did not handle LS_IMPORTPLAN");
    }
    Some((app, tab))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= TOL
}

fn close2(p: (f64, f64), q: (f64, f64)) -> bool {
    close(p.0, q.0) && close(p.1, q.1)
}

/// Reconstruct (center, radius, ccw sweep) of the arc segment from
/// `(x1, y1)`→`(x2, y2)` with LWPolyline `bulge` = tan(sweep/4), CCW positive.
fn arc_from_bulge(x1: f64, y1: f64, x2: f64, y2: f64, bulge: f64) -> ((f64, f64), f64, f64) {
    let sweep = 4.0 * bulge.atan();
    let (dx, dy) = (x2 - x1, y2 - y1);
    let chord = dx.hypot(dy);
    let radius = chord / (2.0 * (sweep / 2.0).sin().abs());
    // Center sits off the chord midpoint along the left normal of the travel
    // direction for CCW (bulge > 0), at the signed apothem r·cos(sweep/2).
    let (mx, my) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
    let (nx, ny) = (-dy / chord, dx / chord); // left normal of the travel direction
    // CCW (bulge > 0): the center sits to the LEFT of travel, at the signed
    // apothem r·cos(sweep/2) (negative past a half circle, flipping sides).
    let apothem = radius * (sweep / 2.0).cos() * sweep.signum();
    (((mx + nx * apothem), (my + ny * apothem)), radius, sweep)
}

#[test]
fn ls_importplan_imports_every_plan_block_faithfully() {
    let Some((app, tab)) = import_fixture() else {
        return;
    };
    let doc = &app.tabs[tab].scene.document;

    // ── report line ────────────────────────────────────────────────────────
    let history = app.command_line.history_plain_text();
    assert!(
        history.contains("LS_IMPORTPLAN: 7 entities on 6 layer(s)"),
        "unexpected import report:\n{history}"
    );

    // ── lines: endpoints + layer, 1e-6 ─────────────────────────────────────
    let lines: Vec<_> = doc
        .entities()
        .filter_map(|e| match e {
            EntityType::Line(l) => Some((
                (l.start.x, l.start.y),
                (l.end.x, l.end.y),
                e.common().layer.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(lines.len(), 3, "expected 3 imported lines: {lines:?}");
    let expect_lines = [
        ((0.0, 0.0), (100.0, 0.0), "BOUNDARY"),
        ((100.0, 0.0), (100.0, 80.0), "BOUNDARY"),
        ((10.0, 10.0), (30.0, 40.0), "EASEMENT"),
    ];
    for (s, e, layer) in expect_lines {
        assert!(
            lines
                .iter()
                .any(|(ls, le, ll)| close2(*ls, s) && close2(*le, e) && ll == layer),
            "line {s:?}->{e:?} on {layer} not found in {lines:?}"
        );
    }

    // ── polyline: vertices, bulge, layer; arc reconstructed from the bulge ─
    let polys: Vec<_> = doc
        .entities()
        .filter_map(|e| match e {
            EntityType::LwPolyline(p) => Some((p.clone(), e.common().layer.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(polys.len(), 1, "expected 1 imported polyline");
    let (poly, poly_layer) = &polys[0];
    assert_eq!(poly_layer, "ROAD");
    assert!(!poly.is_closed, "open chain must import open");
    assert_eq!(poly.vertices.len(), 3);
    let v: Vec<(f64, f64, f64)> = poly
        .vertices
        .iter()
        .map(|v| (v.location.x, v.location.y, v.bulge))
        .collect();
    assert!(close2((v[0].0, v[0].1), (40.0, 50.0)) && close(v[0].2, 0.0), "{v:?}");
    assert!(close2((v[1].0, v[1].1), (60.0, 50.0)), "{v:?}");
    assert!(close2((v[2].0, v[2].1), (50.0, 60.0)) && close(v[2].2, 0.0), "{v:?}");
    // The bulge vertex encodes the known 90° CCW arc: r=10, center (50,50).
    let (center, radius, sweep) = arc_from_bulge(v[1].0, v[1].1, v[2].0, v[2].1, v[1].2);
    assert!(
        close2(center, (50.0, 50.0)),
        "bulge arc center {center:?} != (50, 50)"
    );
    assert!(close(radius, 10.0), "bulge arc radius {radius} != 10");
    assert!(
        close(sweep, std::f64::consts::FRAC_PI_2),
        "bulge arc sweep {sweep} != pi/2"
    );

    // ── standalone arc: center/radius/angles (source degrees → radians) ────
    let arcs: Vec<_> = doc
        .entities()
        .filter_map(|e| match e {
            EntityType::Arc(a) => Some((
                (a.center.x, a.center.y),
                a.radius,
                a.start_angle,
                a.end_angle,
                e.common().layer.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(arcs.len(), 1, "expected 1 imported arc: {arcs:?}");
    let (ac, ar, asr, aer, alayer) = &arcs[0];
    assert!(close2(*ac, (150.0, 20.0)), "arc center {ac:?}");
    assert!(close(*ar, 15.0), "arc radius {ar}");
    assert!(close(*asr, 30f64.to_radians()), "arc start {asr}");
    assert!(close(*aer, 120f64.to_radians()), "arc end {aer}");
    assert_eq!(alayer, "CURVES");

    // ── circle ──────────────────────────────────────────────────────────────
    let circles: Vec<_> = doc
        .entities()
        .filter_map(|e| match e {
            EntityType::Circle(c) => Some(((c.center.x, c.center.y), c.radius, e.common().layer.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(circles.len(), 1, "expected 1 imported circle: {circles:?}");
    assert!(close2(circles[0].0, (20.0, 70.0)), "{circles:?}");
    assert!(close(circles[0].1, 5.0), "{circles:?}");
    assert_eq!(circles[0].2, "UTIL");

    // ── text: content + position. KNOWN GAPS (schema + importer): no rotation
    //    field exists, and the height is the importer's fixed PLAN_TEXT_HEIGHT
    //    (2.0) — the 4th plan field is used as the LAYER, not a text style. ──
    let texts: Vec<_> = doc
        .entities()
        .filter_map(|e| match e {
            EntityType::Text(t) => Some((
                t.value.clone(),
                (t.insertion_point.x, t.insertion_point.y),
                t.height,
                t.rotation,
                e.common().layer.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 1, "expected 1 imported text: {texts:?}");
    let (tv, tp, th, trot, tl) = &texts[0];
    assert_eq!(tv, "LOT 1");
    assert!(close2(*tp, (5.0, 5.0)), "text position {tp:?}");
    assert!(close(*th, 2.0), "importer's fixed plan text height, got {th}");
    assert!(close(*trot, 0.0), "no rotation in the plan schema, got {trot}");
    assert_eq!(tl, "ANNOT");
}

#[test]
fn ls_importplan_then_exportpdf_writes_vector_pdf() {
    let Some((mut app, _tab)) = import_fixture() else {
        return;
    };

    // Honor an explicit output path so tools/check_roundtrip_pdf.py can drive
    // the whole loop; default to a temp file.
    let pdf_path = std::env::var_os("OCS_ROUNDTRIP_PDF_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("ocs_roundtrip_{}.pdf", std::process::id()))
        });
    let _ = std::fs::remove_file(&pdf_path);

    // Real dispatch path: the dialog-free EXPORTPDF <path> arm (#369).
    let start = app.command_line.history.len();
    let _ = app.run_command_line(&format!("EXPORTPDF {}", pdf_path.display()));
    let out: String = app.command_line.history[start..]
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(out.contains("Exported"), "no export confirmation: {out:?}");

    let bytes = std::fs::read(&pdf_path).expect("EXPORTPDF <path> should write the file");
    assert!(bytes.starts_with(b"%PDF"), "output is not a PDF");
    assert!(bytes.len() > 200, "suspiciously small PDF: {}", bytes.len());
    eprintln!("roundtrip PDF written to {}", pdf_path.display());
    if std::env::var_os("OCS_ROUNDTRIP_PDF_OUT").is_none() {
        let _ = std::fs::remove_file(&pdf_path);
    }
}
