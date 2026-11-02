fn main() {
    for p in std::env::args().skip(1) {
        let found = agentbench_server::projects::icon(std::path::Path::new(&p));
        println!("{p}: {:?}", found.map(|(m, b)| (m, b.len())));
    }
}
