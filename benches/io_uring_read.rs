#![cfg(all(tokio_unstable, target_os = "linux"))]

use tempfile::NamedTempFile;

use tokio::fs::{read, File};
use tokio::io::AsyncReadExt;

use criterion::{criterion_group, criterion_main, Criterion};

use std::fs::File as StdFile;
use std::io::Read as StdRead;
use std::io::Write;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .build()
        .unwrap()
}

const BLOCK_COUNT: usize = 1_000;

const BUFFER_SIZE: usize = 4096;
const DEV_ZERO: &str = "/dev/zero";

fn temp_file() -> NamedTempFile {
    let mut file = tempfile::Builder::new().tempfile().unwrap();

    let buffer = [0u8; BUFFER_SIZE];

    file.write_all(&buffer).unwrap();

    file
}

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

fn async_read_buf(c: &mut Criterion) {
    let rt = rt();

    c.bench_function("async_read_buf", |b| {
        b.iter(|| {
            let task = || async {
                let mut file = File::open(DEV_ZERO).await.unwrap();
                let mut buffer = [0u8; BUFFER_SIZE];

                for _i in 0..BLOCK_COUNT {
                    let count = file.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                }
            };

            rt.block_on(task());
        });
    });
}

fn async_read_std_file(c: &mut Criterion) {
    let rt = rt();

    c.bench_function("async_read_std_file", |b| {
        b.iter(|| {
            let task = || async {
                let mut file =
                    tokio::task::block_in_place(|| Box::pin(StdFile::open(DEV_ZERO).unwrap()));

                for _i in 0..BLOCK_COUNT {
                    let mut buffer = [0u8; BUFFER_SIZE];
                    let mut file_ref = file.as_mut();

                    tokio::task::block_in_place(move || {
                        file_ref.read_exact(&mut buffer).unwrap();
                    });
                }
            };

            rt.block_on(task());
        });
    });
}

fn sync_read(c: &mut Criterion) {
    c.bench_function("sync_read", |b| {
        b.iter(|| {
            let mut file = StdFile::open(DEV_ZERO).unwrap();
            let mut buffer = [0u8; BUFFER_SIZE];

            for _i in 0..BLOCK_COUNT {
                file.read_exact(&mut buffer).unwrap();
            }
        })
    });
}

criterion_group!(
    file,
    async_read_file_io_uring,
    async_read_buf,
    sync_read,
    async_read_std_file
);
criterion_main!(file);
