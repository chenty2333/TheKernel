//! Counts/status only: never prints OEM AML, returned buffers or product keys.
use tk_acpica::{Engine, Mode, Value, host::OfflineBackend};
fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: table_probe EXTERNAL_TABLE_DIRECTORY");
        std::process::exit(2);
    };
    let backend = OfflineBackend::from_directory(std::path::Path::new(&path))
        .unwrap_or_else(|e| {
            eprintln!("SKIP external firmware unavailable/invalid: {e}");
            std::process::exit(77)
        })
        .register();
    // SAFETY: exclusively owned offline backend simulates all hardware.
    let engine = unsafe { Engine::initialize(backend, Mode::Offline) }.unwrap_or_else(|s| {
        eprintln!("namespace initialization failed: {s:#x}");
        std::process::exit(1)
    });
    println!(
        "object-init status={:?} (hardware is simulated)",
        engine.initialize_objects()
    );
    let nodes = engine.namespace().unwrap();
    let devices = nodes.iter().filter(|n| n.kind == 6).count();
    let methods = nodes.iter().filter(|n| n.kind == 8).count();
    println!(
        "namespace nodes={} devices={devices} methods={methods}",
        nodes.len()
    );
    let s5 = engine.evaluate("\\_S5", &[]);
    println!(
        "_S5 status={:?}",
        s5.as_ref().map(|v| matches!(v, Value::Package(_)))
    );
    let mut prt_ok = 0;
    let mut prt_err = 0;
    let mut prw_ok = 0;
    let mut prw_err = 0;
    let mut buttons = 0;
    let mut ec = 0;
    let mut thermal = 0;
    for n in &nodes {
        if n.path.ends_with("._PRT") {
            if engine.evaluate(&n.path, &[]).is_ok() {
                prt_ok += 1
            } else {
                prt_err += 1
            }
        }
        if n.path.ends_with("._PRW") {
            if engine
                .evaluate(&n.path, &[])
                .and_then(tk_acpica::gpe::parse)
                .is_ok()
            {
                prw_ok += 1;
            } else {
                prw_err += 1;
            }
        }
        if n.kind == 13 {
            thermal += 1;
        }
        if n.kind == 6 {
            if let Ok(hid) = engine.hardware_id(&n.path) {
                if hid == "PNP0C0C" {
                    buttons += 1;
                    let _ = engine.integer(&format!("{}._STA", n.path));
                }
                if hid == "PNP0C09" {
                    ec += 1;
                }
            }
        }
    }
    println!(
        "_PRT ok={prt_ok} errors={prt_err} method-buttons={buttons} EC={ec} thermal={thermal} \
         AML-errors={}",
        tk_acpica::aml_error_count()
    );
    println!(
        "_PRW decoded={prw_ok} errors={prw_err} AML-errors={}",
        tk_acpica::aml_error_count()
    );
    if s5.is_err() || prt_ok == 0 || prt_err != 0 || prw_err != 0 {
        std::process::exit(1);
    }
}
