use super::{features::FEATURES, IoUring};

#[test]
fn test_create_uring() {
    assert!(FEATURES.io_uring, "io_uring not supported on this machine");
    let _io_uring = IoUring::new().unwrap();
}
