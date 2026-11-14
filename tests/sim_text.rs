// Separate test binary per family: two simulators in one process would answer each other's probes.
mod common;

#[test]
fn text_family() {
    common::roundtrip(otoview::protocol::Family::Text);
}
