use std::{
    env, fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn run(port: &str, arguments: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_cdi-serial"))
        .arg("--port")
        .arg(port)
        .args(arguments)
        .status()
        .expect("starting cdi-serial");
    assert!(
        status.success(),
        "cdi-serial {arguments:?} failed with {status}"
    );
}

/// Exercises the full host -> CD-i -> host path and compares every byte.
///
/// This is deliberately ignored: it writes one disposable file to `/nvr` on a
/// real player. Run it only with a running full Stub and an explicit port:
///
/// `CDI_SERIAL_PORT=/dev/ttyUSB0 cargo test --test serial_integration -- --ignored`
#[test]
#[ignore = "requires a real CD-i player and writes a disposable /nvr file"]
fn put_get_delete_round_trip_preserves_bytes() {
    let port = env::var("CDI_SERIAL_PORT").expect("set CDI_SERIAL_PORT, for example /dev/ttyUSB0");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before Unix epoch")
        .as_millis();
    // OS-9 RBF directory names have a 28-byte limit.
    let name = format!("it{nonce:x}.bin");
    let remote_path = format!("/nvr/{name}");
    let local_root = env::temp_dir();
    let source = local_root.join(format!("cdi-serial-{name}"));
    let received = local_root.join(format!("cdi-serial-read-{name}"));

    let mut payload = Vec::with_capacity(1_280);
    payload.extend(0_u8..=255);
    payload.extend((0_u8..=255).rev());
    for index in 0..768_u32 {
        payload.push(((index * 73 + 19) & 0xff) as u8);
    }
    fs::write(&source, &payload).expect("writing integration-test source file");

    let put = ["put", source.to_str().unwrap(), remote_path.as_str()];
    run(&port, &put);

    let get = ["get", remote_path.as_str(), received.to_str().unwrap()];
    run(&port, &get);
    let round_trip = fs::read(&received).expect("reading integration-test output file");
    assert_eq!(round_trip, payload, "CD-i round trip changed file contents");

    let delete = ["delete", remote_path.as_str()];
    run(&port, &delete);
    fs::remove_file(&source).expect("removing integration-test source file");
    fs::remove_file(&received).expect("removing integration-test output file");
}
