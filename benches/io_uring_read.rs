#![cfg(unix)]

use tempfile::NamedTempFile;

use tokio::fs::read;

use criterion::{criterion_group, criterion_main, Criterion};

use std::io::Write;

const BUFFER_SIZE: usize = 1024 * 1024 * 1024 * 1 + (1024 * 1024);

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_io_uring()
        .build()
        .unwrap()
}

fn temp_file() -> NamedTempFile {
    let mut file = tempfile::Builder::new().tempfile().unwrap();

    let buffer = vec![0u8; BUFFER_SIZE];

    file.write_all(&buffer).unwrap();

    file
}

#[cfg(all(tokio_unstable, target_os = "linux"))]
fn async_read_file_io_uring(c: &mut Criterion) {
    let rt = rt();

    let file_ref = std::sync::Arc::new(temp_file());
    // enable io_uring
    rt.block_on(async {
        let _ = read(file_ref.clone().as_ref()).await.unwrap();
    });

    c.bench_function("async_read_file_io_uring", |b| {
        let file = file_ref.clone();

        b.iter(|| {
            let file = file.clone();

            rt.block_on(async {
                let _bytes = read(file.as_ref()).await.unwrap();
            })
        })
    });

    c.bench_function("sync_read", |b| {
        let file_ref = file_ref.clone();

        b.iter(|| {
            let file_ref = file_ref.clone();
            let _read = std::fs::read(file_ref.as_ref());
        })
    });
}

#[cfg(not(tokio_unstable))]
fn async_read_file_normal(c: &mut Criterion) {
    let rt = rt();

    let file_ref = std::sync::Arc::new(temp_file());

    c.bench_function("async_read_file_normal", |b| {
        let file = file_ref.clone();

        b.iter(|| {
            let file = file.clone();

            let task = || async {
                let _bytes = read(file.as_ref()).await.unwrap();
            };

            rt.block_on(task());
        })
    });
}

#[cfg(all(tokio_unstable, target_os = "linux"))]
criterion_group!(file, async_read_file_io_uring,);

#[cfg(not(tokio_unstable))]
criterion_group!(file, async_read_file_normal);

criterion_main!(file);
