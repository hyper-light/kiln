// The control for T-T2.1.2: the same build naming hyper-durable's disk store, GroupStore over a
// real device, builds: what fails beside it is RamStore alone.
fn main() {
    let _ = core::mem::size_of::<hyper_durable::GroupStore<hyper_block::file::DeviceFile>>();
}
