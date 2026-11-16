use agentbench_core::Host;
use sysinfo::{Disks, System};

pub fn sample() -> Host {
    let mut host = Host {
        load: System::load_average().map(|l| l.one as f32).unwrap_or_default(),
        cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
        ..Host::default()
    };
    if let Ok(mut sys) = System::new() {
        sys.refresh_memory();
        host.mem_total = sys.total_memory();
        host.mem_used = sys.total_memory().saturating_sub(sys.available_memory());
    }
    if let Ok(disks) = Disks::new_with_refreshed_list() {
        let root = disks
            .iter()
            .find(|d| d.mount_point() == std::path::Path::new("/"))
            .or_else(|| disks.iter().max_by_key(|d| d.total_space()));
        if let Some(d) = root {
            host.disk_total = d.total_space();
            host.disk_free = d.available_space();
        }
    }
    host
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_host_reports_memory_disk_and_cores() {
        let h = super::sample();
        assert!(h.mem_total > 0 && h.mem_used <= h.mem_total);
        assert!(h.disk_total > 0 && h.disk_free <= h.disk_total);
        assert!(h.cores >= 1);
        assert!(h.mem_percent() <= 100 && h.disk_percent() <= 100);
    }
}
