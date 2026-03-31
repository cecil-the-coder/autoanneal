use std::path::Path;

fn main() {
    // Test Path::starts_with behavior
    let wd = Path::new("/tmp/abc");
    let sibling = Path::new("/tmp/abcdef");
    let child = Path::new("/tmp/abc/def");
    
    println!("wd: {:?}", wd);
    println!("sibling: {:?}", sibling);
    println!("child: {:?}", child);
    
    println!("sibling starts_with wd: {}", sibling.starts_with(wd));
    println!("child starts_with wd: {}", child.starts_with(wd));
    println!("wd starts_with wd: {}", wd.starts_with(wd));
    
    // Test with canonical paths
    let wd_canon = std::fs::canonicalize("/tmp").unwrap().join("abc");
    let sibling_canon = std::fs::canonicalize("/tmp").unwrap().join("abcdef");
    
    println!("\nWith canonicalized paths:");
    println!("wd_canon: {:?}", wd_canon);
    println!("sibling_canon: {:?}", sibling_canon);
    println!("sibling starts_with wd: {}", sibling_canon.starts_with(&wd_canon));
}
