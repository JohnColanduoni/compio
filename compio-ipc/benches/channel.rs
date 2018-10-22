#![feature(futures_api, async_await, await_macro, pin)]
#![feature(try_blocks)]

#[macro_use] extern crate criterion;

use std::{thread, io};

use compio_local::LocalExecutor;
use compio_ipc::{Channel, PreChannel};

use criterion::Criterion;
use futures_util::join;

fn bench_channel_msg(c: &mut Criterion) {
    let _ = env_logger::try_init();

    let mut executor = LocalExecutor::new().unwrap();
    let (mut a, mut b) = Channel::pair(&executor.registrar()).unwrap(); 

    c.bench_function("channel_msg", move |bencher| bencher.iter(|| {
        executor.block_on(async {
            let read = async {
                let mut buffer = vec![0u8; 64];
                let byte_count = await!(a.recv(&mut buffer)).unwrap();
                buffer.truncate(byte_count);
                buffer
            };
            let write = async {
                await!(b.send(b"Hello World!")).unwrap();
            };
            let (_write, read) = join!(write, read);

            assert_eq!(b"Hello World!", &*read);
        });
    }));
}

fn bench_channel_echo(c: &mut Criterion) {
    let _ = env_logger::try_init();

    let mut executor = LocalExecutor::new().unwrap();
    let (mut a, mut b) = Channel::pair(&executor.registrar()).unwrap();

    executor.spawn(async move {
        let mut channel = a;
        let result: io::Result<()> = try {
            let mut buffer = vec![0u8; 4096];
            loop {
                let msg_len = await!(channel.recv(&mut buffer))?;
                await!(channel.send(& buffer[..msg_len]))?;
            }
        };
        result.or_else(|err| if err.kind() == io::ErrorKind::BrokenPipe {
            Ok(())
        } else {
            Err(err)
        }).unwrap();
    });
    let mut channel = b;
    c.bench_function("channel_echo", move |bencher| bencher.iter(|| {
        executor.block_on(async {
            let mut buffer = vec![0u8; 1024];
            await!(channel.send(&buffer)).unwrap();
            await!(channel.recv(&mut buffer)).unwrap();
        });
    }));
}

fn bench_channel_echo_cross_thread(c: &mut Criterion) {
    let _ = env_logger::try_init();

    let (a, b) = PreChannel::pair().unwrap();

    let background = thread::spawn(move || {
        let mut executor = LocalExecutor::new().unwrap();
        let mut channel = a.register(&executor.registrar()).unwrap();
        let result: io::Result<()> = executor.block_on(async move {
            let mut buffer = vec![0u8; 4096];
            loop {
                let msg_len = await!(channel.recv(&mut buffer))?;
                await!(channel.send(& buffer[..msg_len]))?;
            }
        });
        result.or_else(|err| if err.kind() == io::ErrorKind::BrokenPipe {
            Ok(())
        } else {
            Err(err)
        }).unwrap();
    });

    let mut executor = LocalExecutor::new().unwrap();
    let mut channel = b.register(&executor.registrar()).unwrap();   
    c.bench_function("channel_echo_cross_thread", move |bencher| bencher.iter(|| {
        executor.block_on(async {
            let mut buffer = vec![0u8; 1024];
            await!(channel.send(&buffer)).unwrap();
            await!(channel.recv(&mut buffer)).unwrap();
        });
    }));

    background.join().unwrap();
}

criterion_group!(benches, bench_channel_msg, bench_channel_echo, bench_channel_echo_cross_thread);
criterion_main!(benches);