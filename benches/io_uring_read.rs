#![cfg(unix)]

use tempfile::NamedTempFile;

use tokio::fs::read;

use criterion::{criterion_group, criterion_main, Criterion};

use std::io::Write;

const BUFFER_SIZE: usize = 5000;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .build()
        .unwrap()
}

fn temp_file() -> NamedTempFile {
    let mut file = tempfile::Builder::new().tempfile().unwrap();

    let buffer = [0u8; BUFFER_SIZE];

    file.write_all(&buffer).unwrap();

    file
}

#[cfg(all(tokio_unstable, target_os = "linux"))]
fn async_read_file_io_uring(c: &mut Criterion) {
    let rt = rt();

    let file_ref = std::sync::Arc::new(temp_file());

    c.bench_function("async_read_file_io_uring", |b| {
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

fn sync_read(c: &mut Criterion) {
    let file_ref = std::sync::Arc::new(temp_file());

    c.bench_function("sync_read", |b| {
        let file_ref = file_ref.clone();

        b.iter(|| {
            let file_ref = file_ref.clone();
            let _read = std::fs::read(file_ref.as_ref());
        })
    });
}

#[cfg(all(tokio_unstable, target_os = "linux"))]
criterion_group!(file, async_read_file_io_uring, sync_read,);

#[cfg(not(tokio_unstable))]
criterion_group!(file, sync_read, async_read_file_normal);

criterion_main!(file);
