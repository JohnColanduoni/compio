use std::{io};
use std::thread::{JoinHandle};

pub struct ThreadPool {
}

pub struct Builder {
    thread_count: usize,
    thread_name_prefix: Option<String>,
}

impl ThreadPool {
    pub fn new() -> io::Result<ThreadPool> {
        Builder::new().build()
    }

    pub fn builder() -> Builder {
        Builder::new()
    }
}

impl Builder {
    pub fn new() -> Builder {
        Builder {
            thread_count: num_cpus::get(),
            thread_name_prefix: None,
        }
    }

    pub fn build(self) -> io::Result<ThreadPool> {
        unimplemented!()
    }
}