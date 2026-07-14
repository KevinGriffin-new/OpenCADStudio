# plat2json round-trip check (CAD leg)

Proves the CAD leg of the plat2json pipeline is geometrically lossless, in
isolation from plat-reading: plan-JSON → `LS_IMPORTPLAN` (the real
opencad-landsurvey-plugin, spawned as a plugin-runner process) → document
entities (asserted at 1e-6) → `EXPORTPDF <path>` (the dialog-free arm from
\#369) → vector-content verification of the exported PDF.

## Pieces

- `tests/fixtures/roundtrip_plan.json` — synthetic plan-JSON covering every
  schema block: 3 lines on 2 layers, a polyline with a bulge vertex (a known
  90° r=10 arc), a standalone arc, a circle, a text.
- `src/app/plan_roundtrip.rs` — the two tests:
  - `ls_importplan_imports_every_plan_block_faithfully` — geometry readback.
  - `ls_importplan_then_exportpdf_writes_vector_pdf` — headless PDF export.
- `tools/check_roundtrip_pdf.py` — standalone PDF vector check (PyMuPDF +
  numpy): fits ONE similarity transform from the straight segments, then
  verifies lines/arcs/circle residuals against it and reports text presence.

## Running the whole loop

The import tests need two external artifacts and SKIP (with a message) when
they are absent, so plain `cargo test --lib` stays green everywhere:

```sh
# 1. Build the plugin cdylib (sibling repo)
cd ../opencad-landsurvey-plugin && cargo build

# 2. Build a host binary — the plugin runner is the host exe itself
#    (`--ocs-plugin-runner`); libtest binaries can't play that role.
cargo build --bin OpenCADStudio

# 3. Run the round-trip tests, exporting the PDF to a known path
OCS_LS_PLUGIN_DLL=../opencad-landsurvey-plugin/target/debug/opencad_landsurvey_plugin.dll \
OCS_ROUNDTRIP_PDF_OUT=/tmp/roundtrip.pdf \
cargo test --lib plan_roundtrip -- --nocapture

# 4. Verify the exported PDF's vector content
python tools/check_roundtrip_pdf.py tests/fixtures/roundtrip_plan.json /tmp/roundtrip.pdf
```

`OCS_PLUGIN_RUNNER_EXE` overrides the runner-exe autodetection (which looks
for `OpenCADStudio(.exe)` in the same target profile dir as the test binary).

## Known gaps this loop documents (not regressions)

- The plan-JSON schema has **no text rotation** field, and the importer sets a
  **fixed text height** (`PLAN_TEXT_HEIGHT = 2.0`); the 4th `texts` field is
  used as the *layer*, not a text style.
- `EXPORTPDF` writes wires only: arcs/circles arrive **tessellated** (no PDF
  arc/bezier primitives) and text arrives as **stroke glyphs**, not PDF text
  objects. The Python check accepts tessellation and reports text as
  vector-ink presence.
