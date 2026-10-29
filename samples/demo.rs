// InfEditor demo: syntax highlighting + Chinese text
use std::collections::HashMap;

#[derive(Debug, Clone)]
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    /// Distance to the origin
    fn norm(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

fn main() {
    let mut map: HashMap<&str, i32> = HashMap::new();
    map.insert("apple", 3);
    map.insert("banana", 7);
    let p = Point { x: 3.0, y: 4.0 };
    println!("距离 = {}", p.norm());
    for (k, v) in &map {
        println!("{k} -> {v}");
    }
    let s = r#"原始字符串 "引号" 不会结束"#;
    let c = '\n';
    let n = 0xFF_u8;
    println!("{s} {c} {n}");
}
