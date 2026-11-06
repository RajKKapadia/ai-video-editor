`speech-check.wav` is the JFK speech sample distributed by whisper.cpp:
https://github.com/ggml-org/whisper.cpp/blob/master/samples/jfk.wav

SHA-256: `59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e`.

The worker uses the first eight seconds to verify model loading and measure transcription speed. It does not retain or upload the sample transcript. This fixed English sample is not an accuracy benchmark or a prediction of performance in every language.
