// Quick check: run the git ls-remote based version detection.
fn main() {
    let out = std::process::Command::new("git")
        .args([
            "ls-remote",
            "--tags",
            "https://github.com/vpertj/deepseek-harness-desktop.git",
            "v*",
        ])
        .output()
        .expect("git");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut latest = String::new();
    for line in text.lines() {
        if line.contains("^{}") { continue; }
        if let Some(tag) = line.split('\t').nth(1) {
            let v = tag.trim_start_matches("refs/tags/").trim_start_matches('v').to_string();
            if !v.is_empty() && v > latest { latest = v; }
        }
    }
    println!("git ls-remote tags: {}", text.lines().count());
    println!("latest tag: v{}", latest);
}
