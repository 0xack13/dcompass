use criterion::{criterion_group, criterion_main, Criterion};
use dmatcher::domain::Domain;
use std::{fs::File, io::Read};

fn bench_match(c: &mut Criterion) {
    let mut file = File::open("./benches/sample.txt").unwrap();
    let mut contents = String::new();
    let mut matcher = Domain::new();
    file.read_to_string(&mut contents).unwrap();
    matcher.insert_multi(&contents);
    c.bench_function("match", |b| {
        b.iter(|| assert_eq!(matcher.matches("你好.store.www.baidu.com"), true))
    });
}

criterion_group!(benches, bench_match);
criterion_main!(benches);
