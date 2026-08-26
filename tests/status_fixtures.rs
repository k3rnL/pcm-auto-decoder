use serde_json::Value;

#[test]
fn pcm_ac3_eac3_and_dts_status_fixtures_are_complete() {
    let fixtures = include_str!("fixtures/codec_status.ndjson")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(fixtures.len(), 8);
    assert_eq!(fixtures[0]["lifecycle"], "starting");
    assert_eq!(fixtures[1]["mode"], "detecting");
    assert_eq!(fixtures[2]["mode"], "pcm");
    assert!(fixtures[0]["decoded"].is_null());
    assert_eq!(fixtures[3]["codec"], "ac3");
    assert_eq!(fixtures[3]["decoded"]["channels"], 6);
    assert_eq!(fixtures[4]["codec"], "eac3");
    assert_eq!(fixtures[4]["decoded"]["channelLayout"], "stereo");
    assert_eq!(fixtures[5]["codec"], "dts");
    assert_eq!(fixtures[5]["decoded"]["channelLayout"], "7.1");
    assert_eq!(fixtures[6]["errors"][0]["code"], "unsupported_codec");
    assert_eq!(fixtures[7]["lifecycle"], "failed");

    for (index, fixture) in fixtures.iter().enumerate() {
        assert_eq!(fixture["protocolVersion"], 2);
        assert_eq!(fixture["messageType"], "status");
        assert_eq!(fixture["sequence"], (index + 1) as u64);
        assert!(fixture["timestamp"].as_str().unwrap().ends_with('Z'));
        assert!(fixture["transport"]["sampleRate"].is_u64());
        assert!(fixture["confidence"]["score"].is_number());
        assert!(
            fixture["streams"]["captureStreamName"]
                .as_str()
                .unwrap()
                .contains("fixture")
        );
        assert_eq!(fixture["emitted"]["channelLayout"], "7.1");
    }
}
