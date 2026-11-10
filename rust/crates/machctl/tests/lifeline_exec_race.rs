//! DC-304: a lifeline that ends while another test starts `machctl` never
//! ends it. Until its `exec`, a child is a copy of the test and holds every
//! lifeline the test has; a lifeline that ends while the copy becomes
//! `machctl` must not take it for a holder. `machctl`'s `exec` takes long
//! enough for the race to show, where a small system tool's does not.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use service_layout::test_build::harness::{command, Lifeline};

const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

#[test]
fn a_lifeline_that_ends_while_machctl_execs_spares_it() {
    let stop = Arc::new(AtomicBool::new(false));
    let enders: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    // Open long enough for children to fork while it is.
                    let lifeline = Lifeline::new();
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    drop(lifeline);
                }
            })
        })
        .collect();
    let starters: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                (0..300)
                    .filter_map(|_| {
                        let output = command(MACHCTL).arg("--help").output().unwrap();
                        (!output.status.success()).then_some(output.status.code())
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let ended: Vec<_> = starters
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    stop.store(true, Ordering::SeqCst);
    for ender in enders {
        ender.join().unwrap();
    }
    assert!(
        ended.is_empty(),
        "machctl ended by another lifeline: exit codes {ended:?}"
    );
}
