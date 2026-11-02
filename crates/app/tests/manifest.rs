use std::process::Command;

#[test]
fn desktop_build_uses_the_system_clipboard_and_opens_links() {
    let out = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--offline",
            "-e",
            "features,normal",
            "-i",
            "egui-winit",
            "-p",
            "agentbench",
            "--no-default-features",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tree = String::from_utf8_lossy(&out.stdout);
    for feature in ["clipboard", "links"] {
        assert!(
            tree.contains(&format!("egui-winit feature \"{feature}\"")),
            "build lacks egui-winit/{feature}:\n{tree}"
        );
    }
}
