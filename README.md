# Melba-stt
A rust discord bot that joins a voice channel and transcribes spoken audio from each user.

## Running
1. Install the rust toolchain

### With CUDA and cuDNN (recommended)
2. Install CUDA and cuDNN
3. Run `cargo run --release`

### Cpu only
2. Run `cargo run --release --no-default-features`

Furthermore, you may experience a performance improvement if you compile for the native processor (as simd instructions may be used). This can be activated by using `RUSTFLAGS="-C target-cpu=native" cargo run --release`.

## Configuration
The bot is configured using a `bot.toml` file in the directory that the bot is ran. An example of the values that can go in this file are located in `bot.example.toml`.
Furthermore, the token of the bot is located in `secrets.toml` and has a corresponding example file in `secrets.example.toml`.
By default, the bot will use openai's `tiny` model for whisper, which is equivalent to having the following line in the config file:
```toml
model = { repo = { name = "openai/whisper-tiny" } }
```
If you want to use another model, then the repo can be changed.