# Voice

The console's microphone turns speech into a message. Where the browser can do that itself
(Chrome, Edge and Safari, on desktop and phone), it does, and nothing here needs setting up.
Elsewhere (Firefox, and any browser without speech recognition), the console records and kiln
sends the recording to a speech-to-text service of yours, then puts the words in the message field.

## Set it up

Any service that speaks OpenAI's transcription API (`POST /v1/audio/transcriptions`) will do:
whisper.cpp's `whisper-server`, faster-whisper-server, LocalAI, or OpenAI itself.

```toml
[voice]
transcribe_url = "http://192.168.1.20:8080"   # the service (or "https://api.openai.com/v1"); empty: off
model = "whisper-1"                           # what it is asked for (a local server may ignore it)
api_key_file = "/etc/kiln/openai-key"         # a hosted service's key, sent as a bearer token
language = ""                                 # "en", or empty for the service to tell
```

For example, whisper.cpp on a machine of yours:

```sh
whisper-server -m models/ggml-base.en.bin --host 0.0.0.0 --port 8080 --inference-path /v1/audio/transcriptions --convert
```

(`--convert` uses ffmpeg to read the browsers' webm and ogg recordings.)

## A secure page

Browsers use the microphone only on a secure page: `https://`, or `localhost`. At
`http://192.168.1.77:7070` there is no microphone at all, built in or not. Serve the console over
HTTPS (`[server] https`, or a certificate of yours) to use voice from other devices.

## API

| Method | Path | |
| --- | --- | --- |
| GET | `/api/voice` | `{transcribe, language}`: whether kiln transcribes recordings |
| POST | `/api/voice/transcribe?lang=` | the recording as the body (`Content-Type: audio/webm`, `audio/ogg`, `audio/mp4`, `audio/wav`...; up to 25 MiB) → `{text}`. 501: not set up; 415: not a recording; 413: too big; 502: the service failed |
