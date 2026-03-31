use std::process::Command;

fn main() {
    let output = Command::new("cargo")
        .args(&["check", "-p", "autoanneal-lib"])
        .output()
        .expect("failed to execute cargo check");
    
    println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
    println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    println!("exit code: {:?}", output.status.code());
}
