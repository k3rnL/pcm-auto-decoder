#[test]
fn command_line_exposes_native_single_output_contract() {
    let output = assert_cmd::cargo::cargo_bin_cmd!("pcm-auto-decoder")
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();

    assert!(help.contains("--capture-layout"));
    assert!(help.contains("--output-layout"));
    assert!(help.contains("--capture-file"));
    assert!(help.contains("--output-file"));
    assert!(help.contains("--det-window-ms"));
    assert!(!help.contains("--source"));
    assert!(!help.contains("--sink"));
    assert!(!help.contains("--fifo-out-pcm"));
    assert!(!help.contains("--fifo-out-decoded"));
}
