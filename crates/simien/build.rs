// Node programs granted dangerouslyUseNode run in these binaries (APS 2),
// and their native addons (Rolldown's, fsevents) call Node-API's functions
// in the binary that loads them: export them.
fn main() {
    simien_napi::print_linker_flags("simien");
    simien_napi::print_linker_flags("s");
}
