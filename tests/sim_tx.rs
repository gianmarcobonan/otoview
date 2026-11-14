mod common;

#[test]
fn tx_family() {
    common::roundtrip(otoview::protocol::Family::Tx);
}
