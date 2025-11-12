# PAD - pcm-auto-decoder

This is a simple wrapper around ffmpeg with the ability to detect the input audio format (pure PCM or AC3)
and switch automatically between decoding or simple stereo stream.
```
Usage: pcm-auto-decoder [OPTIONS]

Options:
    --source <SOURCE>
        PulseAudio source name (ignored if --stdin is set)
    --sink <SINK>
        PulseAudio sink name (if neither --fifo-out-* set)
        
    --stdin <STDIN>
        Read input from this file/FIFO instead of PulseAudio (expects S16LE 2ch @ 48kHz, may be IEC61937)
    --in-channels <IN_CHANNELS>
        Input channels, should always be 2 as it's the IEC61937 standard [default: 2]
    --in-rate <IN_RATE>
        Input rate, default 48kHz [default: 48000]
    --in-format <IN_FORMAT>
        Input format, default S16LE [default: S16LE]
        
    --fifo-out-pcm <PATH>
        Write stereo PCM (S16LE 2ch @ 48kHz) here in PCM mode
    --out-pcm-channels <OUT_PCM_CHANNELS>
        Desired channels on the PCM output (when no compressed data is detected), default 2 [default: 2]
    --out-pcm-rate <OUT_PCM_RATE>
        Desired rate on the PCM output (when no compressed data is detected), default 48kHz [default: 48000]
    --out-pcm-format <OUT_PCM_FORMAT>
        Desired format on the PCM output (when no compressed data is detected), default S16LE [default: S16LE]
    
    --fifo-out-decoded <PATH>
        Write decoded 5.1 PCM (F32LE 6ch @ 48kHz) here in AC-3 mode
    --out-decoded-channels <OUT_DECODED_CHANNELS>
        Desired channels on decoded output, default 6 [default: 6]
    --out-decoded-rate <OUT_DECODED_RATE>
        Desired rate on decoded output, default 48kHz [default: 48000]
    --out-decoded-format <OUT_DECODED_FORMAT>
        Desired format on decoded output, default F32LE (float32le) [default: F32LE]
        
    --chunk-frames <CHUNK_FRAMES>
        Frames per read [default: 2048]
    --det-window <DET_WINDOW>
        Chunks without IEC-61937 before switching to PCM (and vice-versa) [default: 64]
        
    -h, --help
        Print help
    -V, --version
        Print version```
```

### Useful commands

```bash
# Start pulseaudio with FIFOs, input/output
pulseaudio -D -n -L "module-pipe-source file=/tmp/pa.input rate=48000 format=S16LE channels=2" \
                 -L "module-pipe-sink file=/tmp/pa.output rate=48000 format=float32LE channels=6" \
                 -L "module-native-protocol-unix"
                 
# Start pcm-auto-decoder
pcm-auto-decoder --source fifo_input --sink fifo_output --chunk-frames 256 --det-window 12

# Read a .wav file and push it's data converted in AC3 into the FIFO
ffmpeg -re -i groovy-vibe-427121.wav -ar 48000 -ac 2 -c:a ac3 -b:a 448k -f spdif /tmp/pa.input

# Read the output and save it to .wav (be careful it's a 6 channels f32, the size increases really fast)
parec -d fifo_output.monitor --format=float32le --rate=48000 --channels=6 | ffmpeg -hide_banner -loglevel error     -f f32le -ac 6 -ar 48000 -i -     -af silenceremove=start_periods=1:start_silence=0.1:start_threshold=-50dB     -t 30     -c:a pcm_s16le decoded_5_1.wav
```
pactl load-module module-pipe-source sink_name=pa_input file /tmp/pa.input